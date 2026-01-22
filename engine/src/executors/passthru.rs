// File: engine/src/executors/passthru.rs
//!
//! ## Pass-Thru Executor
//!
//! An executor designed to pass structured JSON data directly to a model
//! without significant transformation.
//!
//! It maps JSON inputs to model tensors
//! and formats the model's output tensors back into a JSON object.
use super::{Context, Executor, ExecutorOutput};
use crate::error::EngineError;
use ndarray::{ArrayD, IxDyn};
use rayon::prelude::*;
use serde_json::{json, Value};
use shared::vectors::{GenericTensor, TensorDataType, TensorValue};

/// An executor that passes structured JSON data directly to a model.
pub struct PassThruExecutor;

impl PassThruExecutor {
    /// The constructor for the PassThruExecutor.
    pub fn new() -> Self {
        Self
    }
}

impl Executor for PassThruExecutor {
    /// Executes the pass-through logic.
    ///
    /// 1.  It iterates through the model's configured input layers.
    /// 2.  For each layer, it reads the corresponding data from the input JSON payload.
    /// 3.  It reshapes the data into a tensor matching the model's expected input shape.
    /// 4.  It queries the model with the prepared input tensors.
    /// 5.  It processes the output tensors in parallel, mapping them back to a JSON object
    ///     based on the executor's output configuration.
    /// 6.  If `layer_name` is missing in an output mapping, the mapping's index is
    ///     used as the tensor index.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        let model_overview = ctx.model_overview()?;
        let executor_config = &ctx.model_configuration().executor;

        // --- Prepare Input Tensors ---
        let mut input_tensors: Vec<GenericTensor> = Vec::new();
        for (i, model_input_layer) in model_overview.inputs.iter().enumerate() {
            // Extract the raw f32 vector from the JSON input.
            let data_f32: Vec<f32> = ctx.input_json(i)?.as_vec()?;

            // Determine the target shape, handling dynamic dimensions (-1) by treating them as 1.
            let shape: Vec<usize> = model_input_layer
                .shape
                .iter()
                .map(|&d| if d < 0 { 1 } else { d as usize })
                .collect();

            // Create an ndarray tensor from the flat vector and shape.
            let tensor_value = ArrayD::from_shape_vec(IxDyn(&shape), data_f32)
                .map_err(|e| EngineError::Prediction(e.to_string()))?;

            // Wrap the tensor in the application's generic tensor format.
            input_tensors.push(GenericTensor {
                value: TensorValue::Float32(tensor_value),
                shape,
                dtype: TensorDataType::Numeric,
            });
        }

        // --- Query the Model ---
        let output_tensors = ctx.query(&input_tensors)?;

        // --- Process Outputs ---
        // Iterate over the *configured* outputs, which now drive the logic.
        // We use `.par_iter().enumerate()` to get the index of each output mapping.
        let outputs: serde_json::Map<String, Value> = executor_config
            .outputs
            .par_iter()
            .enumerate() // Get the index of each output mapping
            .map(|(output_index, output_mapping)| {
                // Determine which tensor to use.
                // If `layer_name` is provided, find its index by name.
                let tensor_index = if let Some(layer_name) = &output_mapping.layer_name {
                    model_overview
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
                let tensor = output_tensors.get(tensor_index).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Model output tensor at index {} not found for output key '{}'",
                        tensor_index, output_mapping.json_key
                    ))
                })?;

                // Convert the tensor data back into a serde_json::Value.
                let value = match &tensor.value {
                    // The `.into_raw_vec()` method is deprecated.
                    // The correct way to get a `Vec`
                    // containing the logical elements of the array is to consume it into an iterator
                    // and collect the results.
                    TensorValue::Float32(arr) => json!(arr.iter().collect::<Vec<_>>()),
                    TensorValue::Int64(arr) => json!(arr.iter().collect::<Vec<_>>()),
                    TensorValue::String(s) => json!(s),
                    // Handle unsupported types gracefully.
                    TensorValue::Dictionary(_) => json!("Unsupported output type: Dictionary"),
                };
                Ok((output_mapping.json_key.clone(), value))
            })
            .collect::<Result<Vec<_>, EngineError>>()? // Collect results from parallel execution
            .into_iter()
            .collect(); // Convert Vec<(String, Value)> into a Map

        // Return the final result as a JSON object.
        Ok(ExecutorOutput::Json(Value::Object(outputs)))
    }
}
