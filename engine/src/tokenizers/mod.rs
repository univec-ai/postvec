//! Tokenizers: text to input ids, attention masks and word-aligned offsets.

pub mod config;
pub mod error;
mod huggingface;

use self::config::{Config, TokenizerType};
use self::error::TokenizerError;
use self::huggingface::HuggingFaceTokenizer;
use serde::{Deserialize, Serialize};

/// One token and where it sits in the original string.
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

/// Token ids, masks and per-token offsets for one sequence.
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

pub trait Tokenizer: Send + Sync {
    /// Tokenize one string. A vec is returned so chunked inputs can yield
    /// more than one sequence.
    fn encode_plus(
        &self,
        text: &str,
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError>;

    /// Tokenize a batch. Prefer this over looping `encode_plus`.
    fn encode_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError>;

    /// Token ids back to a string.
    fn decode(&self, ids: &[i64]) -> Result<String, TokenizerError>;
}

/// Build a tokenizer from config. Relative vocab paths resolve against `maybe_base_path`.
pub fn new_tokenizer(
    config: &Config,
    maybe_base_path: Option<&str>,
) -> Result<Box<dyn Tokenizer>, TokenizerError> {
    log::debug!("Creating tokenizer of type: {:?}", config.tokenizer_type);
    match config.tokenizer_type {
        TokenizerType::HuggingFaceBpe
        | TokenizerType::HuggingFacePretrained
        | TokenizerType::HuggingFaceWordLevel
        | TokenizerType::HuggingFaceWordPiece
        | TokenizerType::HuggingFaceUnigram => {
            let tokenizer = HuggingFaceTokenizer::new(config, maybe_base_path)?;
            Ok(Box::new(tokenizer))
        }
        _ => Err(TokenizerError::Configuration(format!(
            "Tokenizer type '{:?}' is not yet supported in the Rust implementation.",
            config.tokenizer_type
        ))),
    }
}
