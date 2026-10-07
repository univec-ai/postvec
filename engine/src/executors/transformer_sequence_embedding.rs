//! Text to sentence embeddings: tokenize, run the encoder, pool.
//!
//! `transform` tokenizes and queries (quantization-aware batching) on a
//! local Rayon pool. `execute` then pools each configured output.

use crate::context::Context;
use crate::error::EngineError;
use crate::executors::templates::TemplateSet;
use crate::executors::{EmbeddingOutput, Executor, ExecutorOutput, SingleBatchOutput};
use crate::models::{InputLayoutItem, InputLayoutItemType, ModelConfiguration, QuantizationMode};
use crate::tokenizers::{
    config::Config as TokenizerConfig, new_tokenizer, Tokenizer as TokenizerTrait,
    TransformerEncodingsWithPosition,
};
use crate::InferenceEngine;
use base64::Engine as _;
use ndarray::Array2;
use once_cell::sync::Lazy;
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde_json::{json, Value};
use shared::vectors::{self, FloatMatrix, FloatVector, VectorMathExt};

/// Local Rayon pool so `.par_iter()` does not pile onto Tokio's blocking
/// threads (this executor already runs inside `spawn_blocking`).
static EXECUTOR_POOL: Lazy<ThreadPool> = Lazy::new(|| {
    ThreadPoolBuilder::new()
        .num_threads(4)
        // Tokenizer regex recursion runs here (`encode_batch` inside
        // `install`). Default stack is ~2 MiB; the embedded runtime raises
        // tokio threads to 8 MiB, and these threads must match or they
        // still overflow.
        .stack_size(8 * 1024 * 1024)
        .build()
        .expect("Failed to create local Rayon pool for TransformerForSequenceEmbedding executor")
});

const DEFAULT_BATCH_SIZE: usize = 32;

/// Most padded tokens one inference call may hold. Default is 32 texts
/// of 512 tokens. Longer inputs are split so the call stays that size.
const DEFAULT_MAX_BATCH_TOKENS: usize =
    DEFAULT_BATCH_SIZE * crate::tokenizers::config::DEFAULT_MAX_LENGTH;

/// Cap on rows times length times length for one call. Default is 32
/// times 512 squared. The model builds a full length-by-length score
/// matrix, so two long texts do not share a call. A longer text runs
/// by itself.
const DEFAULT_MAX_BATCH_ATTENTION: usize = DEFAULT_BATCH_SIZE
    * crate::tokenizers::config::DEFAULT_MAX_LENGTH
    * crate::tokenizers::config::DEFAULT_MAX_LENGTH;

/// The two caps for one call: token count, and rows times length squared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BatchBudget {
    tokens: usize,
    attention: usize,
}

impl BatchBudget {
    const DEFAULT: Self = Self {
        tokens: DEFAULT_MAX_BATCH_TOKENS,
        attention: DEFAULT_MAX_BATCH_ATTENTION,
    };

    /// Does a batch of `rows` padded to `longest` tokens fit?
    fn fits(&self, rows: usize, longest: usize) -> bool {
        let rows = rows as u128;
        let longest = longest as u128;
        rows * longest <= self.tokens as u128 && rows * longest * longest <= self.attention as u128
    }
}

/// Number of real (non-padding) tokens in an encoding.
fn real_length(e: &TransformerEncodingsWithPosition) -> usize {
    if e.attention_mask.is_empty() {
        e.input_ids.len()
    } else {
        e.attention_mask.iter().filter(|&&m| m != 0).count()
    }
}

/// True when all padding sits after the real tokens (the engine's tokenizers pad
/// right), which is what makes trimming the tail safe.
fn is_right_padded(e: &TransformerEncodingsWithPosition) -> bool {
    match e.attention_mask.iter().position(|&m| m == 0) {
        None => true,
        Some(first_pad) => e.attention_mask[first_pad..].iter().all(|&m| m == 0),
    }
}

/// Split `lengths` into contiguous ranges that each fit `budget`.
/// A row that does not fit on its own gets a range of its own.
fn plan_lengths(lengths: &[usize], budget: &BatchBudget) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let (mut start, mut longest) = (0, 0);
    for (i, &len) in lengths.iter().enumerate() {
        let candidate = longest.max(len);
        if i > start && !budget.fits(i - start + 1, candidate) {
            ranges.push(start..i);
            start = i;
            longest = len;
        } else {
            longest = candidate;
        }
    }
    if start < lengths.len() {
        ranges.push(start..lengths.len());
    }
    ranges
}

/// Sub-batch plan for one tokenized chunk. A chunk that already fits, or
/// whose padding is not right-sided (the only layout that can be trimmed),
/// stays one batch.
// One range is the whole chunk. The vec is intentional: CI denies the
// single-range lint.
#[allow(clippy::single_range_in_vec_init)]
fn plan_sub_batches(
    encodings: &[TransformerEncodingsWithPosition],
    budget: &BatchBudget,
) -> Vec<std::ops::Range<usize>> {
    let padded = encodings.iter().map(|e| e.input_ids.len()).max().unwrap_or(0);
    if budget.fits(encodings.len(), padded) || !encodings.iter().all(is_right_padded) {
        return vec![0..encodings.len()];
    }
    let lengths: Vec<usize> = encodings.iter().map(real_length).collect();
    plan_lengths(&lengths, budget)
}

/// Drop the right padding a sub-batch no longer needs (its longest row may be much
/// shorter than the chunk's). A no-op when the rows are already that long.
fn trim_right_padding(
    mut encodings: Vec<TransformerEncodingsWithPosition>,
) -> Vec<TransformerEncodingsWithPosition> {
    let longest = encodings.iter().map(real_length).max().unwrap_or(0);
    for e in encodings.iter_mut() {
        if e.input_ids.len() > longest && is_right_padded(e) {
            e.input_ids.truncate(longest);
            e.attention_mask.truncate(longest);
            e.token_type_ids.truncate(longest);
        }
    }
    encodings
}

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

/// Sentence-embedding executor. Holds the tokenizer, quantization mode
/// and per-model input layout. Pooling is configured per output.
pub struct TransformerForSequenceEmbedding {
    tokenizer: Box<dyn TokenizerTrait>,
    quantization: QuantizationMode,
    /// The specific input layout (e.g., [input_ids, attention_mask, token_type_ids])
    /// required by this model, deserialized from `model.params`.
    input_layout: Option<Vec<InputLayoutItem>>,
    /// Compiled `input_type` to Jinja template map. `None` when the model
    /// declares no `executor.params.templates` entry.
    templates: Option<TemplateSet>,
    /// `params.target_dim`. Upper bound for a `dimensions` truncation.
    target_dim: Option<usize>,
    /// Memory budget for one inference call (`max_batch_tokens`, `max_batch_attention`).
    budget: BatchBudget,
    /// Only one over-long text runs at a time.
    heavy_lane: std::sync::Mutex<()>,
    /// The truncation length the tokenizer applies and where it came from.
    input_limit: Option<(usize, crate::tokenizers::config::MaxLengthSource)>,
}

impl TransformerForSequenceEmbedding {
    /// Build the executor from the model configuration.
    pub fn new(engine: &InferenceEngine, config: &ModelConfiguration) -> Result<Self, EngineError> {
        let params = &config.executor.params;
        let mut tokenizer_config: TokenizerConfig = serde_json::from_value(
            params
                .get("tokenizer")
                .cloned()
                // A missing key deserializes as the default tokenizer config.
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|e| {
            EngineError::Configuration(format!("Failed to parse tokenizer configuration: {}", e))
        })?;

        let model_dir = engine.get_model_version_directory(&config.name)?;
        // `to_str()` because `new_tokenizer` takes `Option<&str>`.
        // Truncation length: explicit `max_length`, else `params.sequence_len`.
        let (_, limit_source) = tokenizer_config.resolve_max_length(
            config.sequence_len(),
            engine.sequence_len_cap(),
            &config.name,
        );
        let input_limit = tokenizer_config
            .effective_max_length()
            .map(|n| (n, limit_source));
        let tokenizer = new_tokenizer(&tokenizer_config, model_dir.to_str())?;
        let quantization = config.quantization;

        // `input_layout` lives on the model params, not the executor block.
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

        let templates = TemplateSet::load(params.get("templates"), &model_dir)?;
        if let Some(set) = &templates {
            log::info!(
                "Loaded {} input templates for model '{}': {:?}",
                set.names().len(),
                config.name,
                set.names()
            );
        }

        let target_dim = config
            .params
            .get("target_dim")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        // `max_batch_tokens` / `max_batch_attention`. A long input shrinks the batch.
        let positive = |key: &str, default: usize| {
            params
                .get(key)
                .and_then(|v| v.as_u64())
                .filter(|&n| n > 0)
                .map(|n| n as usize)
                .unwrap_or(default)
        };
        let budget = BatchBudget {
            tokens: positive("max_batch_tokens", BatchBudget::DEFAULT.tokens),
            attention: positive("max_batch_attention", BatchBudget::DEFAULT.attention),
        };

        Ok(Self {
            tokenizer,
            quantization,
            input_layout,
            templates,
            target_dim,
            budget,
            heavy_lane: std::sync::Mutex::new(()),
            input_limit,
        })
    }

    /// Optional executor input by `json_key`.
    ///
    /// `None` when the input is not in the model configuration, or when a
    /// wired key is missing or null. A present value of the wrong type is
    /// an error.
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
            // A missing JSON key is `Prediction`. A missing structured input
            // is `Configuration`. Both mean the caller left the field out.
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

    /// Optional `dimensions` truncation.
    ///
    /// Missing, null, an empty string or a non-positive integer means no
    /// truncation, so a caller can send a fixed payload with the field
    /// blank. A JSON number and a numeric string are the same value.
    /// A non-numeric string or a fractional number is a type error.
    fn optional_dimensions(ctx: &Context, json_key: &str) -> Result<Option<usize>, EngineError> {
        // Non-positive means no truncation.
        let from_int = |v: i64| if v <= 0 { None } else { Some(v as usize) };
        match Self::optional_input(ctx, json_key)? {
            None => Ok(None),
            Some(Value::Number(n)) => match n.as_i64() {
                Some(v) => Ok(from_int(v)),
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

    /// Wrap texts with the `input_type` template when this model has one.
    /// An unknown type is left as-is and logged. The caller does not have
    /// to know which models define which names.
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

    /// Tokenize the inputs and run the model. Returns one raw output per
    /// sub-batch. Batches run on `EXECUTOR_POOL`.
    fn transform(&self, ctx: &Context) -> Result<EmbeddingOutput, EngineError> {
        let model = ctx.model_arc()?;
        // Rayon threads have no tokio context. The bound carries the
        // watchdog runtime so a cancelled query still stops the ONNX run.
        let bound = ctx.query_bound();
        let raw_sentences: Vec<String> = ctx.input_json(0)?.as_string_or_string_list()?;
        let input_type = Self::optional_string(ctx, "input_type")?;
        let sentences = self.apply_template(
            input_type.as_deref(),
            raw_sentences,
            ctx.model_configuration().name.as_str(),
        )?;

        let batch_size = match self.quantization {
            // Scales are computed over the whole input tensor, padding
            // included, so a text's vector would change with the other texts
            // in its batch.
            // One text per batch keeps the vector stable.
            QuantizationMode::Dynamic => 1,
            _ => DEFAULT_BATCH_SIZE,
        };

        let sentences_str: Vec<&str> = sentences.iter().map(AsRef::as_ref).collect();

        let batches = EXECUTOR_POOL.install(|| {
            sentences_str
                .par_chunks(batch_size)
                .map(|chunk| {
                    let encodings = self.tokenizer.encode_batch(chunk)?;

                    // A chunk is padded to its longest text, so one long row
                    // pads every row. Split into ordered groups that fit the
                    // budget, then drop padding the group does not need.
                    let plan = plan_sub_batches(&encodings, &self.budget);
                    let mut remaining = encodings.into_iter();
                    plan.into_iter()
                        .map(|range| {
                            let sub = trim_right_padding(
                                remaining.by_ref().take(range.len()).collect(),
                            );
                            let longest = sub.iter().map(|e| e.input_ids.len()).max().unwrap_or(0);
                            // A group that still exceeds the budget is one long
                            // text. Hold the lock so only one of those runs.
                            let _heavy = if self.budget.fits(sub.len(), longest) {
                                None
                            } else {
                                Some(self.heavy_lane.lock().unwrap_or_else(|p| p.into_inner()))
                            };
                            self.run_batch(model, &bound, &sub)
                        })
                        .collect::<Result<Vec<_>, EngineError>>()
                })
                .collect::<Result<Vec<Vec<_>>, EngineError>>()
        })?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        Ok(EmbeddingOutput::new(batches))
    }

    /// Run one padded group and return its raw outputs.
    fn run_batch(
        &self,
        model: &std::sync::Arc<dyn crate::models::Model>,
        bound: &crate::models::QueryBound,
        encodings: &[TransformerEncodingsWithPosition],
    ) -> Result<SingleBatchOutput, EngineError> {
        // Count real tokens. Padding that lines the rows up is not input.
        let batch_tokens: u32 = encodings.iter().map(|e| real_length(e) as u32).sum();

        let batch_input_ids: Vec<Vec<i64>> =
            encodings.iter().map(|e| e.input_ids.clone()).collect();
        let batch_attn_masks: Vec<Vec<i64>> =
            encodings.iter().map(|e| e.attention_mask.clone()).collect();
        let batch_token_type_ids: Vec<Vec<i64>> =
            encodings.iter().map(|e| e.token_type_ids.clone()).collect();

        let seq_len = batch_input_ids.first().map_or(0, |ids| ids.len());
        let position_ids_range: Vec<i64> = (0..seq_len as i64).collect();
        let batch_position_ids: Vec<Vec<i64>> =
            vec![position_ids_range; batch_input_ids.len()];

        // Pooling needs the mask as an array, and the model may need the
        // same rows, so they are cloned.
        let attention_mask_matrix = if !batch_attn_masks.is_empty() {
            let rows = batch_attn_masks.len();
            let cols = batch_attn_masks.first().map_or(0, |m| m.len());
            let flat_masks: Vec<i64> =
                batch_attn_masks.clone().into_iter().flatten().collect();
            Array2::from_shape_vec((rows, cols), flat_masks)
                .map_err(EngineError::Shape)?
        } else {
            Array2::zeros((0, 0))
        };

        let output_tensors = if let Some(layout) = &self.input_layout {
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
                            .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                        input_tensors.push(tensor.clone());
                    }
                    InputLayoutItemType::PositionIds => {
                        let tensor = created_tensors
                            .entry(InputLayoutItemType::PositionIds)
                            .or_insert_with(|| {
                                vectors::new_2d_tensor_from_i64_vecs(
                                    batch_position_ids.clone(),
                                )
                            })
                            .as_ref()
                            .map_err(|e| EngineError::InputTypeError(e.to_string()))?;
                        input_tensors.push(tensor.clone());
                    }
                }
            }
            model.query_with_deadline(&input_tensors, Some(bound))?
        } else {
            let input_tensor = vectors::new_2d_tensor_from_i64_vecs(batch_input_ids)?;
            let attn_mask_tensor =
                vectors::new_2d_tensor_from_i64_vecs(batch_attn_masks)?;
            model
                .query_with_deadline(&[input_tensor, attn_mask_tensor], Some(bound))?
        };

        Ok(SingleBatchOutput {
            output_tensors,
            attention_mask_array: attention_mask_matrix,
            token_count: batch_tokens,
        })
    }
}

impl Executor for TransformerForSequenceEmbedding {
    fn max_input_tokens(&self) -> Option<(usize, crate::tokenizers::config::MaxLengthSource)> {
        self.input_limit
    }

    /// Run the model, then pool each configured output.
    ///
    /// An explicit `no-pooling` output returns the token matrices. Any other
    /// strategy, including a missing one, returns one vector per input.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        let embedding_output = self.transform(ctx)?;

        // `dimensions` truncates each vector. It cannot exceed `target_dim`.
        let dimensions = Self::optional_dimensions(ctx, "dimensions")?;
        if let (Some(req), Some(tgt)) = (dimensions, self.target_dim) {
            if req > tgt {
                return Err(EngineError::InputTypeError(format!(
                    "dimensions ({}) exceeds model target_dim ({})",
                    req, tgt
                )));
            }
        }
        // Empty `encoding_format` means float, so a fixed payload can leave it blank.
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

        let overview = ctx.model_overview()?;
        let executor_outputs = &ctx.model_configuration().executor.outputs;

        let structured_outputs: Vec<Value> = EXECUTOR_POOL.install(|| {
            executor_outputs
                .par_iter()
                .enumerate()
                .map(|(output_index, output_mapping)| {
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
                        output_index
                    };

                    // A missing strategy is `Cls`, which passes an already-pooled
                    // matrix through. Token matrices come back only for `no-pooling`.
                    let pooling_strategy = output_mapping
                        .pooling_strategy
                        .clone()
                        .unwrap_or_default();

                    let normalize = output_mapping.normalize;

                    let transformer =
                        |batches: &[SingleBatchOutput]| -> Result<Value, EngineError> {
                            if pooling_strategy != crate::pooling::PoolingStrategy::NoPooling {
                                let all_embeddings: Vec<FloatVector> = batches
                                    .par_iter()
                                    .map(|batch| {
                                        let pooled =
                                            batch.pool(pooling_strategy.clone(), tensor_index)?;

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
                                    .collect::<Result<Vec<Vec<FloatVector>>, EngineError>>()?
                                    .into_iter()
                                    .flatten()
                                    .collect();

                                let embeddings_as_vecs: Vec<Vec<f32>> = all_embeddings
                                    .into_iter()
                                    .map(|v| {
                                        let mut out: Vec<f32> = v.to_vec();
                                        if let Some(d) = dimensions {
                                            if d < out.len() {
                                                // Cutting a unit vector short breaks the unit
                                                // length. Re-normalise when the output asks
                                                // for it, or when the vector already was unit
                                                // length. Some graphs normalise inside, such
                                                // as embeddinggemma-300m.
                                                let was_unit = (out.iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs()
                                                    < 1e-3;
                                                out.truncate(d);
                                                if normalize || was_unit {
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
                                Ok(encode_vectors(&embeddings_as_vecs, encoding_format))
                            } else {
                                let all_unpooled_tensors: Vec<FloatMatrix> = batches
                                    .par_iter()
                                    .map(|batch| {
                                        let output_tensor =
                                            batch.output_tensors.get(tensor_index).ok_or_else(
                                                || {
                                                    EngineError::Prediction(format!(
                                                        "Model output tensor at index {} not found for unpooled output.",
                                                        tensor_index
                                                    ))
                                                },
                                            )?;
                                        let tensor_cube = output_tensor.clone().as_float_cube()?;
                                        let sentence_matrices: Vec<FloatMatrix> =
                                            tensor_cube.outer_iter().map(|m| m.to_owned()).collect();
                                        Ok(sentence_matrices)
                                    })
                                    .collect::<Result<Vec<Vec<FloatMatrix>>, EngineError>>()?
                                    .into_iter()
                                    .flatten()
                                    .collect();

                                // Some `no-pooling` outputs are already one row per text.
                                // Truncation still cuts the last axis.
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

                                // One base64 string per row, the flattened floats.
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

                    let final_json_output = embedding_output.export_with_transformer(transformer)?;
                    Ok(final_json_output)
                })
                .collect::<Result<Vec<_>, EngineError>>()
        })?;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A right-padded encoding of `real` tokens padded to `padded`.
    fn enc(real: usize, padded: usize) -> TransformerEncodingsWithPosition {
        let ids: Vec<i64> = (0..padded).map(|i| if i < real { 5 } else { 0 }).collect();
        let mask: Vec<i64> = (0..padded).map(|i| (i < real) as i64).collect();
        TransformerEncodingsWithPosition {
            attention_mask: mask,
            input_ids: ids,
            token_type_ids: vec![0; padded],
            tokens_with_positions: vec![],
        }
    }

    #[test]
    fn default_budget_is_the_old_worst_case() {
        assert_eq!(DEFAULT_MAX_BATCH_TOKENS, 32 * 512);
        assert_eq!(DEFAULT_MAX_BATCH_ATTENTION, 32 * 512 * 512);
    }

    #[test]
    fn attention_budget_bounds_rows_times_length_squared() {
        let b = BatchBudget::DEFAULT;
        // 32 texts of 512 tokens fit exactly.
        assert!(b.fits(32, 512));
        // Two 8192-token rows fit the token limit (16384) and miss the
        // length-squared limit, so they run separately.
        assert!(2 * 8192 <= b.tokens);
        assert!(!b.fits(2, 8192));
        assert_eq!(plan_lengths(&[8192, 8192], &b), vec![0..1, 1..2]);
        // 2048-token rows: two per batch.
        assert!(b.fits(2, 2048) && !b.fits(3, 2048));
    }

    #[test]
    fn a_chunk_that_fits_is_never_split() {
        // 32 rows padded to 512 is the default budget, so the chunk stays whole.
        let chunk: Vec<_> = (0..32).map(|i| enc(10 + i, 512)).collect();
        assert_eq!(plan_sub_batches(&chunk, &BatchBudget::DEFAULT), vec![0..32]);
    }

    #[test]
    fn a_long_text_shrinks_its_batch_and_keeps_order() {
        // 31 short texts and one 8192-token text in the middle.
        let mut lengths = vec![20; 32];
        lengths[10] = 8192;
        let ranges = plan_lengths(&lengths, &BatchBudget::DEFAULT);
        // The ranges stay in order and cover every row.
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, 32);
        for w in ranges.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        for r in &ranges {
            let longest = lengths[r.clone()].iter().max().unwrap();
            assert!(r.len() == 1 || BatchBudget::DEFAULT.fits(r.len(), *longest), "{r:?}");
        }
    }

    #[test]
    fn an_over_budget_text_runs_alone() {
        assert_eq!(plan_lengths(&[10, 40000, 10], &BatchBudget::DEFAULT), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn trimming_drops_only_padding() {
        let sub = trim_right_padding(vec![enc(3, 100), enc(7, 100)]);
        assert!(sub.iter().all(|e| e.input_ids.len() == 7 && e.attention_mask.len() == 7));
        assert_eq!(real_length(&sub[0]), 3);
        assert_eq!(real_length(&sub[1]), 7);
        assert_eq!(&sub[1].input_ids, &vec![5; 7]);
    }

    #[test]
    fn left_padding_is_left_alone() {
        let mut left = enc(3, 6);
        left.attention_mask.reverse();
        left.input_ids.reverse();
        assert!(!is_right_padded(&left));
        let chunk = vec![left, enc(6, 6)];
        assert_eq!(
            plan_sub_batches(&chunk, &BatchBudget { tokens: 1, attention: 1 }),
            vec![0..2]
        );
    }

    #[test]
    fn real_length_ignores_padding() {
        assert_eq!(real_length(&enc(4, 9)), 4);
    }
}
