//! Engine errors: models, I/O, JSON, tokenizers and prediction failures in one type.
use crate::tokenizers::error::TokenizerError;
use ndarray::ShapeError;
use shared::vectors::VectorError;
use thiserror::Error;

/// The primary error type for the `engine` crate.
#[derive(Error, Debug)]
pub enum EngineError {
    /// Inner model error, shown without a "Model error:" prefix.
    #[error(transparent)]
    Model(#[from] crate::models::ModelError),

    /// Invalid or missing configuration.
    #[error("{0}")]
    Configuration(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// Model, executor or other named resource is missing.
    #[error("{0}")]
    NotFound(String),

    /// Inference failed.
    #[error("{0}")]
    Prediction(String),

    /// A bridge could not resolve its complete model chain on this engine
    /// node. Callers may try another node during fleet rollouts.
    #[error("{0}")]
    BridgePathNotFound(String),

    /// A bridge chain's required converter is absent on this engine node.
    #[error("{0}")]
    ConverterNotFound(String),

    /// The requested bridge target is blocked by this deployment's
    /// restriction policy (embed-bridge `restrictions.target_models`).
    /// A distinct variant (not `Prediction`) so it crosses gRPC as
    /// `TARGET_RESTRICTED` and callers can fail fast instead of retrying.
    #[error("{0}")]
    TargetRestricted(String),

    /// Input value could not be converted to the type the executor asked for.
    #[error("{0}")]
    InputTypeError(String),

    /// The caller's deadline elapsed before or during execution — admission
    /// wait, pool wait, or the execution itself. A distinct variant (not
    /// `Prediction`) so it crosses gRPC as `TIMEOUT`/DeadlineExceeded and
    /// callers can distinguish "the engine is broken" from "my budget ran
    /// out".
    #[error("{0}")]
    Timeout(String),

    /// A catch-all for other types of errors, using `anyhow`.
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error(transparent)]
    Tokenizer(#[from] TokenizerError),

    #[error(transparent)]
    Vector(#[from] VectorError),

    #[error(transparent)]
    Shape(#[from] ShapeError),
}

impl EngineError {
    pub fn to_error_code(&self) -> shared::ErrorCode {
        use shared::ErrorCode;
        match self {
            EngineError::NotFound(_) => ErrorCode::ModelNotFound,
            EngineError::TargetRestricted(_) => ErrorCode::TargetRestricted,
            EngineError::BridgePathNotFound(_) => ErrorCode::BridgePathNotFound,
            EngineError::ConverterNotFound(_) => ErrorCode::ConverterNotFound,
            EngineError::InputTypeError(_) => ErrorCode::InvalidInput,
            EngineError::Timeout(_) => ErrorCode::Timeout,
            EngineError::Io(_) => ErrorCode::InternalError,
            EngineError::Json(_) => ErrorCode::InvalidInput,
            EngineError::Configuration(_) => ErrorCode::InternalError,
            EngineError::Prediction(msg) => {
                if msg.to_lowercase().contains("context") && msg.to_lowercase().contains("length") {
                    ErrorCode::ContextLengthExceeded
                } else if msg.to_lowercase().contains("cuda out of memory") {
                    ErrorCode::GpuOutOfMemory
                } else {
                    ErrorCode::InternalError
                }
            }
            EngineError::Model(e) => {
                if e.to_string().to_lowercase().contains("not loaded") {
                    ErrorCode::ModelNotLoaded
                } else {
                    ErrorCode::InternalError
                }
            }
            _ => ErrorCode::InternalError,
        }
    }
}
