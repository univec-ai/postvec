// File: engine/src/tokenizers/config.rs
//!
//! ## Tokenizer Configuration
//!
//! This module defines the structures for configuring the different tokenizers.
//!
//! Using `serde`, these structs can be deserialized directly from a model's
//! JSON configuration file (`ninference.hub.json`), providing a strongly-typed
//! way to handle tokenizer parameters.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Defines the top-level structure for any tokenizer configuration.
/// This is the entry point for deserialization from a model's config file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// Determines which tokenizer implementation to use.
    pub tokenizer_type: TokenizerType,
    /// A flexible JSON Value containing the specific parameters for the chosen
    /// tokenizer type. This allows for diverse configurations from a single file.
    pub params: Value,
    /// The maximum sequence length for the tokenizer. This is now a top-level
    /// field to ensure it's correctly deserialized from the model config.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    /// Disables padding if set to true.
    #[serde(default)]
    pub padding_disabled: bool,
    /// Converts text to lowercase before tokenization if true.
    #[serde(default)]
    pub lowercase: bool,
    /// Disables truncation if set to true.
    #[serde(default)]
    pub truncation_disabled: bool,
}

/// An enum representing the different available tokenizer types.
/// The `serde(rename_all = "kebab-case")` attribute ensures that strings like
/// "huggingface-pretrained" in the JSON correctly map to the enum variants.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum TokenizerType {
    #[serde(rename = "huggingface-pretrained")]
    HuggingFacePretrained,
    #[serde(rename = "huggingface-bpe")]
    HuggingFaceBpe,
    #[serde(rename = "huggingface-wordpiece")]
    HuggingFaceWordPiece,
    #[serde(rename = "huggingface-wordlevel")]
    HuggingFaceWordLevel,
    // This variant is for SentencePiece models, which are handled by the Unigram
    // model in the Hugging Face `tokenizers` crate.
    #[serde(rename = "huggingface-unigram")]
    HuggingFaceUnigram,
    // Native tokenizers from the original Go implementation are listed for completeness,
    // but are not implemented in this pure Rust version.
    #[serde(rename = "native-bert-tokenizer")]
    NativeBertTokenizer,
    #[serde(rename = "native-marian-tokenizer")]
    NativeMarianTokenizer,
    #[serde(rename = "native-bert-fixed-vocab-tokenizer")]
    NativeBertFixedVocabTokenizer,
}

// --- Hugging Face Specific Configurations ---
// Note: The common `HuggingFaceParams` struct has been removed, and its fields
// have been promoted to the main `Config` struct for correct deserialization.

/// Parameters for loading a pretrained Hugging Face tokenizer from the Hub.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFacePretrainedParams {
    #[serde(default)]
    pub pretrained_name: String,
    #[serde(default)]
    pub pretrained_vocab_file: String,
}

/// Parameters for loading a Hugging Face BPE (Byte-Pair Encoding) tokenizer from files.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceBpeParams {
    #[serde(default)]
    pub vocab_file_path: String,
    #[serde(default)]
    pub merges_file_path: String,
}

/// Parameters for loading a Hugging Face WordPiece tokenizer from a vocabulary file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceWordpieceParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

/// Parameters for loading a Hugging Face WordLevel tokenizer from a vocabulary file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceWordlevelParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

/// Parameters for loading a Hugging Face Unigram (SentencePiece) tokenizer from a file.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceUnigramParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

// --- Helper functions to provide default values for optional serde fields ---
fn default_max_length() -> usize {
    512 // A common default sequence length.
}
