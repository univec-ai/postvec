//! Tokenizer config as it appears in `ninference.hub.json`.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// Determines which tokenizer implementation to use.
    pub tokenizer_type: TokenizerType,
    /// Type-specific parameters (vocab paths, pretrained name, ...).
    pub params: Value,
    /// Maximum sequence length.
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

/// Tokenizer implementation. JSON uses kebab-case (`huggingface-pretrained`).
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
    // SentencePiece, loaded as a Hugging Face Unigram model.
    #[serde(rename = "huggingface-unigram")]
    HuggingFaceUnigram,
    // Present in the schema; this engine does not implement them.
    #[serde(rename = "native-bert-tokenizer")]
    NativeBertTokenizer,
    #[serde(rename = "native-marian-tokenizer")]
    NativeMarianTokenizer,
    #[serde(rename = "native-bert-fixed-vocab-tokenizer")]
    NativeBertFixedVocabTokenizer,
}

/// Files / name for a pretrained Hugging Face tokenizer.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFacePretrainedParams {
    #[serde(default)]
    pub pretrained_name: String,
    #[serde(default)]
    pub pretrained_vocab_file: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceBpeParams {
    #[serde(default)]
    pub vocab_file_path: String,
    #[serde(default)]
    pub merges_file_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceWordpieceParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceWordlevelParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HuggingFaceUnigramParams {
    #[serde(default)]
    pub vocab_file_path: String,
}

fn default_max_length() -> usize {
    512
}
