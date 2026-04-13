use ndarray::ShapeError;
use thiserror::Error;

/// Failures from vector, tensor and file operations.
#[derive(Error, Debug)]
pub enum VectorError {
    #[error("Vectors dimensions mismatch: expected {expected:?}, got {actual:?}")]
    DimensionMismatch {
        expected: Vec<usize>,
        actual: Vec<usize>,
    },
    #[error("Vector is empty")]
    EmptyVector,
    #[error("Vector norm is zero")]
    ZeroNorm,
    #[error("Division by zero")]
    DivisionByZero,
    #[error("Cannot reshape: vector (size {size}) cannot fit to dimensions {dims:?}")]
    ReshapeError { size: usize, dims: Vec<usize> },
    #[error("Cannot convert: {0}")]
    ConversionError(String),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Binary format error: {0}")]
    BinaryError(String),
    #[error("Word not found in model: {0}")]
    WordNotFound(String),
    #[error("Invalid or empty list for operation: {0}")]
    InvalidList(String),
    #[error("Failed to parse string: {0}")]
    ParseFloatError(#[from] std::num::ParseFloatError),
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    #[error("Shape error: {0}")]
    ShapeError(#[from] ShapeError),
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}
