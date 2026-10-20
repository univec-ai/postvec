//! JSON in, tensors through the model, JSON out. No extra transform.
use super::{Context, Executor, ExecutorOutput};
use crate::error::EngineError;
use ndarray::{ArrayD, IxDyn};
use rayon::prelude::*;
use serde_json::{json, Value};
use shared::vectors::{GenericTensor, TensorDataType, TensorValue};

pub struct PassThruExecutor;

impl PassThruExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Executor for PassThruExecutor {
    /// A missing `layer_name` on an output mapping uses that mapping's index.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        let model_overview = ctx.model_overview()?;
        let executor_config = &ctx.model_configuration().executor;

        let mut input_tensors: Vec<GenericTensor> = Vec::new();
        for (i, model_input_layer) in model_overview.inputs.iter().enumerate() {
            let data_f32: Vec<f32> = ctx.input_json(i)?.as_vec()?;

            // Dynamic dimensions (-1) are treated as 1.
            let shape: Vec<usize> = model_input_layer
                .shape
                .iter()
                .map(|&d| if d < 0 { 1 } else { d as usize })
                .collect();

            let tensor_value = ArrayD::from_shape_vec(IxDyn(&shape), data_f32)
                .map_err(|e| EngineError::Prediction(e.to_string()))?;

            input_tensors.push(GenericTensor {
                value: TensorValue::Float32(tensor_value),
                shape,
                dtype: TensorDataType::Numeric,
            });
        }

        let output_tensors = ctx.query(&input_tensors)?;

        let outputs: serde_json::Map<String, Value> = executor_config
            .outputs
            .par_iter()
            .enumerate()
            .map(|(output_index, output_mapping)| {
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
                    output_index
                };

                let tensor = output_tensors.get(tensor_index).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Model output tensor at index {} not found for output key '{}'",
                        tensor_index, output_mapping.json_key
                    ))
                })?;

                let value = match &tensor.value {
                    TensorValue::Float32(arr) => json!(arr.iter().collect::<Vec<_>>()),
                    TensorValue::Int64(arr) => json!(arr.iter().collect::<Vec<_>>()),
                    TensorValue::String(s) => json!(s),
                    TensorValue::Dictionary(_) => json!("Unsupported output type: Dictionary"),
                };
                Ok((output_mapping.json_key.clone(), value))
            })
            .collect::<Result<Vec<_>, EngineError>>()?
            .into_iter()
            .collect();

        Ok(ExecutorOutput::Json(Value::Object(outputs)))
    }
}
