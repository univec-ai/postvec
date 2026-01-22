// File: engine/src/tokenizers/mod.rs
//!
//! ## Tokenizers Module
//!
//!
//! This module provides a unified interface for using different tokenization strategies,
//!
//! primarily for preparing text inputs for machine learning models.
//! It is an integrated
//! part of the `engine` crate.
//!
//! ### Core Components:
//!
//! - `Tokenizer` trait: The central contract for all tokenizers.
//!
//!
//! - `new_tokenizer`: A factory function to create tokenizer instances from configuration.
//! - `TokenWithPosition`, `TransformerEncodingsWithPosition`: Data structures for
//!
//!
//! detailed tokenization output, including mappings back to the original text.
// Declare the sub-modules of the `tokenizers` module.
// These are visible within the `engine` crate but are primarily used by this module.
pub mod config;
pub mod error;
mod huggingface;

// Bring necessary items from sub-modules into this module's scope.
// `self` is used to refer to the current module (`tokenizers`).
use self::config::{Config, TokenizerType};
use self::error::TokenizerError;
use self::huggingface::HuggingFaceTokenizer;
use serde::{Deserialize, Serialize};

/// Represents a single token and its relationship to the original input text.
/// This structure is essential for tasks that require aligning model outputs
/// with original words, such as named entity recognition.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TokenWithPosition {
    /// The index of the original word in the source text.
    pub original_word_idx: usize,
    /// The original word from which this token (or sub-token) was derived.
    pub original_word: String,
    /// The token string itself (e.g., "token", "##izer").
    pub token_part: String,
    /// A placeholder for an output label, often used in classification tasks.
    pub output_label: i32,
    /// The line index where the original word appeared (placeholder, typically 0).
    pub line_index: usize,
    /// The starting byte offset of the token in the original input string.
    pub column_index: usize,
    /// The ending byte offset of the token in the original input string.
    pub end_column_index: usize,
}

/// Represents the complete tokenization output for a sequence, ready for a model.
/// This mirrors the `TransformerEncodingsWithPosition` struct from the Go implementation.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransformerEncodingsWithPosition {
    /// The attention mask tensor, indicating which tokens should be attended to.
    pub attention_mask: Vec<i64>,
    /// The input IDs (token indices) tensor for the model.
    pub input_ids: Vec<i64>,
    /// The token type IDs (segment IDs) tensor, for models like BERT.
    pub token_type_ids: Vec<i64>,
    /// Detailed information about each token and its position.
    pub tokens_with_positions: Vec<TokenWithPosition>,
}

/// The primary trait defining the contract for all tokenizers in the application.
///
/// This trait ensures that any tokenizer, regardless of its underlying implementation
/// can be used interchangeably by the application's executors.
pub trait Tokenizer: Send + Sync {
    /// Tokenizes a string into a detailed structure including input IDs, attention masks,
    /// and positional information mapping tokens back to the original text.
    ///
    /// # Arguments
    /// * `text` - The input string to tokenize.
    ///
    /// # Returns
    /// A `Result` containing a vector of `TransformerEncodingsWithPosition` objects.
    /// A vector is returned to support models that might handle text in chunks.
    fn encode_plus(
        &self,
        text: &str,
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError>;

    /// Tokenizes a batch of strings. This is often more efficient than calling `encode_plus` in a loop.
    ///
    /// # Arguments
    /// * `texts` - A slice of input strings to tokenize.
    ///
    /// # Returns
    /// A `Result` containing a vector of `TransformerEncodingsWithPosition` objects, one for each input string.
    fn encode_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError>;

    /// Decodes a sequence of token IDs back into a string.
    ///
    /// # Arguments
    /// * `ids` - A slice of token IDs.
    ///
    // # Returns
    /// A `Result` containing the reconstructed string.
    fn decode(&self, ids: &[i64]) -> Result<String, TokenizerError>;
}

/// A factory function to create a concrete `Tokenizer` instance from a configuration object.
///
/// This function acts as a router, delegating the creation of the tokenizer to the
/// unified `HuggingFaceTokenizer` implementation, which can handle all supported types.
///
/// # Arguments
/// * `config` - The configuration object defining the type and parameters of the tokenizer.
/// * `maybe_base_path` - An optional path to the model's directory, used to resolve relative paths for vocabulary files.
///
/// # Returns
/// A `Result` containing a boxed `Tokenizer` trait object, or an error if instantiation fails.
pub fn new_tokenizer(
    config: &Config,
    maybe_base_path: Option<&str>,
) -> Result<Box<dyn Tokenizer>, TokenizerError> {
    log::debug!("Creating tokenizer of type: {:?}", config.tokenizer_type);
    match config.tokenizer_type {
        // All supported tokenizer types are now handled by the HuggingFaceTokenizer.
        // This simplifies the factory logic significantly.
        TokenizerType::HuggingFaceBpe
        | TokenizerType::HuggingFacePretrained
        | TokenizerType::HuggingFaceWordLevel
        | TokenizerType::HuggingFaceWordPiece
        | TokenizerType::HuggingFaceUnigram => {
            // Pass the entire config object to the constructor.
            let tokenizer = HuggingFaceTokenizer::new(config, maybe_base_path)?;
            Ok(Box::new(tokenizer))
        }
        // Use a catch-all for any tokenizer types not yet implemented.
        _ => Err(TokenizerError::Configuration(format!(
            "Tokenizer type '{:?}' is not yet supported in the Rust implementation.",
            config.tokenizer_type
        ))),
    }
}
