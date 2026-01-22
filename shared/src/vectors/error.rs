use ndarray::ShapeError;
use thiserror::Error;

/// @brief The primary error type for all fallible operations in this crate.
#[derive(Error, Debug)]
pub enum VectorError {
    /// @brief Error representing a mismatch in tensor/vector dimensions.
    #[error("Vectors dimensions mismatch: expected {expected:?}, got {actual:?}")]
    DimensionMismatch {
        expected: Vec<usize>,
        actual: Vec<usize>,
    },
    /// @brief Error for operations on an empty vector where it's not allowed.
    #[error("Vector is empty")]
    EmptyVector,
    /// @brief Error when a vector's norm is zero, preventing normalization.
    #[error("Vector norm is zero")]
    ZeroNorm,
    /// @brief Error during division by zero.
    #[error("Division by zero")]
    DivisionByZero,
    /// @brief Error for when input shape cannot be converted to a target shape.
    #[error("Cannot reshape: vector (size {size}) cannot fit to dimensions {dims:?}")]
    ReshapeError { size: usize, dims: Vec<usize> },
    /// @brief General error for failed type conversions.
    #[error("Cannot convert: {0}")]
    ConversionError(String),
    /// @brief Errors originating from I/O operations (e.g., reading files).
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),
    /// @brief Errors from parsing binary data.
    #[error("Binary format error: {0}")]
    BinaryError(String),
    /// @brief Error when a word is not found in the vocabulary.
    #[error("Word not found in model: {0}")]
    WordNotFound(String),
    /// @brief Error when an operation expects a list that is empty or invalid.
    #[error("Invalid or empty list for operation: {0}")]
    InvalidList(String),
    /// @brief Error for failed parsing from a string.
    #[error("Failed to parse string: {0}")]
    ParseFloatError(#[from] std::num::ParseFloatError),
    /// @brief Error for invalid input parameters.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    /// @brief Error propagating from `ndarray` shape operations.
    #[error("Shape error: {0}")]
    ShapeError(#[from] ShapeError),
    /// @brief Error propagating from `serde_json` operations.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}
