//! Model configuration: backends, executor mappings, inputs/outputs and params.
//! Parsed from each model's `ninference.hub.json`.
use super::error::ModelError;
use crate::pooling::PoolingStrategy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
/// Backend that runs the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ModelBackend {
    #[default]
    Generic,
    OnnxRuntime,
    Candle,
    Catboost,
    LlamaCpp,
    LlamaCppEmbedding,
}
/// ONNX Runtime execution provider (CPU, CUDA, TensorRT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionProvider {
    Cpu,
    Cuda,
    TensorRt,
}
/// Quantization mode. Affects whether batches can share a scaling factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum QuantizationMode {
    /// No quantization.
    #[default]
    None,
    /// Static quantization, where scaling factors are fixed. Compatible with batching.
    #[serde(rename = "static")]
    Static,
    /// Dynamic quantization, where scaling factors are computed per-batch.
    /// This mode is generally incompatible with batching across different inference calls,
    /// as the embeddings would not be comparable.
    #[serde(rename = "dynamic")]
    Dynamic,
}
/// Tokenizer output mapped onto a model input layer (`input_layout`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InputLayoutItemType {
    /// Corresponds to the `input_ids` tensor.
    #[serde(rename = "input_ids")]
    InputIds,
    /// Corresponds to the `attention_mask` tensor.
    #[serde(rename = "attention_mask")]
    AttentionMask,
    /// Corresponds to the `token_type_ids` tensor.
    #[serde(rename = "token_type_ids")]
    TokenTypeIds,
    /// `position_ids` tensor (RoPE models such as Qwen2).
    #[serde(rename = "position_ids")]
    PositionIds,
}
/// One entry in `input_layout`: order and type of a model input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLayoutItem {
    /// The type of input this layer expects (e.g., "input_ids").
    #[serde(rename = "type")]
    pub item_type: InputLayoutItemType,
}

impl std::fmt::Display for ModelBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Match the kebab-case JSON form.
        let s = serde_json::to_string(self)
            .unwrap_or_else(|_| "\"generic\"".to_string())
            .replace('\"', "");
        write!(f, "{}", s)
    }
}
/// Name, shape and data type of one input or output layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerOverview {
    pub name: String,
    pub shape: Vec<i64>,
    #[serde(rename = "type")]
    pub data_type: i32,
}
/// Input and output layers of a loaded model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelOverview {
    pub inputs: Vec<LayerOverview>,
    pub outputs: Vec<LayerOverview>,
}
/// Maps a JSON request key onto a model input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InputMapping {
    /// The key to look for in the incoming JSON payload.
    pub json_key: String,
}
/// Maps a model output onto a JSON response key, with optional pooling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutputMapping {
    /// The key to use for this output in the outgoing JSON response.
    pub json_key: String,
    /// Output layer name. `None` means the first tensor (index 0).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// Pooling for sequence-embedding executors. Optional.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pooling_strategy: Option<PoolingStrategy>,
    /// L2-normalize after pooling. Default false; set true for sentence-transformers
    /// pipelines that include a `2_Normalize` step.
    #[serde(default)]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub normalize: bool,
}
/// The `executor` block in `ninference.hub.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ExecutorConfiguration {
    /// The key that determines which executor implementation to use (e.g., "passthru").
    #[serde(default)]
    pub key: String,
    /// A list defining how to map incoming JSON fields to the model's input tensors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputMapping>,
    /// A list defining how to map the model's output tensors to the outgoing JSON response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputMapping>,
    /// A key-value map for executor-specific parameters.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, Value>,
}
/// Full model descriptor (`ninference.hub.json`). Empty collections are
/// omitted on serialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelConfiguration {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub backend: ModelBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// A list of execution providers for ONNX Runtime to attempt to use, in order.
    /// If empty, a sensible default (usually CPU) will be used.
    /// Example: `["cuda", "cpu"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_providers: Vec<ExecutionProvider>,
    /// The quantization mode of the model.
    /// Defaults to `None`.
    #[serde(default)]
    pub quantization: QuantizationMode,
    /// A key-value map for model-specific parameters, such as `input_layout`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub params: HashMap<String, Value>,

    #[serde(default)]
    pub executor: ExecutorConfiguration,
    #[serde(default)]
    pub dependencies: Vec<String>,
}
impl ModelConfiguration {
    /// Parse a `ninference.hub.json`.
    pub fn from_file<P: AsRef<Path>>(descriptor_file: P) -> Result<Self, ModelError> {
        let path = descriptor_file.as_ref();
        let file_content = fs::read_to_string(path)?;
        let config: ModelConfiguration = serde_json::from_str(&file_content)?;
        Ok(config)
    }
    /// Overlay `other` onto `self`. Present fields replace; `params` merge
    /// key by key; the executor block is replaced whole.
    pub fn override_with(&mut self, other: &Self) {
        self.name = other.name.clone();
        self.enabled = other.enabled;
        self.backend = other.backend.clone();
        if other.file_path.is_some() {
            self.file_path = other.file_path.clone();
        }

        if !other.params.is_empty() {
            for (key, value) in &other.params {
                self.params.insert(key.clone(), value.clone());
            }
        }

        self.executor = other.executor.clone();
        self.dependencies = other.dependencies.clone();
    }
}
