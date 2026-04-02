//! Tokenizer load and encode failures.

use thiserror::Error;

#[derive(Error, Debug)]
pub enum TokenizerError {
    #[error("Configuration error: {0}")]
    Configuration(String),

    #[error("Tokenization process failed: {0}")]
    Process(String),

    #[error("JSON processing error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Underlying tokenizer library error: {0}")]
    Library(#[from] anyhow::Error),
}
