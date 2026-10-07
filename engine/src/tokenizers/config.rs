//! Tokenizer config as it appears in `ninference.hub.json`.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    /// Determines which tokenizer implementation to use.
    pub tokenizer_type: TokenizerType,
    /// Type-specific parameters (vocab paths, pretrained name, ...).
    pub params: Value,
    /// Explicit truncation length in tokens, special tokens included.
    ///
    /// When absent, executors fill it from the model's `params.sequence_len`
    /// via [`Config::resolve_max_length`], then fall back to
    /// [`DEFAULT_MAX_LENGTH`] when the model declares nothing. `Some(0)`
    /// means no truncation, same as `truncation_disabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    /// Disables padding if set to true.
    #[serde(default)]
    pub padding_disabled: bool,
    /// Converts text to lowercase before tokenization if true.
    #[serde(default)]
    pub lowercase: bool,
    /// Disables truncation if set to true.
    #[serde(default)]
    pub truncation_disabled: bool,
    /// Which end of an over-long input is cut. Absent = the executor's default:
    /// `right` (keep the start, as sentence-transformers and HF tokenizers do) for
    /// encoders, `left` for generation prompts so their end (usually the question)
    /// survives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_side: Option<TruncationSide>,
}

/// The end of the input that truncation removes tokens from.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TruncationSide {
    #[default]
    Right,
    Left,
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

/// Truncation length when neither the tokenizer block (`max_length`) nor
/// the model (`params.sequence_len`) declares one.
pub const DEFAULT_MAX_LENGTH: usize = 512;

/// Where the effective truncation length came from. Logged at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxLengthSource {
    /// `executor…tokenizer.max_length` was set in the descriptor.
    Explicit,
    /// Clamped by the engine-wide ceiling (`HostPolicy::max_sequence_len`).
    HostCap,
    /// Taken from the model's `params.sequence_len`.
    SequenceLen,
    /// Neither was set; [`DEFAULT_MAX_LENGTH`] applies.
    Default,
}

impl std::fmt::Display for MaxLengthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            MaxLengthSource::Explicit => "the descriptor's tokenizer.max_length",
            MaxLengthSource::HostCap => "the engine-wide ceiling",
            MaxLengthSource::SequenceLen => "the descriptor's sequence_len",
            MaxLengthSource::Default => "the engine default (the descriptor declares no length)",
        })
    }
}

impl Config {
    /// Resolve the effective truncation length and store it in `max_length`.
    ///
    /// Precedence is an explicit `max_length`, then the model's declared
    /// `sequence_len`, then [`DEFAULT_MAX_LENGTH`]. The result is clamped to
    /// `host_cap`, the engine-wide ceiling from
    /// `HostPolicy::sequence_len_cap`. An explicit value above `sequence_len`
    /// is kept, because the operator set it, and logged, because positions
    /// past the model's limit are usually invalid.
    pub fn resolve_max_length(
        &mut self,
        sequence_len: Option<usize>,
        host_cap: Option<usize>,
        model_name: &str,
    ) -> (usize, MaxLengthSource) {
        let (mut effective, mut source) = match (self.max_length, sequence_len) {
            (Some(explicit), declared) => {
                if let Some(declared) = declared {
                    if explicit == 0 || explicit > declared {
                        log::warn!(
                            "Model '{}': tokenizer max_length {} exceeds the declared sequence_len {}",
                            model_name,
                            if explicit == 0 { "0 (unbounded)".to_string() } else { explicit.to_string() },
                            declared
                        );
                    }
                }
                (explicit, MaxLengthSource::Explicit)
            }
            (None, Some(declared)) => (declared, MaxLengthSource::SequenceLen),
            (None, None) => {
                log::warn!(
                    "Model '{}' declares neither tokenizer max_length nor params.sequence_len; truncating at {} tokens",
                    model_name,
                    DEFAULT_MAX_LENGTH
                );
                (DEFAULT_MAX_LENGTH, MaxLengthSource::Default)
            }
        };
        // The ceiling also bounds an unlimited length (`0`) and
        // `truncation_disabled`. One over-long input can exhaust memory.
        if let Some(cap) = host_cap {
            let unbounded = effective == 0 || self.truncation_disabled;
            if unbounded || effective > cap {
                log::warn!(
                    "Model '{}': truncation length {} clamped to the engine-wide ceiling of {} tokens",
                    model_name,
                    if unbounded { "unbounded".to_string() } else { effective.to_string() },
                    cap
                );
                effective = cap;
                source = MaxLengthSource::HostCap;
                self.truncation_disabled = false;
            }
        }
        self.max_length = Some(effective);
        log::info!(
            "Model '{}': tokenizer truncates at {} tokens (source: {:?})",
            model_name,
            if effective == 0 || self.truncation_disabled { "no limit".to_string() } else { effective.to_string() },
            source
        );
        (effective, source)
    }

    /// The truncation length the tokenizer will apply, or `None` when it will
    /// not truncate (`truncation_disabled`, or an explicit `0`).
    pub fn effective_max_length(&self) -> Option<usize> {
        if self.truncation_disabled {
            return None;
        }
        match self.max_length.unwrap_or(DEFAULT_MAX_LENGTH) {
            0 => None,
            n => Some(n),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: serde_json::Value) -> Config {
        serde_json::from_value(v).unwrap()
    }

    fn base() -> serde_json::Value {
        json!({ "tokenizer_type": "huggingface-pretrained", "params": {} })
    }

    #[test]
    fn absent_max_length_deserializes_as_none() {
        assert_eq!(parse(base()).max_length, None);
        let mut v = base();
        v["max_length"] = json!(2048);
        assert_eq!(parse(v).max_length, Some(2048));
    }

    #[test]
    fn sequence_len_fills_an_absent_max_length() {
        let mut c = parse(base());
        assert_eq!(c.resolve_max_length(Some(8192), None, "m"), (8192, MaxLengthSource::SequenceLen));
        assert_eq!(c.max_length, Some(8192));
        assert_eq!(c.effective_max_length(), Some(8192));
    }

    #[test]
    fn explicit_max_length_wins_over_sequence_len() {
        let mut v = base();
        v["max_length"] = json!(2048);
        let mut c = parse(v);
        assert_eq!(c.resolve_max_length(Some(32768), None, "m"), (2048, MaxLengthSource::Explicit));
        // Larger than declared is honoured (and logged), not clamped.
        let mut v = base();
        v["max_length"] = json!(1024);
        let mut c = parse(v);
        assert_eq!(c.resolve_max_length(Some(256), None, "m"), (1024, MaxLengthSource::Explicit));
    }

    #[test]
    fn nothing_declared_keeps_the_historical_default() {
        let mut c = parse(base());
        assert_eq!(c.resolve_max_length(None, None, "m"), (DEFAULT_MAX_LENGTH, MaxLengthSource::Default));
        // An unresolved config also truncates at the default.
        assert_eq!(parse(base()).effective_max_length(), Some(DEFAULT_MAX_LENGTH));
    }

    #[test]
    fn host_cap_clamps_declared_explicit_and_unbounded_lengths() {
        let mut c = parse(base());
        assert_eq!(c.resolve_max_length(Some(32768), Some(8192), "m"), (8192, MaxLengthSource::HostCap));
        // Below the cap nothing changes.
        let mut c = parse(base());
        assert_eq!(c.resolve_max_length(Some(2048), Some(8192), "m"), (2048, MaxLengthSource::SequenceLen));
        // An explicit 0 ("unlimited") and truncation_disabled are bounded too.
        let mut v = base();
        v["max_length"] = json!(0);
        let mut c = parse(v);
        assert_eq!(c.resolve_max_length(None, Some(8192), "m").0, 8192);
        assert_eq!(c.effective_max_length(), Some(8192));
        let mut v = base();
        v["truncation_disabled"] = json!(true);
        let mut c = parse(v);
        c.resolve_max_length(Some(512), Some(8192), "m");
        assert_eq!(c.effective_max_length(), Some(8192));
        // No cap: unlimited stays unlimited.
        let mut v = base();
        v["max_length"] = json!(0);
        let mut c = parse(v);
        c.resolve_max_length(None, None, "m");
        assert_eq!(c.effective_max_length(), None);
    }

    #[test]
    fn truncation_side_defaults_to_right() {
        assert_eq!(parse(base()).truncation_side, None);
        assert_eq!(TruncationSide::default(), TruncationSide::Right);
        let mut v = base();
        v["truncation_side"] = json!("left");
        assert_eq!(parse(v).truncation_side, Some(TruncationSide::Left));
    }

    #[test]
    fn zero_or_disabled_means_no_truncation() {
        let mut v = base();
        v["max_length"] = json!(0);
        assert_eq!(parse(v).effective_max_length(), None);
        let mut v = base();
        v["truncation_disabled"] = json!(true);
        assert_eq!(parse(v).effective_max_length(), None);
    }
}
