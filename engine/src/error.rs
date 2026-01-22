// File: engine/src/error.rs
//!
//! ## Engine Error Handling
//!
//! This module defines the primary error enum `EngineError` for the inference engine.
//!
//! Using `thiserror`, it consolidates errors from various sources (I/O, JSON, models, etc.)
//! into a single, crate-specific type.
use thiserror::Error;
// Update the use path to point to the new, internal tokenizers module.
use crate::tokenizers::error::TokenizerError;
use ndarray::ShapeError;
use shared::vectors::VectorError;

/// The primary error type for the `engine` crate.
#[derive(Error, Debug)]
pub enum EngineError {
    /// An error originating from the internal models module.
    // We make this transparent so the ModelError message is shown directly,
    // instead of being prefixed with "Model error:".
    #[error(transparent)]
    Model(#[from] crate::models::ModelError),

    /// An error related to invalid or missing configuration.
    // We remove the "Configuration error:" prefix.
    #[error("{0}")]
    Configuration(String),

    /// An error that occurs during I/O operations, such as file access.
    // We make this transparent.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// An error during JSON serialization or deserialization.
    // We make this transparent.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A requested resource (like a model or executor) could not be found.
    // We remove the "Not found:" prefix.
    #[error("{0}")]
    NotFound(String),

    /// An error occurred during the prediction/inference process.
    // We remove the "Prediction failed:" prefix.
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

    /// An error occurred while converting an input value to a target type.
    // We remove the "Input type error:" prefix.
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

    /// An error originating from the integrated tokenizers module.
    // We make this transparent to show the specific tokenizer error.
    #[error(transparent)]
    Tokenizer(#[from] TokenizerError),

    /// An error originating from the vectors library.
    // We make this transparent.
    #[error(transparent)]
    Vector(#[from] VectorError),

    /// An error from ndarray shape operations.
    // We make this transparent.
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
            EngineError::Io(_) => ErrorCode::InternalError, // Could break down further if needed
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
                // Inspect inner model error if possible, for now map to internal
                // In future, ModelError could also implement to_error_code
                if e.to_string().to_lowercase().contains("not loaded") {
                    ErrorCode::ModelNotLoaded
                } else {
                    ErrorCode::InternalError
                }
            }
            // For now map others to internal, can refine later
            _ => ErrorCode::InternalError,
        }
    }
}
