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

/// Ceiling on provider response-body bytes copied into an error message.
/// Bodies can be arbitrarily large and these messages travel into host logs
/// (and, mapped to wire codes, toward PostgreSQL) — bound them at creation.
const ERROR_BODY_PREVIEW_BYTES: usize = 500;

/// Truncate a provider response body for inclusion in an error message,
/// respecting UTF-8 char boundaries.
pub(crate) fn body_preview(body: &str) -> String {
    if body.len() <= ERROR_BODY_PREVIEW_BYTES {
        return body.to_string();
    }
    let mut end = ERROR_BODY_PREVIEW_BYTES;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &body[..end], body.len())
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
    fn body_preview_truncates_on_char_boundaries() {
        let short = "short body";
        assert_eq!(body_preview(short), short);

        // 600 multi-byte chars: the cut must land on a boundary and note the size.
        let long: String = "é".repeat(600);
        let preview = body_preview(&long);
        assert!(preview.len() < long.len());
        assert!(preview.contains("1200 bytes total"), "{preview}");
    }
}
