//! Batch of embedding vectors through a model, batch of embeddings back.
//!
//! Input rows are L2-normalized before the model runs (`executor.params.normalize_input`,
//! default `true`): the univec converters are trained on unit-length embeddings while
//! callers may send unnormalized vectors (eq truncated Matryoshka dimensions or embed models
//! without l2norm output).

use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{Executor, ExecutorOutput};
use crate::models::ModelConfiguration;
use serde_json::{json, Value};
use shared::vectors::{FloatMatrix, IntoGenericTensorExt};

/// The `VectorEmbeddingExecutor`.
///
/// This executor passes embedding data through an underlying model, L2-normalizing
/// the input rows first unless `normalize_input` is off.
pub struct VectorEmbeddingExecutor {
    normalize_input: bool,
    /// The descriptor's `params.source_dim`, the fallback input width when the
    /// graph's input dimension is dynamic.
    source_dim: Option<usize>,
}

impl VectorEmbeddingExecutor {
    /// Reads `executor.params.normalize_input` (bool, default `true`).
    pub fn new(config: &ModelConfiguration) -> Result<Self, EngineError> {
        let normalize_input = match config.executor.params.get("normalize_input") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| {
                EngineError::Configuration(format!(
                    "model '{}': executor.params.normalize_input must be true or false, got {}",
                    config.name, v
                ))
            })?,
        };
        let source_dim = config
            .params
            .get("source_dim")
            .and_then(|v| v.as_u64())
            .filter(|&d| d > 0)
            .map(|d| d as usize);
        Ok(Self {
            normalize_input,
            source_dim,
        })
    }

    /// The vector width this model accepts: the graph input's static last
    /// dimension when it has one (authoritative), else the declared
    /// `source_dim`.
    fn expected_width(&self, overview: &crate::models::ModelOverview) -> Option<usize> {
        overview
            .inputs
            .first()
            .and_then(|layer| layer.shape.last().copied())
            .filter(|&d| d > 0)
            .map(|d| d as usize)
            .or(self.source_dim)
    }
}

/// Reject a batch whose vectors are not the model's input width.
/// The caller gets invalid input. Left unchecked, ONNX Runtime raises an
/// internal error and the server answers 502.
fn check_width(input: &FloatMatrix, expected: Option<usize>) -> Result<(), EngineError> {
    match expected {
        Some(expected) if input.nrows() > 0 && input.ncols() != expected => {
            Err(EngineError::InputTypeError(format!(
                "this converter takes {expected}-dimensional vectors, got {}",
                input.ncols()
            )))
        }
        _ => Ok(()),
    }
}

/// L2-normalizes every row in place. All-zero rows are left as they are.
fn normalize_rows(m: &mut FloatMatrix) {
    for mut row in m.rows_mut() {
        let norm = row.dot(&row).sqrt();
        if norm > 0.0 {
            row /= norm;
        }
    }
}

impl Executor for VectorEmbeddingExecutor {
    /// JSON matrix in, model query, JSON matrix out per configured output.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        let mut input_embeddings: FloatMatrix = ctx.input_json(0)?.as_float_matrix()?;
        let overview = ctx.model_overview()?;
        check_width(&input_embeddings, self.expected_width(&overview))?;
        if self.normalize_input {
            normalize_rows(&mut input_embeddings);
        }

        let input_tensor = input_embeddings.to_tensor();
        let output_tensors = ctx.query(&[input_tensor])?;
        let executor_outputs = &ctx.model_configuration().executor.outputs;

        let structured_outputs: Vec<Value> = executor_outputs
            .iter()
            .enumerate()
            .map(|(output_index, output_mapping)| {
                let tensor_index = if let Some(layer_name) = &output_mapping.layer_name {
                    overview
                        .outputs
                        .iter()
                        .position(|l| &l.name == layer_name)
                        .ok_or_else(|| {
                            EngineError::Configuration(format!(
                                "Output layer '{}' defined in executor config not found in model overview.",
                                layer_name
                            ))
                        })?
                } else {
                    output_index
                };

                let output_tensor = output_tensors.get(tensor_index).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Model output tensor at index {} not found for output key '{}'",
                        tensor_index, output_mapping.json_key
                    ))
                })?;

                let output_embeddings: FloatMatrix = output_tensor.clone().as_float_matrix()?;
                let embeddings_as_vecs: Vec<Vec<f32>> = output_embeddings
                    .rows()
                    .into_iter()
                    .map(|row| row.to_vec())
                    .collect();

                Ok(json!(embeddings_as_vecs))
            })
            .collect::<Result<Vec<_>, EngineError>>()?;

        Ok(ExecutorOutput::Structured(structured_outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

    #[test]
    fn wrong_width_is_invalid_input_not_internal() {
        let m = array![[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0]];
        assert!(check_width(&m, Some(3)).is_ok());
        assert!(check_width(&m, None).is_ok());
        let err = check_width(&m, Some(1024)).unwrap_err();
        assert!(matches!(err, EngineError::InputTypeError(_)));
        assert_eq!(err.to_error_code(), shared::ErrorCode::InvalidInput);
        // An empty batch has no rows to check.
        let empty = FloatMatrix::zeros((0, 0));
        assert!(check_width(&empty, Some(1024)).is_ok());
    }

    fn cfg(params: serde_json::Value) -> ModelConfiguration {
        serde_json::from_value(serde_json::json!({
            "name": "convert-test", "executor": { "key": "vector-embedding", "params": params }
        }))
        .unwrap()
    }

    #[test]
    fn normalize_input_defaults_to_true_and_can_be_turned_off() {
        assert!(VectorEmbeddingExecutor::new(&cfg(serde_json::json!({}))).unwrap().normalize_input);
        let off = cfg(serde_json::json!({ "normalize_input": false }));
        assert!(!VectorEmbeddingExecutor::new(&off).unwrap().normalize_input);
        assert!(VectorEmbeddingExecutor::new(&cfg(serde_json::json!({ "normalize_input": "no" }))).is_err());
    }

    #[test]
    fn rows_become_unit_length_and_zero_rows_stay_zero() {
        let mut m: FloatMatrix = array![[3.0, 4.0], [0.0, 0.0], [0.6, 0.8]];
        normalize_rows(&mut m);
        assert_eq!(m, array![[0.6, 0.8], [0.0, 0.0], [0.6, 0.8]]);
    }
}
