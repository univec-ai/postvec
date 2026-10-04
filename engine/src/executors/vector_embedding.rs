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
        Ok(Self { normalize_input })
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
        // --- 1. Get Input ---
        // Retrieve the input embeddings from the first configured JSON input.
        // The `as_float_matrix` helper handles the conversion from a JSON
        // array of arrays into an ndarray `FloatMatrix`.
        let mut input_embeddings: FloatMatrix = ctx.input_json(0)?.as_float_matrix()?;
        if self.normalize_input {
            normalize_rows(&mut input_embeddings);
        }

        // --- 2. Prepare for Model ---
        // Convert the `FloatMatrix` into a `GenericTensor`.
        // The `to_tensor` method
        // is part of the `IntoGenericTensorExt` trait, which is implemented for
        // ndarray types like `FloatMatrix`.
        let input_tensor = input_embeddings.to_tensor();

        // --- 3. Run Inference ---
        // Query the model with the prepared input tensor.
        let output_tensors = ctx.query(&[input_tensor])?;

        // --- 4. Process Outputs ---
        // Get model and executor configuration.
        let overview = ctx.model_overview()?;
        let executor_outputs = &ctx.model_configuration().executor.outputs;

        // Process each configured output in parallel.
        // We use `.iter().enumerate()` to get the index of each output mapping.
        let structured_outputs: Vec<Value> = executor_outputs
            .iter()
            .enumerate() // Get the index of each output mapping
            .map(|(output_index, output_mapping)| {
                // Determine which tensor to use.
                // If `layer_name` is provided, find its index by name.
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
                    // If no `layer_name` is given, default to the mapping's index.
                    output_index
                };

                // Get the tensor from the model's output.
                let output_tensor = output_tensors.get(tensor_index).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Model output tensor at index {} not found for output key '{}'",
                        tensor_index, output_mapping.json_key
                    ))
                })?;

                // Convert the generic output tensor back into a strongly-typed `FloatMatrix`.
                // The `as_float_matrix` method on `GenericTensor` handles this conversion,
                // failing if the tensor is not numeric or doesn't have a 2D shape.
                let output_embeddings: FloatMatrix = output_tensor.clone().as_float_matrix()?;

                // To create a JSON array of arrays for the response, we first convert the
                // `FloatMatrix` into a `Vec<Vec<f32>>`.
                let embeddings_as_vecs: Vec<Vec<f32>> = output_embeddings
                    .rows()
                    .into_iter()
                    .map(|row| row.to_vec())
                    .collect();

                // Convert the final list of embeddings into a JSON value.
                Ok(json!(embeddings_as_vecs))
            })
            .collect::<Result<Vec<_>, EngineError>>()?;

        // --- 5. Format and Return ---
        // Return the embeddings as a structured output. The server will map these
        // values to the keys defined in `executor.outputs`.
        Ok(ExecutorOutput::Structured(structured_outputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::array;

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
