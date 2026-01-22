//! ## Tokenizer Error Handling
//!
//! This module defines the error types specific to the tokenizers crate.
//! Using `thiserror` allows us to create a structured error enum that can
//! be easily converted from other error types and provides clear messages.

use thiserror::Error;

/// The primary error enum for the tokenizers crate.
///
/// Each variant represents a different class of error that can occur during
/// tokenizer instantiation or use. The `#[from]` attribute enables automatic
/// and ergonomic error conversion using the `?` operator.
#[derive(Error, Debug)]
pub enum TokenizerError {
    /// An error related to invalid or missing configuration.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// An error that occurs during the tokenization process itself.
    #[error("Tokenization process failed: {0}")]
    Process(String),

    /// An error during JSON serialization or deserialization, typically of config.
    #[error("JSON processing error: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O error, e.g., failing to read a vocabulary file.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A general-purpose variant to wrap errors from underlying tokenizer libraries.
    /// Using `anyhow::Error` provides a flexible way to handle various error
    /// types from third-party crates without causing implementation conflicts.
    #[error("Underlying tokenizer library error: {0}")]
    Library(#[from] anyhow::Error),
}
