//! Batch of embedding vectors through a model, batch of embeddings back.

use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{Executor, ExecutorOutput};
use serde_json::{json, Value};
use shared::vectors::{FloatMatrix, IntoGenericTensorExt};

pub struct VectorEmbeddingExecutor;

impl VectorEmbeddingExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Executor for VectorEmbeddingExecutor {
    /// JSON matrix in, model query, JSON matrix out per configured output.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // --- 1. Get Input ---
        // Retrieve the input embeddings from the first configured JSON input.
        // The `as_float_matrix` helper handles the conversion from a JSON
        // array of arrays into an ndarray `FloatMatrix`.
        let input_embeddings: FloatMatrix = ctx.input_json(0)?.as_float_matrix()?;

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
