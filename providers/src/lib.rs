//!
//! providers/src/lib.rs
//!
//! This crate provides a unified abstraction layer for generating text embeddings
//! from various providers like OpenAI, Google Gemini, Cohere, and AWS Titan.
//!
//! It exposes a single public trait, `EmbeddingBackend`, which all provider-specific
//! clients implement. It also provides a factory function, `new_embedding_backend`,
//! to easily instantiate clients based on configuration. This design allows the
//! consuming application to interact with any provider through a common interface.
//!

// Declare the private module for AWS Signature V4 logic.
// It is not part of the public API.
mod aws_sigv4;

// Declare the internal module for retry logic.
// This is not part of the public API.
mod retry;

// Declare and publicly export the provider client modules and the factory.
pub mod cohere;
pub mod factory;
pub mod gemini;
pub mod mistral;
pub mod openai;
pub mod openrouter;
pub mod titan;

// The built-in model catalog: CLI descriptor prefill and docs generation
// only — the serving hosts never consult it.
pub mod catalog;

// The providers.d config layer (feature `config`) and the gateway both
// inference hosts mount (feature `wire` — that one also needs the fork's
// `shared` error codes). The split lets the CLI validate a provider file
// against the loader's own schema without linking the gateway.
#[cfg(feature = "config")]
pub mod config;
#[cfg(feature = "wire")]
pub mod gateway;

/// The in-process mock HTTP server shared by this crate's integration tests
/// and by the inference hosts' test suites (feature `test-util`).
#[cfg(any(test, feature = "test-util"))]
pub mod testing;

// Publicly export the client structs and the factory function for easy access.
pub use cohere::CohereClient;
pub use factory::{new_embedding_backend, ProviderConfig};
pub use gemini::GeminiClient;
pub use mistral::MistralClient;
pub use openai::OpenAIClient;
pub use openrouter::OpenRouterClient;
pub use titan::TitanClient;

use async_trait::async_trait;
use thiserror::Error;

/// AWS regions become part of a hostname verbatim
/// (`bedrock-runtime.{region}.amazonaws.com`, see `titan.rs`) and part of
/// the SigV4 credential scope. A value carrying a dot or a slash would
/// therefore redirect signed, credential-bearing requests at an arbitrary
/// host. Real region names are `[a-z0-9-]`, so refuse anything else — at
/// providers.d load time and at `postvec provider add` — rather than letting
/// a typo (or a tampered file) choose the endpoint.
///
/// Lives here rather than in `config` so the CLI, which builds this crate
/// without the `wire` feature, enforces the same rule before writing a file.
pub fn validate_region(region: &str) -> Result<(), String> {
    if region.is_empty() || region.len() > 32 {
        return Err("region must be 1..=32 characters".to_string());
    }
    if !region
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
    {
        return Err(format!(
            "region {region:?} contains characters outside [a-z0-9-]; it is interpolated into \
             the Bedrock hostname and the SigV4 credential scope"
        ));
    }
    Ok(())
}

/// How many bytes of a **non-2xx** body are read at all. The preview is 500;
/// reading more than this to throw it away only gives a hostile or broken
/// endpoint a free allocation, once per attempt and once per retry.
const ERROR_BODY_READ_BYTES: usize = 8 * 1024;

/// Floor and ceiling on the computed success-body budget. The floor keeps a
/// one-vector response comfortable; the ceiling is the absolute limit no
/// legitimate embedding response approaches — the largest shape any supported
/// descriptor can ask for (512 inputs × 3072 components) renders to roughly
/// 30 MB of JSON.
const MIN_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// Bytes of JSON a single float component is allowed to occupy. Real values
/// render to 10–20; 24 leaves room for exponent forms without inflating the
/// budget into uselessness.
const BYTES_PER_COMPONENT: usize = 24;

/// The success-body budget for one request: what a *legitimate* response to
/// this call could plausibly weigh.
///
/// A provider is a third party reached over the internet, sometimes through
/// an operator-supplied `base_url`. Nothing in HTTP obliges it to send a body
/// the size it promised — `Content-Length` may be absent or the transfer
/// chunked — so a broken, compromised or merely misconfigured endpoint can
/// stream until the reader gives up. `reqwest`'s `bytes()`/`text()` give up
/// at OOM. In embedded mode that is the PostgreSQL launcher's RSS, which is
/// the one process whose death restarts the cluster.
///
/// `declared_dim` is the descriptor's dimension where the connector knows it;
/// the fallback covers the connectors that do not carry one (their responses
/// are still bounded by the input count and the ceiling).
pub(crate) fn response_budget(texts: usize, declared_dim: Option<usize>) -> usize {
    let dim = declared_dim.filter(|d| *d > 0).unwrap_or(4096);
    texts
        .saturating_mul(dim)
        .saturating_mul(BYTES_PER_COMPONENT)
        .saturating_add(64 * 1024)
        .clamp(MIN_RESPONSE_BYTES, MAX_RESPONSE_BYTES)
}

/// Read a response body, refusing to allocate more than `limit` bytes.
///
/// The one body reader every connector uses, for both success and error
/// bodies. Six subtly different limits is the failure mode this exists to
/// prevent.
///
/// Classification matters as much as the limit. A body that *exceeds* the
/// budget is not an outage — retrying it re-runs the same allocation against
/// the same broken peer — so it comes back as the same `Api { status: 200 }`
/// sentinel a body that will not parse uses, which the gateway maps to
/// `InvalidInput` (Permanent). A body that fails mid-read is transport
/// trouble and stays `Network` (Transient), exactly as before.
pub(crate) async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, EmbeddingError> {
    // A declared length over the budget is refused before a byte is read;
    // an absent or lying one is caught by the running total below.
    if let Some(declared) = response.content_length() {
        if declared > limit as u64 {
            return Err(oversized(declared as usize, limit));
        }
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(64 * 1024) as usize);
    while let Some(chunk) = response.chunk().await.map_err(EmbeddingError::Network)? {
        if body.len() + chunk.len() > limit {
            return Err(oversized(body.len() + chunk.len(), limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn oversized(seen: usize, limit: usize) -> EmbeddingError {
    EmbeddingError::Api {
        status: 200,
        message: format!(
            "response body exceeds the {limit}-byte budget for this request (at least {seen} \
             bytes); refusing to buffer it"
        ),
    }
}

/// Decode a body that has already been read off a 2xx response.
///
/// Every client reads `response.bytes()` first and then calls this, instead
/// of `response.json()`. The split is a classification decision, not a style
/// one: reading the body is transport work (a reset connection or a
/// truncated body must stay `Network`, and therefore transient), while a
/// body that is not the shape the provider documents is permanent for this
/// response. `reqwest` folds both into one `Decode` error kind
/// (`async_impl/decoder.rs`, the `PlainText` arm), so a caller that used
/// `response.json()` could not tell an outage from garbage — and the
/// gateway would classify a dropped connection as a dead-letter.
pub(crate) fn decode_json<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
) -> Result<T, EmbeddingError> {
    serde_json::from_slice(bytes).map_err(|e| EmbeddingError::Api {
        status: 200,
        // Position and classification, never content. A 2xx body that will
        // not parse is the most likely place to find the *source text* echoed
        // back, and this message is logged and stored durably in
        // `postvec.jobs_dead.last_error`. Line and column locate the problem
        // as precisely as a quotation would.
        message: format!(
            "response body is not the documented shape: {:?} error at line {}, column {} of \
             {} bytes",
            e.classify(),
            e.line(),
            e.column(),
            bytes.len()
        ),
    })
}

/// Build the `Api` error for a non-2xx response: read a bounded slice of the
/// body, strip anything that could forge a log line, and remove the
/// credential if the peer echoed it.
///
/// Every connector routes its non-2xx path through this. The three rules it
/// enforces exist because this message does not stay in the process: it is
/// logged by both hosts, crosses tonic as a status, and can be stored
/// durably in `postvec.jobs_dead.last_error`.
///
/// - **401/403 bodies are never included.** An authentication failure is the
///   one response most likely to quote the credential back, and the status
///   alone is the whole diagnosis.
/// - **No control characters can survive**, because no free text does: an
///   upstream must not be able to write a second, forged line into a
///   PostgreSQL log.
/// - **Nothing from the body is forwarded but a code this crate already
///   knows.** [`provider_error_code`] matches against a closed list, so
///   neither an echoed credential nor an echoed fragment of the row's own
///   source text can reach a log, a tonic status or
///   `postvec.jobs_dead.last_error` — not because it is unlikely to fit a
///   grammar, but because the value has to be one of ours.
pub(crate) async fn api_error(response: reqwest::Response) -> EmbeddingError {
    let status = response.status().as_u16();
    if matches!(status, 401 | 403) {
        return EmbeddingError::Api {
            status,
            message: "authentication rejected by the provider (response body withheld)".to_string(),
        };
    }
    let body = match read_bounded(response, ERROR_BODY_READ_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return EmbeddingError::Api {
                status,
                message: "the response body could not be read".to_string(),
            }
        }
    };
    let body = String::from_utf8_lossy(&body);

    // Classify here, while the body is in hand. The gateway used to sniff the
    // *message* for token-limit wording, which is the only reason the body had
    // to survive this far.
    if matches!(status, 400 | 413 | 422) && looks_like_context_length(&body) {
        return EmbeddingError::InputTooLong { status };
    }
    EmbeddingError::Api {
        status,
        message: match provider_error_code(&body) {
            Some(code) => format!("provider error code {code:?}"),
            None => "no recognised provider error code in the response".to_string(),
        },
    }
}

/// The error codes this crate is willing to repeat back.
///
/// A **closed** vocabulary, not a grammar. The previous version forwarded any
/// short `[A-Za-z0-9_.-]` value found in a provider's error field, which is
/// still upstream-controlled: a provider — or anything behind an
/// operator-supplied `base_url` — can put a spaceless fragment of the row's
/// own source text in `error.code` and it would have travelled to the host's
/// log, across tonic, and into `postvec.jobs_dead.last_error`.
///
/// Matching against a list *we* wrote makes the leak impossible rather than
/// unlikely. The cost is bounded and visible: an unrecognised code is
/// reported as absent, so a new provider code means slightly less detail in
/// one message — never a disclosure. Add to this list when a provider
/// documents a code worth distinguishing.
const KNOWN_PROVIDER_ERROR_CODES: &[&str] = &[
    // OpenAI / OpenRouter / Azure-compatible
    "insufficient_quota",
    "invalid_api_key",
    "invalid_organization",
    "invalid_request_error",
    "model_not_found",
    "rate_limit_exceeded",
    "server_error",
    "tokens_exceeded",
    "context_length_exceeded",
    "billing_hard_limit_reached",
    "unsupported_value",
    "invalid_value",
    // Cohere
    "invalid_argument",
    "not_found",
    "too_many_requests",
    "unavailable",
    // Google
    "INVALID_ARGUMENT",
    "PERMISSION_DENIED",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "UNAVAILABLE",
    "INTERNAL",
    // AWS Bedrock
    "ValidationException",
    "ThrottlingException",
    "ModelTimeoutException",
    "ModelErrorException",
    "ModelNotReadyException",
    "ServiceQuotaExceededException",
    "AccessDeniedException",
];

/// The one thing extracted from an upstream error body: a code this crate
/// already knows, found in one of the fields providers use for exactly that.
///
/// The body itself never leaves this function, and neither does any value
/// that is not in [`KNOWN_PROVIDER_ERROR_CODES`].
fn provider_error_code(body: &str) -> Option<&'static str> {
    let parsed: serde_json::Value = serde_json::from_str(body).ok()?;
    [
        "/error/code",
        "/error/type",
        "/error/status",
        "/code",
        "/status",
        "/message",
        "/__type",
    ]
    .into_iter()
    .filter_map(|pointer| parsed.pointer(pointer))
    .filter_map(|value| value.as_str())
    .find_map(|candidate| {
        KNOWN_PROVIDER_ERROR_CODES
            .iter()
            .find(|known| **known == candidate)
            .copied()
    })
}

/// Provider-specific "input too long" detection over a 4xx body. Heuristic on
/// purpose: OpenAI says "maximum context length … tokens", Cohere "total
/// number of tokens … exceeds", Mistral "too many tokens". 401/403/429 never
/// reach it — [`api_error`] returns before this for the first two, and the
/// status match excludes the third.
fn looks_like_context_length(body: &str) -> bool {
    let body = body.to_lowercase();
    body.contains("token")
        && (body.contains("exceed")
            || body.contains("too long")
            || body.contains("too many")
            || body.contains("maximum")
            || body.contains("max_tokens")
            || body.contains("context length"))
}

/// Represents a single successfully generated embedding vector.
///
/// This struct is the successful result of an embedding operation for a single text.
#[derive(Debug, Clone)]
pub struct Embedding {
    /// The index of the text in the original input slice that this vector corresponds to.
    /// This is crucial for mapping results back to their original inputs, especially
    /// when processing batches.
    pub text_index: usize,
    /// The embedding vector, represented as a vector of 32-bit floats.
    pub vector: Vec<f32>,
}

/// Represents all possible errors that can occur during the embedding process.
///
/// This enum uses `thiserror` to provide structured, informative errors, allowing
/// the calling application to handle different failure modes gracefully.
#[derive(Error, Debug)]
pub enum EmbeddingError {
    /// A network-level error occurred while making the HTTP request.
    /// This wraps the underlying `reqwest::Error`.
    #[error("Network request failed: {0}")]
    Network(#[from] reqwest::Error),

    /// The API provider returned a non-successful HTTP status code.
    /// This includes the status code and a bounded preview of the response
    /// body for debugging (never the request URL or credentials).
    #[error("API request failed with status {status}: {message}")]
    Api { status: u16, message: String },

    /// An error occurred while deserializing the API's JSON response.
    /// This typically indicates an unexpected response format from the provider.
    #[error("Failed to deserialize API response: {0}")]
    Deserialization(#[from] serde_json::Error),

    /// The client was configured with invalid parameters or a required
    /// credential/setting was missing.
    #[error("Invalid configuration: {0}")]
    Configuration(String),

    /// An authentication-related error occurred, typically a missing API key.
    #[error("Authentication error: {0}")]
    Authentication(String),

    /// The provider rejected the request because an input was too long.
    ///
    /// A distinct variant rather than a marker inside [`EmbeddingError::Api`]'s
    /// message, because it is the one classification that used to be recovered
    /// by *re-reading the response body* one layer up. Deciding it here — where
    /// the body is, and before the body is discarded — is what lets the public
    /// error carry no upstream text at all.
    #[error("Provider rejected an input as too long (status {status})")]
    InputTooLong { status: u16 },

    /// The caller's deadline was exhausted before the operation (including
    /// any in-client retries) could complete.
    #[error("Deadline exhausted: {0}")]
    Deadline(String),
}

/// The core trait defining the functionality of an embedding backend client.
///
/// All provider-specific clients implement this trait, providing a single,
/// unified `embed` method.
/// The `Send + Sync` bounds are essential to allow
/// the trait object to be safely shared across threads in a multi-threaded
/// asynchronous runtime.
#[async_trait]
pub trait EmbeddingBackend: Send + Sync {
    /// Generates embeddings for a batch of texts.
    ///
    /// The method takes a slice of string slices and returns a `Result`.
    /// On success,
    /// it contains a vector of `Embedding` structs.
    /// On failure, it returns an
    /// `EmbeddingError`. The operation is treated as transactional;
    /// if the API
    /// returns an error for the batch, the entire operation fails.
    ///
    /// # Arguments
    /// * `texts` - A slice of strings to be embedded.
    /// * `deadline` - Absolute point past which no further attempt (or retry)
    ///   may start. `None` means "no caller deadline" — the in-client retry
    ///   budget alone bounds the call.
    ///
    /// # Returns
    /// A `Result` containing a vector of `Embedding` structs on success,
    /// or an `EmbeddingError` on failure.
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages_are_actionable() {
        // The `Display` strings surface in host logs, so pin their shape.
        let api = EmbeddingError::Api {
            status: 429,
            message: "rate limited".into(),
        };
        assert_eq!(
            api.to_string(),
            "API request failed with status 429: rate limited"
        );

        let cfg = EmbeddingError::Configuration("missing region".into());
        assert_eq!(cfg.to_string(), "Invalid configuration: missing region");

        let auth = EmbeddingError::Authentication("no key".into());
        assert_eq!(auth.to_string(), "Authentication error: no key");

        let deadline = EmbeddingError::Deadline("2 retries left unused".into());
        assert_eq!(
            deadline.to_string(),
            "Deadline exhausted: 2 retries left unused"
        );
    }

    #[test]
    fn embedding_struct_holds_index_and_vector() {
        let e = Embedding {
            text_index: 3,
            vector: vec![1.0, 2.0],
        };
        let cloned = e.clone();
        assert_eq!(cloned.text_index, 3);
        assert_eq!(cloned.vector, vec![1.0, 2.0]);
    }

    #[test]
    fn a_region_that_could_redirect_the_endpoint_is_refused() {
        assert!(validate_region("us-east-1").is_ok());
        assert!(validate_region("eu-central-1").is_ok());
        for bad in [
            "us-east-1.evil.example",
            "us east 1",
            "../x",
            "",
            "US-EAST-1",
        ] {
            assert!(validate_region(bad).is_err(), "{bad:?}");
        }
    }

    /// A legitimate response is comfortably inside the budget; a hostile one
    /// cannot make the host allocate without bound. The largest shape any
    /// supported descriptor can request is 512 inputs × 3072 components.
    #[test]
    fn the_response_budget_fits_real_answers_and_bounds_hostile_ones() {
        // One small vector still gets a workable floor.
        assert_eq!(response_budget(1, Some(1024)), MIN_RESPONSE_BYTES);
        // The largest legitimate OpenAI batch: ~30 MB of JSON, budget above it.
        let big = response_budget(512, Some(3072));
        assert!(big > 512 * 3072 * 12, "must fit a real answer: {big}");
        assert!(big <= MAX_RESPONSE_BYTES);
        // No declared dimension still bounds by input count.
        assert!(response_budget(96, None) < MAX_RESPONSE_BYTES);
        // Absurd inputs saturate at the ceiling rather than overflowing.
        assert_eq!(
            response_budget(usize::MAX, Some(usize::MAX)),
            MAX_RESPONSE_BYTES
        );
    }
}
