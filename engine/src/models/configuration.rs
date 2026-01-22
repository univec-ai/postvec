// File: engine/src/models/configuration.rs
//!
//! //! ## Model Configuration
//!
//! This module defines the structures for configuring machine learning models,
//!
//! mirroring the setup from the Go implementation. It includes types for
//! defining model inputs, outputs, backends, and other parameters.
//!
//! `serde` is
//! used for robust serialization and deserialization from formats like JSON.
use super::error::ModelError;
use crate::pooling::PoolingStrategy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
/// Represents the backend technology used to run the model.
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
/// Represents the desired execution provider for ONNX Runtime.
/// This allows specifying whether to use CPU, CUDA, TensorRT, etc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionProvider {
    Cpu,
    Cuda,
    TensorRt,
}
/// Enum for the quantization mode of a model.
/// This is crucial for determining
/// the correct execution strategy, especially regarding batching.
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
/// Defines the type of a model input layer, as specified in the `input_layout`.
/// This enum allows the executor to correctly map tokenizer outputs to model inputs.
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
    /// Corresponds to the `position_ids` tensor.
    /// This is required by models like Qwen2 that use RoPE embeddings.
    #[serde(rename = "position_ids")]
    PositionIds,
}
/// Represents a single item in the `input_layout` array in the model configuration.
/// This defines the expected order and type of inputs for the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLayoutItem {
    /// The type of input this layer expects (e.g., "input_ids").
    #[serde(rename = "type")]
    pub item_type: InputLayoutItemType,
}
/// Allows the `ModelBackend` enum to be easily converted to a string.
impl std::fmt::Display for ModelBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Use serde_json to ensure the string representation matches the serialization format.
        let s = serde_json::to_string(self)
            .unwrap_or_else(|_| "\"generic\"".to_string())
            .replace('\"', "");
        write!(f, "{}", s)
    }
}
/// Defines an overview of a model layer, including its name, shape, and data type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerOverview {
    pub name: String,
    pub shape: Vec<i64>,
    #[serde(rename = "type")]
    pub data_type: i32,
}
/// Provides a summary of the model's architecture, detailing its input
/// and output layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelOverview {
    pub inputs: Vec<LayerOverview>,
    pub outputs: Vec<LayerOverview>,
}
/// Represents a mapping from a JSON key in the API payload to a model input.
/// This decouples the public API contract from the internal model layer names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InputMapping {
    /// The key to look for in the incoming JSON payload.
    pub json_key: String,
}
/// Represents a mapping from a model output to a JSON key in the API response.
/// This decouples the public API contract from the internal model layer names
/// and allows specifying output-specific post-processing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct OutputMapping {
    /// The key to use for this output in the outgoing JSON response.
    pub json_key: String,
    /// The name of the model's output layer to read from.
    /// If `None`, the executor will default to the first output tensor (index 0).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    /// The pooling strategy to apply to this specific output layer.
    /// This is optional and only relevant for executors that produce sequence embeddings.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pooling_strategy: Option<PoolingStrategy>,
    /// Whether to L2-normalize the output embeddings after pooling.
    /// Defaults to `false`. Set to `true` for models that include a Normalize
    /// step in their sentence-transformers pipeline (e.g., `2_Normalize` module).
    #[serde(default)]
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub normalize: bool,
}
/// Represents the 'executor' block in the model configuration.
// This structure holds all configuration specific to the executor, separating it
// from the model's core configuration (like backend and file path).
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
/// A comprehensive structure for defining the configuration of a model.
/// It uses `serde` attributes to control JSON serialization, such as skipping
/// empty fields for a cleaner output.
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
    // The executor configuration is now a distinct, nested structure.
    #[serde(default)]
    pub executor: ExecutorConfiguration,
    #[serde(default)]
    pub dependencies: Vec<String>,
}
impl ModelConfiguration {
    /// Loads a model configuration from a JSON file.
    ///
    /// # Arguments
    /// * `descriptor_file` - The path to the JSON configuration file.
    ///
    /// # Returns
    /// A `Result` containing the `ModelConfiguration` or a `ModelError`.
    pub fn from_file<P: AsRef<Path>>(descriptor_file: P) -> Result<Self, ModelError> {
        let path = descriptor_file.as_ref();
        let file_content = fs::read_to_string(path)?;
        let config: ModelConfiguration = serde_json::from_str(&file_content)?;
        Ok(config)
    }
    /// Merges another `ModelConfiguration` into the current one.
    ///
    /// This implementation performs a field-by-field merge.
    /// If a field in
    /// `other` is present (`Some` or not empty), it overwrites the corresponding
    /// field in `self`.
    /// HashMap `params` are merged key by key.
    pub fn override_with(&mut self, other: &Self) {
        self.name = other.name.clone();
        self.enabled = other.enabled;
        self.backend = other.backend.clone();
        if other.file_path.is_some() {
            self.file_path = other.file_path.clone();
        }
        // Merge model-specific parameters.
        if !other.params.is_empty() {
            for (key, value) in &other.params {
                self.params.insert(key.clone(), value.clone());
            }
        }
        // The entire executor configuration from `other` replaces the current one.
        // A more complex, key-by-key merge could be implemented here if needed.
        self.executor = other.executor.clone();
        self.dependencies = other.dependencies.clone();
    }
}
