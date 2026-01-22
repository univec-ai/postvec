// File: engine/src/executors/transformer_sequence_embedding.rs
//!
//! ## Transformer for Sequence Embedding Executor
//!
//! This executor is designed for models that generate sentence embeddings, such as
//! BERT or other encoder-only transformers.
//!
//! It handles the full pipeline from raw text input to a matrix of embeddings.
//!
//! Stages:
//!
//! 1.  **Transformation**: The `transform` method handles tokenization and model
//!     inference, producing a raw `EmbeddingOutput` object containing all output tensors.
//!     This stage is aware of model quantization and adjusts batching logic accordingly.
//!     It uses a local Rayon thread pool for parallel batch processing.
//!
//! 2.  **Exporting**: The `execute` method orchestrates the process. It iterates
//!     through the `executor.outputs` configuration. For each configured output,
//!     it selects the specified tensor (by name, or by index if name is absent),
//!     applies the specified pooling (defaulting to `NoPooling`),
//!     and adds the resulting embeddings to the final output list.
//!     This stage also uses the local Rayon pool for parallel post-processing.
//!
// --- Crate-internal Imports ---
use crate::context::Context;
use crate::error::EngineError;
// Import the output handling components.
use crate::executors::templates::TemplateSet;
use crate::executors::{EmbeddingOutput, Executor, ExecutorOutput, SingleBatchOutput};
// Import the new InputLayout types from the models module
use crate::models::{InputLayoutItem, InputLayoutItemType, ModelConfiguration, QuantizationMode};
use crate::tokenizers::{
    config::Config as TokenizerConfig, new_tokenizer, Tokenizer as TokenizerTrait,
};
use crate::InferenceEngine;
// --- External Imports ---
use base64::Engine as _;
use ndarray::Array2;
// --- Rayon & Concurrency Imports ---
use once_cell::sync::Lazy;
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde_json::{json, Value};
use shared::vectors::{self, FloatMatrix, FloatVector, VectorMathExt};

// --- Constrained Rayon Thread Pool ---

/// A dedicated, constrained Rayon thread pool for this executor.
///
/// We limit the number of threads (e.S., to 4) to avoid **thread oversubscription**.
/// This executor is designed to be run inside a `tokio::task::spawn_blocking` call,
/// which uses Tokio's *own* blocking thread pool.
///
/// If we used Rayon's *global* pool (`.par_iter()` directly), we would have:
/// (Tokio blocking threads) + (Rayon global threads)
///
/// This creates too many threads competing for CPU, leading to high context-switching
/// overhead and *worse* performance.
///
/// By using `EXECUTOR_POOL.install(|| ...)` around our parallel code, we ensure
/// that `.par_iter()` and other parallel operations use this *local, constrained*
/// pool, allowing for safe and efficient nested parallelism.
static EXECUTOR_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        // This is a tunable parameter. 4 is a safe default to get
        // parallelism without overwhelming the system.
        .num_threads(4)
        // Tokenizer regex recursion runs on these threads (`encode_batch`
        // inside `EXECUTOR_POOL.install`); the ~2 MiB std default is the
        // same stack ceiling the embedded runtime raises to 8 MiB for its
        // tokio threads, and it must be raised here too or the mitigation
        // misses the threads that actually tokenize.
        .stack_size(8 * 1024 * 1024)
        .build()
        .expect("Failed to create local Rayon pool for TransformerForSequenceEmbedding executor")
});

// Define a default batch size for processing.
// This can be tuned for performance.
const DEFAULT_BATCH_SIZE: usize = 32;

/// OpenAI-compatible `encoding_format` selector. Default is `Float`.
#[derive(Clone, Copy)]
enum EncodingFormat {
    Float,
    Base64,
}

/// Encode a single `f32` slice into the OpenAI-compatible base64 payload:
/// raw little-endian IEEE-754 bytes, standard base64 (no padding stripped).
fn encode_f32_base64(values: &[f32]) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(&bytes)
}

/// Format the pooled embeddings as JSON: either a list of float arrays, or a
/// list of base64-encoded strings, depending on `format`.
fn encode_vectors(vectors: &[Vec<f32>], format: EncodingFormat) -> Value {
    match format {
        EncodingFormat::Float => json!(vectors),
        EncodingFormat::Base64 => {
            let encoded: Vec<String> = vectors.iter().map(|v| encode_f32_base64(v)).collect();
            json!(encoded)
        }
    }
}

/// The `TransformerForSequenceEmbedding` executor.
///
/// It holds the tokenizer and quantization mode required to
/// preprocess text inputs.
/// Pooling strategy is now defined per-output in the config.
pub struct TransformerForSequenceEmbedding {
    tokenizer: Box<dyn TokenizerTrait>,
    quantization: QuantizationMode,
    /// The specific input layout (e.g., [input_ids, attention_mask, token_type_ids])
    /// required by this model, deserialized from `model.params`.
    input_layout: Option<Vec<InputLayoutItem>>,
    /// Compiled `input_type → Jinja template` map (Task 3). `None` when the
    /// model declares no `executor.params.templates` entry.
    templates: Option<TemplateSet>,
    /// `params.target_dim` (model card) — required to validate Matryoshka
    /// `dimensions` truncation requests.
    target_dim: Option<usize>,
}

impl TransformerForSequenceEmbedding {
    /// The constructor for the executor.
    ///
    /// It initializes the tokenizer and determines the
    /// quantization mode from the model's configuration.
    /// It also parses the optional `input_layout` from the model parameters.
    pub fn new(engine: &InferenceEngine, config: &ModelConfiguration) -> Result<Self, EngineError> {
        // Extract the executor-specific parameters from the model configuration.
        let params = &config.executor.params;
        // Load the tokenizer configuration from the `tokenizer` parameter.
        let tokenizer_config: TokenizerConfig = serde_json::from_value(
            params
                .get("tokenizer")
                .cloned()
                // If the "tokenizer" key is missing, default to a null JSON value,
                // which will cause `from_value` to use the
                // default `TokenizerConfig`.
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|e| {
            EngineError::Configuration(format!("Failed to parse tokenizer configuration: {}", e))
        })?;

        // Resolve the model's directory to find tokenizer assets if relative paths are used.
        let model_dir = engine.get_model_version_directory(&config.name)?;
        // Create the tokenizer instance. `to_str()` is used as the tokenizer constructor
        // expects an `Option<&str>`.
        let tokenizer = new_tokenizer(&tokenizer_config, model_dir.to_str())?;
        // Get the quantization mode from the top-level model configuration.
        let quantization = config.quantization;

        // Deserialize the input_layout from model parameters, if it exists.
        // This uses the top-level `config`, not the executor-specific `params`.
        let input_layout: Option<Vec<InputLayoutItem>> = config
            .params
            .get("input_layout")
            .and_then(|layout_val| serde_json::from_value(layout_val.clone()).ok());

        if let Some(layout) = &input_layout {
            log::info!(
                "Initialized TransformerForSequenceEmbedding with custom input layout ({} inputs).",
                layout.len()
            );
        } else {
            log::info!("No custom input layout found for TransformerForSequenceEmbedding, using default (input_ids, attention_mask).");
        }

        log::info!(
            "Initialized TransformerForSequenceEmbedding with quantization: {:?}",
            quantization
        );

        // Optional input-type templates, e.g. EmbeddingGemma's
        // `search_query` / `search_document` wrappers. Loaded once and reused
        // for every request.
        let templates = TemplateSet::load(params.get("templates"), &model_dir)?;
        if let Some(set) = &templates {
            log::info!(
                "Loaded {} input templates for model '{}': {:?}",
                set.names().len(),
                config.name,
                set.names()
            );
        }

        // Validation bound for the optional `dimensions` (Matryoshka) parameter.
        // Pulled from `params.target_dim` which the hub already populates for
        // embed models (the hosted gateway applies the same rule).
        let target_dim = config
            .params
            .get("target_dim")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        Ok(Self {
            tokenizer,
            quantization,
            input_layout,
            templates,
            target_dim,
        })
    }

    /// Read an optional executor input by `json_key`, returning `None` when the
    /// input is either not wired in the model configuration (older configs that
    /// predate Task 2) or absent / null in the request payload. Errors only on
    /// type mismatch.
    fn optional_input(ctx: &Context, json_key: &str) -> Result<Option<Value>, EngineError> {
        let inputs = &ctx.model_configuration().executor.inputs;
        let Some(idx) = inputs.iter().position(|m| m.json_key == json_key) else {
            return Ok(None);
        };
        match ctx.input_json(idx) {
            Ok(v) => {
                let value = v.as_value();
                if value.is_null() {
                    Ok(None)
                } else {
                    Ok(Some(value.clone()))
                }
            }
            // `input_json` returns `Prediction("Missing required key …")` when
            // a wired key isn't present in JSON payloads, and
            // `Configuration("Attempted to access structured input index …")`
            // when it isn't present in a chained Structured call. Both are
            // "wired but absent" — fall back to None and let callers default.
            Err(EngineError::Prediction(_)) | Err(EngineError::Configuration(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn optional_string(ctx: &Context, json_key: &str) -> Result<Option<String>, EngineError> {
        match Self::optional_input(ctx, json_key)? {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(other) => Err(EngineError::InputTypeError(format!(
                "`{}` must be a string, got {}",
                json_key, other
            ))),
        }
    }

    /// Read the optional `dimensions` (Matryoshka truncation) input.
    ///
    /// Relaxed contract: anything that means "I don't care, give me the full
    /// vector" maps to `None` (no truncation) rather than an error, so callers
    /// can send a fixed payload shape with the field blanked or sentinel-filled:
    /// - absent / null in the payload,
    /// - an empty string,
    /// - a non-positive integer (`0`, `-1`, …).
    ///
    /// The value may arrive as a JSON number (`22`) or as a numeric string
    /// (`"22"`) — payload shapes that quote every field are common — and both
    /// are accepted identically. Genuine type errors (non-empty non-numeric
    /// strings, fractional numbers) still surface so real mistakes aren't
    /// silently swallowed.
    fn optional_dimensions(ctx: &Context, json_key: &str) -> Result<Option<usize>, EngineError> {
        // Map a parsed integer to the relaxed result: non-positive → no
        // truncation, positive → truncate to that many dims.
        let from_int = |v: i64| if v <= 0 { None } else { Some(v as usize) };
        match Self::optional_input(ctx, json_key)? {
            None => Ok(None),
            Some(Value::Number(n)) => match n.as_i64() {
                Some(v) => Ok(from_int(v)),
                // Fractional / out-of-range number — a real type error.
                None => Err(EngineError::InputTypeError(format!(
                    "`{}` must be a positive integer, got {}",
                    json_key, n
                ))),
            },
            Some(Value::String(s)) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    return Ok(None);
                }
                match trimmed.parse::<i64>() {
                    Ok(v) => Ok(from_int(v)),
                    Err(_) => Err(EngineError::InputTypeError(format!(
                        "`{}` must be a positive integer, got '{}'",
                        json_key, s
                    ))),
                }
            }
            Some(other) => Err(EngineError::InputTypeError(format!(
                "`{}` must be a positive integer, got {}",
                json_key, other
            ))),
        }
    }

    /// Apply the optional `input_type` template to the input strings.
    ///
    /// Permissive by design — see docs/openrouter/openrouter-integration.md §3.4:
    /// - missing or empty `input_type` → render as-is (current/legacy path),
    /// - `input_type` set but no templates configured on this model → render
    ///   as-is and log a warning so the operator can spot the misconfiguration,
    /// - `input_type` set but unknown to this model's template set → same:
    ///   render as-is + warn.
    ///
    /// Bulk-embedding workflows hitting a heterogeneous catalogue should not
    /// have to special-case which models understand which `input_type` strings,
    /// and OpenRouter/Cohere keep inventing new ones; liberal acceptance is
    /// what keeps the surface stable.
    fn apply_template(
        &self,
        input_type: Option<&str>,
        texts: Vec<String>,
        model_name: &str,
    ) -> Result<Vec<String>, EngineError> {
        let name = match input_type {
            Some(n) if !n.is_empty() => n,
            _ => return Ok(texts),
        };
        match &self.templates {
            None => {
                log::warn!(
                    "Model '{}' received input_type='{}' but has no templates configured; rendering text as-is",
                    model_name, name
                );
                Ok(texts)
            }
            Some(set) if !set.contains(name) => {
                log::warn!(
                    "Model '{}' has no template for input_type='{}' (known: {:?}); rendering text as-is",
                    model_name,
                    name,
                    set.names()
                );
                Ok(texts)
            }
            Some(set) => set.render(name, &texts),
        }
    }

    /// A lower-level method that performs tokenization and inference, returning raw batch outputs.
    ///
    /// This function separates the core inference logic from the post-processing,
    /// enabling more flexible use cases and cleaner code.
    /// It is now responsible for
    /// handling quantization-specific logic, such as disabling batching for
    /// dynamically quantized models.
    /// It also dynamically builds the input tensor list based on the `input_layout`.
    ///
    /// This method uses the `EXECUTOR_POOL` to process batches in parallel.
    ///
    /// # Arguments
    /// * `ctx` - The execution context, containing the model and input data.
    ///
    /// # Returns
    /// A `Result` containing an `EmbeddingOutput` instance, which is a staging
    /// area for all the raw tensors produced by the model for all batches.
    fn transform(&self, ctx: &Context) -> Result<EmbeddingOutput, EngineError> {
        // Retrieve a thread-safe reference to the model from the context.
        let model = ctx.model_arc()?;
        // Cancellation contract for every query below. The batches run on the
        // dedicated Rayon pool, whose threads have NO ambient tokio context —
        // the bound carries the watchdog runtime explicitly, so the ONNX
        // termination path works from these threads too.
        let bound = ctx.query_bound();
        let raw_sentences: Vec<String> = ctx.input_json(0)?.as_string_or_string_list()?;
        // Optional OpenAI-style knobs — see Task 2 / Task 3.
        let input_type = Self::optional_string(ctx, "input_type")?;
        // Apply input-type template (e.g. "search_query") if one is configured.
        let sentences = self.apply_template(
            input_type.as_deref(),
            raw_sentences,
            ctx.model_configuration().name.as_str(),
        )?;

        // Determine the batch size, crucially checking the quantization mode.
        let batch_size = match self.quantization {
            // For dynamically quantized models, batching can lead to inconsistent embeddings
            // across different calls, as the quantization scale is determined per-batch.
            // We enforce that the entire input is processed as a single batch.
            QuantizationMode::Dynamic => {
                log::warn!(
                    "Model '{}' uses dynamic quantization. All inputs will be processed as a single batch.",
                    model.name()
                );
                sentences.len()
            }
            // For other modes, use the default batch size.
            _ => DEFAULT_BATCH_SIZE,
        };

        let sentences_str: Vec<&str> = sentences.iter().map(AsRef::as_ref).collect();

        // Process sentences in parallel batches using our constrained Rayon pool.
        let batches = EXECUTOR_POOL.install(|| {
            sentences_str
                .par_chunks(batch_size) // Use parallel chunks
                .map(|chunk| {
                    // --- 1. Tokenization ---
                    let encodings = self.tokenizer.encode_batch(chunk)?;
                    let batch_tokens: u32 =
                        encodings.iter().map(|e| e.input_ids.len() as u32).sum();

                    // --- 2. Tensor Creation ---
                    // Extract all potential tensor data from the encodings.
                    let batch_input_ids: Vec<Vec<i64>> =
                        encodings.iter().map(|e| e.input_ids.clone()).collect();
                    let batch_attn_masks: Vec<Vec<i64>> =
                        encodings.iter().map(|e| e.attention_mask.clone()).collect();
                    let batch_token_type_ids: Vec<Vec<i64>> =
                        encodings.iter().map(|e| e.token_type_ids.clone()).collect();

                    // Get the sequence length from the (padded) input_ids of the first item.
                    // In a batch, all sequences are padded to the same length.
                    let seq_len = batch_input_ids.first().map_or(0, |ids| ids.len());
                    // Create a single position ID range: [0, 1, 2, ..., seq_len-1]
                    let position_ids_range: Vec<i64> = (0..seq_len as i64).collect();
                    // Create the batch of position IDs by cloning the range for each item in the batch.
                    // The shape will be [batch_size, seq_len], matching input_ids.
                    let batch_position_ids: Vec<Vec<i64>> =
                        vec![position_ids_range; batch_input_ids.len()];

                    // Convert attention masks to an ndarray::Array2 for pooling.
                    // We must clone `batch_attn_masks` here so it remains available for the
                    // `attn_mask_tensor` if needed by the input_layout.
                    let attention_mask_matrix = if !batch_attn_masks.is_empty() {
                        let rows = batch_attn_masks.len();
                        // Handle potential empty encodings
                        let cols = batch_attn_masks.first().map_or(0, |m| m.len());
                        // Clone `batch_attn_masks` before consuming it with `into_iter`.
                        let flat_masks: Vec<i64> =
                            batch_attn_masks.clone().into_iter().flatten().collect();
                        Array2::from_shape_vec((rows, cols), flat_masks)
                            .map_err(EngineError::Shape)?
                    } else {
                        // Handle empty input case.
                        Array2::zeros((0, 0))
                    };

                    // --- 3. Model Inference ---
                    // Build the input tensors based on the configured layout.
                    let output_tensors = if let Some(layout) = &self.input_layout {
                        // --- Custom Input Layout Logic ---
                        // Lazily create tensors only if they are needed.
                        let mut created_tensors = std::collections::HashMap::new();
                        let mut input_tensors = Vec::with_capacity(layout.len());

                        for item in layout {
                            match item.item_type {
                                InputLayoutItemType::InputIds => {
                                    let tensor = created_tensors
                                        .entry(InputLayoutItemType::InputIds)
                                        .or_insert_with(|| {
                                            vectors::new_2d_tensor_from_i64_vecs(
                                                batch_input_ids.clone(),
                                            )
                                        })
                                        .as_ref()
                                        // Convert `&VectorError` to `EngineError`
                                        .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                                    input_tensors.push(tensor.clone());
                                }
                                InputLayoutItemType::AttentionMask => {
                                    let tensor = created_tensors
                                        .entry(InputLayoutItemType::AttentionMask)
                                        .or_insert_with(|| {
                                            vectors::new_2d_tensor_from_i64_vecs(
                                                batch_attn_masks.clone(),
                                            )
                                        })
                                        .as_ref()
                                        // Convert `&VectorError` to `EngineError`
                                        .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                                    input_tensors.push(tensor.clone());
                                }
                                InputLayoutItemType::TokenTypeIds => {
                                    let tensor = created_tensors
                                        .entry(InputLayoutItemType::TokenTypeIds)
                                        .or_insert_with(|| {
                                            vectors::new_2d_tensor_from_i64_vecs(
                                                batch_token_type_ids.clone(),
                                            )
                                        })
                                        .as_ref()
                                        // Convert `&VectorError` to `EngineError`
                                        .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                                    input_tensors.push(tensor.clone());
                                }
                                InputLayoutItemType::PositionIds => {
                                    // Add logic for the new PositionIds type.
                                    let tensor = created_tensors
                                        .entry(InputLayoutItemType::PositionIds)
                                        .or_insert_with(|| {
                                            // Use the `batch_position_ids` variable we created earlier.
                                            vectors::new_2d_tensor_from_i64_vecs(
                                                batch_position_ids.clone(),
                                            )
                                        })
                                        .as_ref()
                                        // Convert `&VectorError` to `EngineError`
                                        .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                                    input_tensors.push(tensor.clone());
                                }
                            }
                        }
                        model.query_with_deadline(&input_tensors, Some(&bound))?
                    } else {
                        // --- Default (Legacy) Input Layout Logic ---
                        let input_tensor = vectors::new_2d_tensor_from_i64_vecs(batch_input_ids)?;
                        let attn_mask_tensor =
                            vectors::new_2d_tensor_from_i64_vecs(batch_attn_masks)?;
                        model
                            .query_with_deadline(&[input_tensor, attn_mask_tensor], Some(&bound))?
                    };

                    // --- 4. Package Raw Output ---
                    // We now package *all* raw tensors and the attention mask together.
                    Ok(SingleBatchOutput {
                        output_tensors,
                        attention_mask_array: attention_mask_matrix,
                        token_count: batch_tokens,
                    })
                })
                .collect::<Result<Vec<_>, EngineError>>()
        })?; // The '?' operator handles the `Result` from the `install` block.

        Ok(EmbeddingOutput::new(batches))
    }
}

impl Executor for TransformerForSequenceEmbedding {
    /// The main `execute` method that processes an embedding request.
    ///
    /// This method now orchestrates the two-stage process:
    /// 1. Calls `transform` to get the raw, batched model outputs (containing all tensors).
    /// 2. Iterates through the `executor.outputs` configuration *in parallel*
    ///    using the `EXECUTOR_POOL`.
    /// 3. For each configured output, it creates a custom "transformer" closure
    ///    that checks the `pooling_strategy`:
    ///    a. **If `NoPooling` (which is now the default):** It selects the correct
    ///    tensor (by name or index) and returns the raw, unpooled token embeddings.
    ///    The output is a JSON array of 2D matrices.
    ///    b. **Otherwise (Cls, Mean, etc.):** It selects the correct tensor, applies
    ///    the specified pooling *in parallel* over the batches, and normalizes the
    ///    results. The output is a JSON array of 1D vectors.
    /// 4. It collects all processed JSON outputs into a `Structured` result.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // --- 1. Transformation Stage ---
        // Get the raw, batched outputs from the model.
        // This call already uses Rayon internally for batch processing.
        let embedding_output = self.transform(ctx)?;

        // --- 1b. OpenAI-compatible post-pool knobs (Task 2) ---
        // `dimensions` — Matryoshka truncation. Validated against the model's
        // declared target_dim so callers can't ask for more dimensions than
        // the model actually emits.
        let dimensions = Self::optional_dimensions(ctx, "dimensions")?;
        if let (Some(req), Some(tgt)) = (dimensions, self.target_dim) {
            if req > tgt {
                return Err(EngineError::InputTypeError(format!(
                    "dimensions ({}) exceeds model target_dim ({})",
                    req, tgt
                )));
            }
        }
        // `encoding_format` — "float" (default) or "base64". An empty string is
        // treated as absent (default to float) so callers can send a fixed
        // payload shape with the field blanked out rather than omitting it.
        let encoding_format = match Self::optional_string(ctx, "encoding_format")?.as_deref() {
            None | Some("") | Some("float") => EncodingFormat::Float,
            Some("base64") => EncodingFormat::Base64,
            Some(other) => {
                return Err(EngineError::InputTypeError(format!(
                    "encoding_format must be 'float' or 'base64', got '{}'",
                    other
                )));
            }
        };

        // --- 2. Post-processing Stage ---
        // Get the model overview to map layer names to tensor indices.
        let overview = ctx.model_overview()?;
        // Get the list of configured outputs.
        let executor_outputs = &ctx.model_configuration().executor.outputs;

        // Process each configured output in parallel using our constrained Rayon pool.
        let structured_outputs: Vec<Value> = EXECUTOR_POOL.install(|| {
            executor_outputs
                .par_iter() // Use parallel iterator
                .enumerate()
                .map(|(output_index, output_mapping)| {
                    // Determine which tensor to use for this output.
                    // If `layer_name` is provided, find its index by name.
                    let tensor_index = if let Some(layer_name) = &output_mapping.layer_name {
                        overview
                            .outputs
                            .iter()
                            .position(|l| &l.name == layer_name)
                            .ok_or_else(|| {
                                EngineError::Configuration(format!(
                                    "Output layer '{}' defined in
 executor config not found in model overview.",
                                    layer_name
                                ))
                            })?
                    } else {
                        // If no `layer_name` is given, default to the mapping's index.
                        output_index
                    };

                    // Get the pooling strategy.
                    // If the `pooling_strategy` key is missing or null,
                    // `unwrap_or_default()` will now call `PoolingStrategy::default()`,
                    // which we changed to `PoolingStrategy::NoPooling`.
                    let pooling_strategy = output_mapping
                        .pooling_strategy
                        .clone()
                        .unwrap_or_default();

                    // Whether to L2-normalize the output embeddings after pooling.
                    let normalize = output_mapping.normalize;

                    // Create the post-processing pipeline for this specific output.
                    // This closure returns a `Result<Value, EngineError>` because its output
                    // format (1D vectors vs 2D matrices) is conditional.
                    let transformer =
                        |batches: &[SingleBatchOutput]| -> Result<Value, EngineError> {
                            // We check if the strategy is *not* `NoPooling`.
                            if pooling_strategy != crate::pooling::PoolingStrategy::NoPooling {
                                // --- Pooled and Normalized Logic ---
                                // This branch executes for `Cls`, `Mean`, `Splade`, `LastToken`.
                                // We process the batches in parallel using the pool.
                                let all_embeddings: Vec<FloatVector> = batches
                                    .par_iter() // Use parallel iterator over batches
                                    .map(|batch| {
                                        // 1. Pool the batch output.
                                        let pooled =
                                            batch.pool(pooling_strategy.clone(), tensor_index)?;

                                        // 2. Optionally normalize each embedding in the pooled batch.
                                        let embeddings: Vec<FloatVector> = if normalize {
                                            pooled
                                                .rows()
                                                .into_iter()
                                                .map(|row| row.to_owned().normalized())
                                                .collect()
                                        } else {
                                            pooled
                                                .rows()
                                                .into_iter()
                                                .map(|row| row.to_owned())
                                                .collect()
                                        };

                                        Ok(embeddings)
                                    })
                                    // Collect the results from parallel processing.
                                    .collect::<Result<Vec<Vec<FloatVector>>, EngineError>>()?
                                    .into_iter()
                                    .flatten()
                                    .collect();

                                // 3. Optional Matryoshka truncation (with renormalisation
                                //    when the configured output is normalised — truncating a
                                //    unit-norm vector breaks unit-norm).
                                let embeddings_as_vecs: Vec<Vec<f32>> = all_embeddings
                                    .into_iter()
                                    .map(|v| {
                                        let mut out: Vec<f32> = v.to_vec();
                                        if let Some(d) = dimensions {
                                            if d < out.len() {
                                                out.truncate(d);
                                                if normalize {
                                                    let norm: f32 = out
                                                        .iter()
                                                        .map(|x| x * x)
                                                        .sum::<f32>()
                                                        .sqrt();
                                                    if norm > 0.0 {
                                                        for x in out.iter_mut() {
                                                            *x /= norm;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        out
                                    })
                                    .collect();
                                // 4. Optional base64 encoding (OpenAI parity).
                                Ok(encode_vectors(&embeddings_as_vecs, encoding_format))
                            } else {
                                // --- Unpooled (Raw) Tensor Logic ---
                                // This branch executes if pooling_strategy was "no-pooling" or absent.
                                // We return the raw token embeddings for each sentence.
                                // The result will be a list of 2D matrices.
                                // We process the batches in parallel using the pool.
                                let all_unpooled_tensors: Vec<FloatMatrix> = batches
                                    .par_iter() // Use parallel iterator over batches
                                    .map(|batch| {
                                        // 1. Get the raw output tensor for this batch.
                                        let output_tensor =
                                            batch.output_tensors.get(tensor_index).ok_or_else(
                                                || {
                                                    EngineError::Prediction(format!(
                                                        "Model output tensor at index {} not found for unpooled output.",
                                                        tensor_index
                                                    ))
                                                },
                                            )?;
                                        // 2. Convert it to a 3D cube [batch_size, seq_len, hidden_size].
                                        let tensor_cube = output_tensor.clone().as_float_cube()?;
                                        // 3. Split the 3D cube into a Vec of 2D matrices (one per sentence).
                                        let sentence_matrices: Vec<FloatMatrix> =
                                            tensor_cube.outer_iter().map(|m| m.to_owned()).collect();
                                        Ok(sentence_matrices)
                                    })
                                    // Collect the results from parallel processing.
                                    .collect::<Result<Vec<Vec<FloatMatrix>>, EngineError>>()?
                                    .into_iter()
                                    .flatten()
                                    .collect();

                                // 4. Final Formatting (for this output)
                                // Convert `Vec<FloatMatrix>` to `Vec<Vec<Vec<f32>>>` for JSON,
                                // truncating along the hidden dimension when `dimensions` is set.
                                // `NoPooling` typically marks pre-pooled sentence vectors that
                                // arrive as 1×D rows; truncating the last axis is the same
                                // operation in both shapes.
                                let unpooled_as_vecs: Vec<Vec<Vec<f32>>> = all_unpooled_tensors
                                    .into_iter()
                                    .map(|matrix| {
                                        matrix
                                            .rows()
                                            .into_iter()
                                            .map(|row| {
                                                let mut v: Vec<f32> = row.to_vec();
                                                if let Some(d) = dimensions {
                                                    if d < v.len() {
                                                        v.truncate(d);
                                                    }
                                                }
                                                v
                                            })
                                            .collect()
                                    })
                                    .collect();

                                // base64 for raw token tensors: encode the flattened f32 buffer
                                // per row. The OpenAI wire shape is one base64 string per data
                                // item, regardless of underlying tensor rank.
                                match encoding_format {
                                    EncodingFormat::Float => Ok(json!(unpooled_as_vecs)),
                                    EncodingFormat::Base64 => {
                                        let encoded: Vec<String> = unpooled_as_vecs
                                            .iter()
                                            .map(|matrix| {
                                                let flat: Vec<f32> =
                                                    matrix.iter().flatten().copied().collect();
                                                encode_f32_base64(&flat)
                                            })
                                            .collect();
                                        Ok(json!(encoded))
                                    }
                                }
                            }
                        };

                    // Apply the transformer to get the final JSON value for this output.
                    let final_json_output = embedding_output.export_with_transformer(transformer)?;
                    // The transformer closure now returns the final JSON value directly,
                    // so we just return it.
                    Ok(final_json_output)
                })
                .collect::<Result<Vec<_>, EngineError>>() // Collect parallel results
        })?; // The '?' operator handles the `Result` from the `install` block.

        // Return the embeddings as a structured output.
        // The order of values in the `Vec` matches the order of `executor.outputs`.
        // Return the embeddings as a structured output with usage metadata.
        // The order of values in the `Vec` matches the order of `executor.outputs`.
        let total_tokens = embedding_output.total_tokens();
        Ok(ExecutorOutput::StructuredWithUsage {
            outputs: structured_outputs,
            usage: super::ExecutionMetadata {
                prompt_tokens: total_tokens,
                completion_tokens: 0,
                total_tokens,
            },
        })
    }
}
