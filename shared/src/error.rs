use serde::{Deserialize, Serialize};
use std::fmt;

/// Standardized error codes for the Ravenna system.
/// These codes are used to communicate precise error conditions across service boundaries (Ninference -> Aphex)
/// and allow the frontend to display localized/friendly error messages.
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
    /// restriction policy (embed-bridge `restrictions.target_models`,
    /// docs/licenses/licenses.md §5.3). Permanent for the caller: retrying
    /// the same target on this deployment can never succeed.
    TargetRestricted,

    // --- Upstream/Dependency Errors ---
    UpstreamServiceUnavailable, // e.g., if we were calling OpenAI directly (not current arch, but good for future)
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl ErrorCode {
    /// Returns a string representation compatible with gRPC metadata.
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
        }
    }

    /// Parses a string representation back into an ErrorCode.
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
        ] {
            assert_eq!(ErrorCode::from_str(code.as_str()), Some(code), "{code}");
        }
        assert_eq!(ErrorCode::from_str("NO_SUCH_CODE"), None);
    }
}
