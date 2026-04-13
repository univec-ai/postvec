use serde::{Deserialize, Serialize};
use std::fmt;

/// Wire error codes carried as `x-ravenna-error-code` gRPC metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    // --- General Errors ---
    InternalError,
    InvalidInput,
    Timeout,

    // --- Model Errors ---
    ModelNotFound,
    ModelNotLoaded,
    ModelDisabled,
    ContextLengthExceeded,

    // --- Resource/Hardware Errors ---
    GpuOutOfMemory,
    CpuOverload,

    // --- Bridge/Orchestration Errors ---
    BridgePathNotFound,
    ConverterNotFound,
    /// The requested bridge target is blocked by this deployment's
    /// restriction policy (`embed-bridge` `restrictions.target_models`).
    /// Permanent: retrying the same target on this deployment can never
    /// succeed.
    TargetRestricted,

    // --- Upstream/Dependency Errors ---
    UpstreamServiceUnavailable,
    /// An external embedding provider refused the credential (HTTP 401/403:
    /// bad, revoked or missing key). Distinct from
    /// `UpstreamServiceUnavailable`: an operator problem, not a transient
    /// one. Clients classify it as configuration (bounded retry,
    /// failover-eligible: another node may hold a valid key), never as a
    /// hot-loop transient and never as data poison.
    UpstreamAuthFailed,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl ErrorCode {
    /// The gRPC metadata string.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InternalError => "INTERNAL_ERROR",
            ErrorCode::InvalidInput => "INVALID_INPUT",
            ErrorCode::Timeout => "TIMEOUT",
            ErrorCode::ModelNotFound => "MODEL_NOT_FOUND",
            ErrorCode::ModelNotLoaded => "MODEL_NOT_LOADED",
            ErrorCode::ModelDisabled => "MODEL_DISABLED",
            ErrorCode::ContextLengthExceeded => "CONTEXT_LENGTH_EXCEEDED",
            ErrorCode::GpuOutOfMemory => "GPU_OUT_OF_MEMORY",
            ErrorCode::CpuOverload => "CPU_OVERLOAD",
            ErrorCode::BridgePathNotFound => "BRIDGE_PATH_NOT_FOUND",
            ErrorCode::ConverterNotFound => "CONVERTER_NOT_FOUND",
            ErrorCode::TargetRestricted => "TARGET_RESTRICTED",
            ErrorCode::UpstreamServiceUnavailable => "UPSTREAM_SERVICE_UNAVAILABLE",
            ErrorCode::UpstreamAuthFailed => "UPSTREAM_AUTH_FAILED",
        }
    }

    /// Parse a gRPC metadata string.
    ///
    /// Intentionally not `std::str::FromStr`: that trait must return
    /// `Result<Self, Self::Err>`, whereas an unknown code here is simply
    /// "no such variant" — best modelled as `Option<Self>` with no error type.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "INTERNAL_ERROR" => Some(ErrorCode::InternalError),
            "INVALID_INPUT" => Some(ErrorCode::InvalidInput),
            "TIMEOUT" => Some(ErrorCode::Timeout),
            "MODEL_NOT_FOUND" => Some(ErrorCode::ModelNotFound),
            "MODEL_NOT_LOADED" => Some(ErrorCode::ModelNotLoaded),
            "MODEL_DISABLED" => Some(ErrorCode::ModelDisabled),
            "CONTEXT_LENGTH_EXCEEDED" => Some(ErrorCode::ContextLengthExceeded),
            "GPU_OUT_OF_MEMORY" => Some(ErrorCode::GpuOutOfMemory),
            "CPU_OVERLOAD" => Some(ErrorCode::CpuOverload),
            "BRIDGE_PATH_NOT_FOUND" => Some(ErrorCode::BridgePathNotFound),
            "CONVERTER_NOT_FOUND" => Some(ErrorCode::ConverterNotFound),
            "TARGET_RESTRICTED" => Some(ErrorCode::TargetRestricted),
            "UPSTREAM_SERVICE_UNAVAILABLE" => Some(ErrorCode::UpstreamServiceUnavailable),
            "UPSTREAM_AUTH_FAILED" => Some(ErrorCode::UpstreamAuthFailed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant must survive the gRPC-metadata string round trip —
    /// a variant missing from `from_str` silently degrades to the callers'
    /// unknown-code fallback on the other side of the wire.
    #[test]
    fn every_code_round_trips_through_as_str() {
        for code in [
            ErrorCode::InternalError,
            ErrorCode::InvalidInput,
            ErrorCode::Timeout,
            ErrorCode::ModelNotFound,
            ErrorCode::ModelNotLoaded,
            ErrorCode::ModelDisabled,
            ErrorCode::ContextLengthExceeded,
            ErrorCode::GpuOutOfMemory,
            ErrorCode::CpuOverload,
            ErrorCode::BridgePathNotFound,
            ErrorCode::ConverterNotFound,
            ErrorCode::TargetRestricted,
            ErrorCode::UpstreamServiceUnavailable,
            ErrorCode::UpstreamAuthFailed,
        ] {
            assert_eq!(ErrorCode::from_str(code.as_str()), Some(code), "{code}");
        }
        assert_eq!(ErrorCode::from_str("NO_SUCH_CODE"), None);
    }
}
