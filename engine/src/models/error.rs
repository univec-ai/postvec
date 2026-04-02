//! Errors from model load, query and configuration.

use thiserror::Error;

/// Failures from model load, query and configuration. Inner errors are
/// shown without a wrapping prefix.
#[derive(Error, Debug)]
pub enum ModelError {
    #[error("The method '{0}' is not implemented for the '{1}' backend")]
    NotImplemented(String, String),

    #[error("{0}")]
    InvalidModel(String),

    #[error("{0}")]
    QueryError(String),

    #[error("{0}")]
    ConfigurationError(String),

    #[error(transparent)]
    VectorError(#[from] shared::vectors::VectorError),

    /// ONNX Runtime backend (`onnx` feature).
    #[cfg(feature = "onnx")]
    #[error(transparent)]
    OnnxRuntimeError(#[from] ort::Error),

    #[error(transparent)]
    ShapeError(#[from] ndarray::ShapeError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error(transparent)]
    JsonError(#[from] serde_json::Error),

    #[error("Application was not compiled with support for the '{0}' backend")]
    BackendNotSupported(String),
}
