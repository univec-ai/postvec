// File: engine/src/tokenizers/huggingface.rs
//!
//! ## Hugging Face Tokenizer Implementation
//!
//!
//!
//! This module provides the `Tokenizer` trait implementation for models supported by
//! the Hugging Face `tokenizers` crate.
//!
//!
//!
//! It handles loading various tokenizer types
//! (Pretrained, BPE, WordPiece, WordLevel, and Unigram for SentencePiece) and
//!
//!
//! implements the `encode_plus` method to provide detailed, word-aligned
//! tokenization output.
// Use items from the parent module (`tokenizers::mod.rs`).
use super::{TokenWithPosition, Tokenizer, TransformerEncodingsWithPosition};
// Use items from sibling modules (`tokenizers::config` and `tokenizers::error`).
// The `super::` prefix is used because `config` and `error` are declared in the parent `mod.rs`.
use super::config::{
    Config as TokenizerConfig, HuggingFaceBpeParams, HuggingFacePretrainedParams,
    HuggingFaceUnigramParams, HuggingFaceWordlevelParams, HuggingFaceWordpieceParams,
    TokenizerType,
};
use super::error::TokenizerError;
use anyhow::Context;
use std::collections::HashMap;
// We now use `hf_tokenizers` to refer to the external crate, which was renamed
// in Cargo.toml to resolve the name conflict with our internal `tokenizers` module.
// This change fixes all the "unresolved import" and "type annotation needed" errors.
use hf_tokenizers::models::bpe::BPE;
use hf_tokenizers::models::unigram::Unigram;
use hf_tokenizers::models::wordlevel::WordLevel;
use hf_tokenizers::models::wordpiece::WordPiece;
use hf_tokenizers::{PaddingParams, PaddingStrategy, Tokenizer as HfTokenizer, TruncationParams};

/// A struct that wraps the `hf_tokenizers::Tokenizer` and holds configuration.
#[derive(Debug, Clone)]
pub struct HuggingFaceTokenizer {
    instance: HfTokenizer,
    lowercase: bool,
}

/// Resolve a configured tokenizer file path against the model's base
/// directory (audit-p3.md P3-R05).
///
/// Every file-backed tokenizer shape resolves the same way: a relative
/// configured path is relative to the **model directory** — the layout the
/// registry packages and the publisher/installer validate — never to the
/// server process's working directory. An absolute configured path is
/// honoured as-is (an operator's explicit choice), and with no base
/// directory the configured value passes through unchanged.
fn resolve_tokenizer_path(configured: &str, maybe_base_path: Option<&str>) -> String {
    match maybe_base_path {
        Some(base) if !configured.is_empty() => {
            let path = std::path::Path::new(configured);
            if path.is_absolute() {
                configured.to_string()
            } else {
                std::path::Path::new(base)
                    .join(path)
                    .to_string_lossy()
                    .into_owned()
            }
        }
        _ => configured.to_string(),
    }
}

impl HuggingFaceTokenizer {
    /// The constructor for the HuggingFaceTokenizer.
    ///
    /// It now accepts the entire `TokenizerConfig` object.
    /// This allows it to access
    /// top-level settings like `max_length` while also using the nested `params`
    /// field to deserialize type-specific configuration (like `pretrained_name`).
    pub fn new(
        config: &TokenizerConfig,
        maybe_base_path: Option<&str>,
    ) -> Result<Self, TokenizerError> {
        // Use the `params` field from the config for type-specific deserialization.
        let params_value = &config.params;
        let tokenizer_type = &config.tokenizer_type;

        // Load the actual tokenizer instance based on its type.
        let mut instance = match tokenizer_type {
            TokenizerType::HuggingFacePretrained => {
                let specific_params: HuggingFacePretrainedParams =
                    serde_json::from_value(params_value.clone())?;
                // The library returns a `Box<dyn Error>`, which is an unsized type.
                // We map the error into an `anyhow::Error` to handle it uniformly.
                if specific_params.pretrained_vocab_file.is_empty() {
                    return Err(anyhow::anyhow!(
                        "tokenizer for '{}' declares no pretrained_vocab_file; \
                         downloading from the Hugging Face Hub is not supported \
                         in this build — package the tokenizer file with the model",
                        specific_params.pretrained_name
                    )
                    .into());
                } else {
                    let absolute_vocab_file_path = resolve_tokenizer_path(
                        &specific_params.pretrained_vocab_file,
                        maybe_base_path,
                    );
                    let error_message = format!(
                        "Failed to load pretrained tokenizer from {:?}",
                        absolute_vocab_file_path
                    );
                    HfTokenizer::from_file(absolute_vocab_file_path)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))
                        .with_context(|| error_message)?
                }
            }
            TokenizerType::HuggingFaceBpe => {
                let specific_params: HuggingFaceBpeParams =
                    serde_json::from_value(params_value.clone())?;
                // Every file-backed shape resolves against the model base
                // directory, exactly like the pretrained branch (P3-R05) —
                // the packaged model-relative paths must be what runtime opens.
                let vocab =
                    resolve_tokenizer_path(&specific_params.vocab_file_path, maybe_base_path);
                let merges =
                    resolve_tokenizer_path(&specific_params.merges_file_path, maybe_base_path);
                // Correctly use the builder pattern: build the model first, then
                // create the tokenizer instance with the built model.
                let bpe_model = BPE::from_file(&vocab, &merges)
                    .build()
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .with_context(|| {
                        format!("Failed to build BPE model from {vocab:?} + {merges:?}")
                    })?;
                HfTokenizer::new(bpe_model)
            }
            TokenizerType::HuggingFaceWordPiece => {
                let specific_params: HuggingFaceWordpieceParams =
                    serde_json::from_value(params_value.clone())?;
                let vocab =
                    resolve_tokenizer_path(&specific_params.vocab_file_path, maybe_base_path);
                // Correctly use the builder pattern for WordPiece.
                let wordpiece_model = WordPiece::from_file(&vocab)
                    .build()
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .with_context(|| format!("Failed to build WordPiece model from {vocab:?}"))?;
                HfTokenizer::new(wordpiece_model)
            }
            TokenizerType::HuggingFaceWordLevel => {
                let specific_params: HuggingFaceWordlevelParams =
                    serde_json::from_value(params_value.clone())?;
                let vocab =
                    resolve_tokenizer_path(&specific_params.vocab_file_path, maybe_base_path);
                // The `WordLevel::from_file` function requires a second argument for the unknown token.
                let wordlevel_model = WordLevel::from_file(&vocab, "[UNK]".to_string())
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .with_context(|| format!("Failed to load WordLevel model from {vocab:?}"))?;
                HfTokenizer::new(wordlevel_model)
            }
            TokenizerType::HuggingFaceUnigram => {
                let specific_params: HuggingFaceUnigramParams =
                    serde_json::from_value(params_value.clone())?;
                let vocab =
                    resolve_tokenizer_path(&specific_params.vocab_file_path, maybe_base_path);
                // The `Unigram` model uses `load` instead of `from_file`.
                let unigram_model = Unigram::load(&vocab)
                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                    .with_context(|| {
                        format!("Failed to load Unigram (SentencePiece) model from {vocab:?}")
                    })?;
                HfTokenizer::new(unigram_model)
            }
            _ => {
                return Err(TokenizerError::Configuration(format!(
                    "Invalid or unsupported HuggingFace tokenizer type provided: {:?}",
                    tokenizer_type
                )));
            }
        };

        // Now, configure truncation and padding using the top-level fields from the config.
        if !config.truncation_disabled && config.max_length > 0 {
            let truncation_params = TruncationParams {
                max_length: config.max_length,
                ..Default::default()
            };
            // The `with_truncation` method modifies the instance in place. We just need to handle the potential error.
            instance
                .with_truncation(Some(truncation_params))
                .map_err(|e| TokenizerError::Process(e.to_string()))?;
        }

        if !config.padding_disabled {
            // Using `BatchLongest` is more efficient for batch processing as it pads
            // each batch to the length of the longest sequence in that batch, rather than
            // a fixed global max length.
            let padding_params = PaddingParams {
                strategy: PaddingStrategy::BatchLongest,
                ..Default::default()
            };
            // `with_padding` also modifies the instance in place.
            instance.with_padding(Some(padding_params));
        }

        Ok(Self {
            instance,
            lowercase: config.lowercase,
        })
    }

    /// Converts a single `hf_tokenizers::Encoding` into our custom `TransformerEncodingsWithPosition`.
    /// This is a helper to reduce code duplication between `encode_plus` and `encode_batch`.
    fn convert_encoding_to_custom(
        &self,
        encoding: hf_tokenizers::Encoding,
    ) -> Result<TransformerEncodingsWithPosition, TokenizerError> {
        // Step 2: Group token IDs by their corresponding word ID.
        let mut tokens_by_word_id: HashMap<u32, Vec<u32>> = HashMap::new();
        for (token_id, word_id_option) in encoding.get_ids().iter().zip(encoding.get_word_ids()) {
            if let Some(word_id) = word_id_option {
                tokens_by_word_id
                    .entry(*word_id)
                    .or_default()
                    .push(*token_id);
            }
        }

        // Step 3: Decode each group of token IDs to get the original word string.
        let mut word_id_to_word: HashMap<u32, String> = HashMap::new();
        for (word_id, token_ids) in &tokens_by_word_id {
            let word = self
                .instance
                .decode(token_ids, false) // `false` to not skip special tokens here
                .map_err(|e| TokenizerError::Process(e.to_string()))?;
            word_id_to_word.insert(*word_id, word.trim().to_string());
        }

        // Step 4: Create the final `TokenWithPosition` list.
        let tokens_with_positions: Vec<TokenWithPosition> = encoding
            .get_tokens()
            .iter()
            .zip(encoding.get_word_ids())
            .zip(encoding.get_offsets())
            .map(|((token_str, word_id_option), (start, end))| {
                let (word_id, original_word) = word_id_option
                    .and_then(|id| {
                        word_id_to_word
                            .get(&id)
                            .map(|word| (id as usize, word.clone()))
                    })
                    .unwrap_or((0, String::new()));

                TokenWithPosition {
                    original_word_idx: word_id,
                    original_word,
                    token_part: token_str.clone(),
                    output_label: 0,
                    line_index: 0,
                    column_index: *start,
                    end_column_index: *end,
                }
            })
            .collect();

        // Assemble the final output structure.
        Ok(TransformerEncodingsWithPosition {
            attention_mask: encoding
                .get_attention_mask()
                .iter()
                .map(|&x| x as i64)
                .collect(),
            input_ids: encoding.get_ids().iter().map(|&x| x as i64).collect(),
            token_type_ids: encoding.get_type_ids().iter().map(|&x| x as i64).collect(),
            tokens_with_positions,
        })
    }
}

impl Tokenizer for HuggingFaceTokenizer {
    fn encode_plus(
        &self,
        text: &str,
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError> {
        let input_text = if self.lowercase {
            text.to_lowercase()
        } else {
            text.to_string()
        };

        let encoding = self
            .instance
            .encode(input_text, true)
            .map_err(|e| TokenizerError::Process(e.to_string()))?;

        let result = self.convert_encoding_to_custom(encoding)?;
        Ok(vec![result])
    }

    fn encode_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<TransformerEncodingsWithPosition>, TokenizerError> {
        let input_texts: Vec<String> = if self.lowercase {
            texts.iter().map(|s| s.to_lowercase()).collect()
        } else {
            texts.iter().map(|s| s.to_string()).collect()
        };

        // Use the library's batch encoding method for efficiency.
        let batch_encodings = self
            .instance
            .encode_batch(input_texts, true)
            .map_err(|e| TokenizerError::Process(e.to_string()))?;

        // Convert each encoding in the batch to our custom format.
        batch_encodings
            .into_iter()
            .map(|encoding| self.convert_encoding_to_custom(encoding))
            .collect()
    }

    fn decode(&self, ids: &[i64]) -> Result<String, TokenizerError> {
        let u32_ids: Vec<u32> = ids.iter().map(|&id| id as u32).collect();
        let decoded = self
            .instance
            .decode(&u32_ids, true)
            .map_err(|e| TokenizerError::Process(e.to_string()))?;
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn relative_paths_resolve_against_the_model_directory() {
        assert_eq!(
            resolve_tokenizer_path("tok/vocab.json", Some("/models/onnx-runtime/m")),
            "/models/onnx-runtime/m/tok/vocab.json"
        );
        // Absolute configured paths are an explicit operator choice.
        assert_eq!(
            resolve_tokenizer_path("/etc/vocab.json", Some("/models/m")),
            "/etc/vocab.json"
        );
        // No base directory: pass through unchanged.
        assert_eq!(resolve_tokenizer_path("vocab.json", None), "vocab.json");
        assert_eq!(resolve_tokenizer_path("", Some("/models/m")), "");
    }

    fn config(tokenizer_type: TokenizerType, params: serde_json::Value) -> TokenizerConfig {
        TokenizerConfig {
            tokenizer_type,
            params,
            max_length: 16,
            padding_disabled: true,
            lowercase: false,
            truncation_disabled: false,
        }
    }

    fn temp_model_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postvec-tokenizer-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("tok")).unwrap();
        dir
    }

    /// P3-R05 load-level proof: a model-relative WordLevel vocabulary loads
    /// (and encodes) when the CWD does not contain it — the base directory,
    /// not the process working directory, is what runtime opens.
    #[test]
    fn wordlevel_loads_a_model_relative_vocabulary() {
        let dir = temp_model_dir();
        std::fs::write(
            dir.join("tok/vocab.json"),
            serde_json::to_vec(&json!({"[UNK]": 0, "hello": 1, "world": 2})).unwrap(),
        )
        .unwrap();
        let tokenizer = HuggingFaceTokenizer::new(
            &config(
                TokenizerType::HuggingFaceWordLevel,
                json!({"vocab_file_path": "tok/vocab.json"}),
            ),
            Some(dir.to_str().unwrap()),
        )
        .expect("loads relative to the model dir");
        let encodings = tokenizer.encode_plus("hello world").unwrap();
        assert!(!encodings[0].input_ids.is_empty());

        // Without the base directory the same config must fail: the file is
        // not CWD-relative.
        assert!(HuggingFaceTokenizer::new(
            &config(
                TokenizerType::HuggingFaceWordLevel,
                json!({"vocab_file_path": "tok/vocab.json"}),
            ),
            None,
        )
        .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wordpiece_loads_a_model_relative_vocabulary() {
        let dir = temp_model_dir();
        std::fs::write(
            dir.join("tok/vocab.txt"),
            "[UNK]\n[CLS]\n[SEP]\nhello\nworld\n",
        )
        .unwrap();
        let tokenizer = HuggingFaceTokenizer::new(
            &config(
                TokenizerType::HuggingFaceWordPiece,
                json!({"vocab_file_path": "tok/vocab.txt"}),
            ),
            Some(dir.to_str().unwrap()),
        )
        .expect("loads relative to the model dir");
        assert!(!tokenizer.encode_plus("hello").unwrap()[0]
            .input_ids
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bpe_loads_model_relative_vocabulary_and_merges() {
        let dir = temp_model_dir();
        std::fs::write(
            dir.join("tok/vocab.json"),
            serde_json::to_vec(&json!({"h": 0, "i": 1, "hi": 2})).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("tok/merges.txt"), "#version: 0.2\nh i\n").unwrap();
        let tokenizer = HuggingFaceTokenizer::new(
            &config(
                TokenizerType::HuggingFaceBpe,
                json!({"vocab_file_path": "tok/vocab.json",
                       "merges_file_path": "tok/merges.txt"}),
            ),
            Some(dir.to_str().unwrap()),
        )
        .expect("loads relative to the model dir");
        assert!(!tokenizer.encode_plus("hi").unwrap()[0].input_ids.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
