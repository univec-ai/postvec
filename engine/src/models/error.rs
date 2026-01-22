// File: engine/src/models/error.rs
//!
//! ## Model Error Types
//!
//! Defines the custom error types for the models library using `thiserror`
//! for ergonomic and descriptive error handling.

use thiserror::Error;

/// The primary error enum for all model-related operations.
#[derive(Error, Debug)]
pub enum ModelError {
    #[error("The method '{0}' is not implemented for the '{1}' backend")]
    NotImplemented(String, String),

    // We remove the "Model is not valid:" prefix to show the inner error directly.
    #[error("{0}")]
    InvalidModel(String),

    // We remove the "Model query failed:" prefix.
    #[error("{0}")]
    QueryError(String),

    // We remove the "Configuration error:" prefix.
    #[error("{0}")]
    ConfigurationError(String),

    // We make the VectorError transparent to show its specific message.
    #[error(transparent)]
    VectorError(#[from] shared::vectors::VectorError),

    /// Errors from the ONNX Runtime backend, enabled by the `onnx` feature.
    // We make this transparent. This is the key fix.
    // Instead of "ONNX runtime operation failed: [ort_error]",
    // it will now just show "[ort_error]".
    #[cfg(feature = "onnx")]
    #[error(transparent)]
    OnnxRuntimeError(#[from] ort::Error),

    // We make the ShapeError transparent.
    #[error(transparent)]
    ShapeError(#[from] ndarray::ShapeError),

    // We make I/O errors transparent.
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    // We make JSON errors transparent.
    #[error(transparent)]
    JsonError(#[from] serde_json::Error),

    #[error("Application was not compiled with support for the '{0}' backend")]
    BackendNotSupported(String),
}
