//! Transport-agnostic inference client.
//!
//! Everything above the transport (queue, worker, search, migrate) depends
//! only on [`InferenceClient`]. gRPC mode implements it over gRPC/HTTP;
//! embedded mode talks to the in-process engine behind the same trait.

pub mod discovery;
#[cfg(feature = "embedded")]
pub mod embedded;
pub mod grpc;

use std::fmt;

/// ninference error vocabulary, carried as `x-ravenna-error-code` gRPC
/// metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RavennaCode {
    InternalError,
    InvalidInput,
    Timeout,
    ModelNotFound,
    ModelNotLoaded,
    ModelDisabled,
    ContextLengthExceeded,
    GpuOutOfMemory,
    CpuOverload,
    BridgePathNotFound,
    ConverterNotFound,
    /// The bridge target is blocked by the deployment's restriction policy
    /// (embed-bridge `restrictions.target_models`) — permanent: retrying the
    /// same target against this deployment can never succeed.
    TargetRestricted,
    UpstreamServiceUnavailable,
    /// An external embedding provider refused the credential (HTTP 401/403).
    /// An operator problem, not a transient one: bounded retry with backoff,
    /// failover-eligible (another node may hold a valid key), never a
    /// hot-loop and never dead-letters data for an ops problem.
    UpstreamAuthFailed,
    /// A code we don't recognize (forward compatibility).
    Unknown,
}

impl RavennaCode {
    pub fn parse(s: &str) -> Self {
        match s {
            "INTERNAL_ERROR" | "InternalError" => Self::InternalError,
            "INVALID_INPUT" | "InvalidInput" => Self::InvalidInput,
            "TIMEOUT" | "Timeout" => Self::Timeout,
            "MODEL_NOT_FOUND" | "ModelNotFound" => Self::ModelNotFound,
            "MODEL_NOT_LOADED" | "ModelNotLoaded" => Self::ModelNotLoaded,
            "MODEL_DISABLED" | "ModelDisabled" => Self::ModelDisabled,
            "CONTEXT_LENGTH_EXCEEDED" | "ContextLengthExceeded" => Self::ContextLengthExceeded,
            "GPU_OUT_OF_MEMORY" | "GpuOutOfMemory" => Self::GpuOutOfMemory,
            "CPU_OVERLOAD" | "CpuOverload" => Self::CpuOverload,
            "BRIDGE_PATH_NOT_FOUND" | "BridgePathNotFound" => Self::BridgePathNotFound,
            "CONVERTER_NOT_FOUND" | "ConverterNotFound" => Self::ConverterNotFound,
            "TARGET_RESTRICTED" | "TargetRestricted" => Self::TargetRestricted,
            "UPSTREAM_SERVICE_UNAVAILABLE" | "UpstreamServiceUnavailable" => {
                Self::UpstreamServiceUnavailable
            }
            "UPSTREAM_AUTH_FAILED" | "UpstreamAuthFailed" => Self::UpstreamAuthFailed,
            _ => Self::Unknown,
        }
    }
}

/// Worker retry policy classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Retry with exponential backoff.
    Transient,
    /// Retry (model may JIT-restore); surface in status(); dead after max_retries.
    Config,
    /// Bisect the batch; the offending row goes to jobs_dead.
    PoisonRow,
    /// Straight to jobs_dead / fail the migration.
    Permanent,
}

#[derive(Debug, thiserror::Error)]
pub enum PvError {
    #[error("no ninference endpoints configured (set postvec.ninference_grpc_endpoints / postvec.ninference_http_endpoints)")]
    NoEndpoints,
    #[error("transport error talking to ninference at {endpoint}: {message}")]
    Transport { endpoint: String, message: String },
    #[error("deadline exceeded after {ms} ms")]
    Deadline { ms: u64 },
    #[error("ninference error {code:?}: {message}")]
    Remote { code: RavennaCode, message: String },
    #[error("bad response from ninference: {0}")]
    Decode(String),
    #[error("model {0:?} not known to postvec (try postvec.refresh_models())")]
    UnknownModel(String),
    #[error("model {model:?} is not embeddable: {detail} (try postvec.refresh_models())")]
    NoEmbedPath { model: String, detail: String },
    // (fields can't be named `source`/`target`: thiserror treats a `source`
    // field as the wrapped error)
    #[error("no convert path from {from:?} to {to:?}")]
    NoConvertPath { from: String, to: String },
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("{0}")]
    Internal(String),
}

impl PvError {
    /// Classify for the worker's retry policy.
    pub fn class(&self) -> ErrorClass {
        match self {
            PvError::NoEndpoints => ErrorClass::Config,
            PvError::Transport { .. } | PvError::Deadline { .. } => ErrorClass::Transient,
            PvError::Remote { code, .. } => match code {
                RavennaCode::Timeout
                | RavennaCode::CpuOverload
                | RavennaCode::GpuOutOfMemory
                | RavennaCode::UpstreamServiceUnavailable
                | RavennaCode::InternalError
                | RavennaCode::Unknown => ErrorClass::Transient,
                // Bridge inventory errors mean "the complete chain is not
                // loaded on this node right now" — deployment skew or a model
                // load/unload window, not proof the request can never succeed
                // (the genuinely-absent-route case is caught locally as
                // NoEmbedPath before any RPC). Failover tries other nodes
                // first; after that: bounded queue retry, indefinite
                // migration retry — same policy as a model mid-JIT-restore.
                RavennaCode::ModelNotFound
                | RavennaCode::ModelNotLoaded
                | RavennaCode::ModelDisabled
                | RavennaCode::BridgePathNotFound
                | RavennaCode::ConverterNotFound => ErrorClass::Config,
                // A provider 401/403 is an ops problem (bad or revoked key):
                // Config keeps it on bounded backoff and failover — another
                // node may hold a valid key — without dead-lettering rows or
                // hot-looping while the operator rotates the credential.
                RavennaCode::UpstreamAuthFailed => ErrorClass::Config,
                RavennaCode::ContextLengthExceeded => ErrorClass::PoisonRow,
                RavennaCode::InvalidInput | RavennaCode::TargetRestricted => ErrorClass::Permanent,
            },
            PvError::Decode(_) | PvError::InvalidInput(_) => ErrorClass::Permanent,
            PvError::UnknownModel(_)
            | PvError::NoEmbedPath { .. }
            | PvError::NoConvertPath { .. } => ErrorClass::Permanent,
            PvError::Internal(_) => ErrorClass::Permanent,
        }
    }
}

/// One model as seen in ninference `GET /config`.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Internal ninference name (what goes into the gRPC `model` field).
    pub name: String,
    /// "embed" | "convert" | "embed-bridge" | "convert-bridge" | "legacy"
    pub model_type: String,
    pub source_model: Option<String>,
    pub target_model: Option<String>,
    pub source_dim: Option<u32>,
    pub target_dim: Option<u32>,
    pub sequence_len: Option<u32>,
    /// Full HubModel snapshot for postvec.models.raw.
    pub raw: serde_json::Value,
}

impl fmt::Display for ModelInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.model_type)
    }
}

/// What the texts of an embed call are *for*. Quality-relevant to providers
/// that distinguish the two (Cohere's `input_type`; Gemini's `taskType` is
/// deferred), and inert for every other route.
///
/// Carried on [`EmbedRoute`] rather than as a new trait method or proto
/// field: the wire already has `EmbedTextsRequest.input_type`, so this
/// changes no contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmbedPurpose {
    /// Stored content: the worker, one-shot `embed()`, migrate reembed, and
    /// the dimension probe.
    #[default]
    Document,
    /// A `search()` query being embedded to compare against stored vectors.
    Query,
}

impl EmbedPurpose {
    /// The `EmbedTextsRequest.input_type` value, using the vocabulary the
    /// providers already speak.
    pub fn as_wire(self) -> &'static str {
        match self {
            EmbedPurpose::Document => "search_document",
            EmbedPurpose::Query => "search_query",
        }
    }
}

/// Bridge fields for an embed call routed through an `embed-bridge` executor
/// (texts are embedded with `bridge_model`, then converted into
/// `target_model`'s space engine-side); both `None` for a direct embed model.
/// Same wire shape aphex uses for `/v1/embed-bridge`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedRoute {
    pub bridge_model: Option<String>,
    pub target_model: Option<String>,
    /// Query vs document. Defaults to `Document`, so every existing
    /// construction site keeps today's meaning.
    pub purpose: EmbedPurpose,
}

impl EmbedRoute {
    /// The same route, embedding for `purpose`.
    pub fn with_purpose(mut self, purpose: EmbedPurpose) -> Self {
        self.purpose = purpose;
        self
    }
}

/// Transport-agnostic core trait. Async; callers run it on the per-process
/// current-thread runtime via `runtime::block_on_with_timeout`.
pub trait InferenceClient: Send + Sync {
    fn embed(
        &self,
        texts: &[String],
        model: &str,
        route: &EmbedRoute,
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, PvError>> + Send;

    /// Direct conversion under `model`'s own name. postvec plans no other
    /// kind: the two-hop path through the `convert-bridge` executor was
    /// removed ahead of that executor's deprecation, so the wire's bridge
    /// fields are always sent empty.
    fn convert(
        &self,
        vecs: &[Vec<f32>],
        model: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Vec<f32>>, PvError>> + Send;

    fn list_models(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ModelInfo>, PvError>> + Send;
}

// There is deliberately no mode-switching client wrapper: every actor that
// talks to inference — worker, backend, launcher-mode per-DB worker — uses
// `GrpcClient::from_gucs`, which resolves the transport per mode (mesh
// endpoints in gRPC mode, the launcher-hosted loopback listeners in embedded
// mode). The engine host itself never drains, so no in-process client exists.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_parse_roundtrip() {
        assert_eq!(RavennaCode::parse("Timeout"), RavennaCode::Timeout);
        assert_eq!(RavennaCode::parse("TIMEOUT"), RavennaCode::Timeout);
        assert_eq!(
            RavennaCode::parse("ContextLengthExceeded"),
            RavennaCode::ContextLengthExceeded
        );
        assert_eq!(
            RavennaCode::parse("CONTEXT_LENGTH_EXCEEDED"),
            RavennaCode::ContextLengthExceeded
        );
        assert_eq!(
            RavennaCode::parse("BRIDGE_PATH_NOT_FOUND"),
            RavennaCode::BridgePathNotFound
        );
        assert_eq!(
            RavennaCode::parse("CONVERTER_NOT_FOUND"),
            RavennaCode::ConverterNotFound
        );
        assert_eq!(
            RavennaCode::parse("TARGET_RESTRICTED"),
            RavennaCode::TargetRestricted
        );
        assert_eq!(
            RavennaCode::parse("TargetRestricted"),
            RavennaCode::TargetRestricted
        );
        assert_eq!(
            RavennaCode::parse("MODEL_NOT_FOUND"),
            RavennaCode::ModelNotFound
        );
        assert_eq!(
            RavennaCode::parse("UPSTREAM_AUTH_FAILED"),
            RavennaCode::UpstreamAuthFailed
        );
        assert_eq!(
            RavennaCode::parse("UpstreamAuthFailed"),
            RavennaCode::UpstreamAuthFailed
        );
        assert_eq!(
            RavennaCode::parse("INVALID_INPUT"),
            RavennaCode::InvalidInput
        );
        assert_eq!(RavennaCode::parse("SomethingNew"), RavennaCode::Unknown);
    }

    /// The default must be Document: every construction site that predates
    /// the purpose keeps meaning exactly what it meant.
    #[test]
    fn embed_purpose_defaults_to_document_and_maps_to_the_wire() {
        assert_eq!(EmbedPurpose::default(), EmbedPurpose::Document);
        assert_eq!(EmbedRoute::default().purpose, EmbedPurpose::Document);
        assert_eq!(EmbedPurpose::Document.as_wire(), "search_document");
        assert_eq!(EmbedPurpose::Query.as_wire(), "search_query");

        // `with_purpose` changes only the purpose.
        let bridged = EmbedRoute {
            bridge_model: Some("m".into()),
            target_model: Some("ext".into()),
            ..Default::default()
        };
        let query = bridged.clone().with_purpose(EmbedPurpose::Query);
        assert_eq!(query.bridge_model, bridged.bridge_model);
        assert_eq!(query.target_model, bridged.target_model);
        assert_eq!(query.purpose, EmbedPurpose::Query);
    }

    #[test]
    fn error_classification_matches_plan_taxonomy() {
        let remote = |code| PvError::Remote {
            code,
            message: String::new(),
        };
        // transient
        for code in [
            RavennaCode::Timeout,
            RavennaCode::CpuOverload,
            RavennaCode::GpuOutOfMemory,
            RavennaCode::UpstreamServiceUnavailable,
        ] {
            assert_eq!(remote(code).class(), ErrorClass::Transient, "{code:?}");
        }
        assert_eq!(
            PvError::Transport {
                endpoint: "x".into(),
                message: "y".into()
            }
            .class(),
            ErrorClass::Transient
        );
        // config — includes the bridge inventory codes: an incomplete chain
        // on the serving node is retryable state (rollout skew, load/unload
        // window), not a permanently invalid request.
        for code in [
            RavennaCode::ModelNotFound,
            RavennaCode::ModelNotLoaded,
            RavennaCode::ModelDisabled,
            RavennaCode::BridgePathNotFound,
            RavennaCode::ConverterNotFound,
            // A revoked provider key is an ops problem: bounded retry +
            // failover, never Transient (hot loop) and never Permanent
            // (dead-lettering data for a credential rotation).
            RavennaCode::UpstreamAuthFailed,
        ] {
            assert_eq!(remote(code).class(), ErrorClass::Config, "{code:?}");
        }
        // poison
        assert_eq!(
            remote(RavennaCode::ContextLengthExceeded).class(),
            ErrorClass::PoisonRow
        );
        // permanent — TargetRestricted stays permanent: it is a deliberate
        // policy refusal, not missing inventory.
        for code in [RavennaCode::InvalidInput, RavennaCode::TargetRestricted] {
            assert_eq!(remote(code).class(), ErrorClass::Permanent, "{code:?}");
        }
        assert_eq!(
            PvError::Decode("row count mismatch".into()).class(),
            ErrorClass::Permanent
        );
    }
}
