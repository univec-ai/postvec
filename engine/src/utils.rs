//! Shared utility functions for executors.

use crate::error::EngineError;
use crate::executors::ExecutorOutput;
use crate::models::ModelConfiguration;
use serde_json::Value;

/// Extracts embeddings from an ExecutorOutput using the model's configuration.
///
/// For `Structured` output: The values correspond to `executor.outputs` in order,
/// so the first value maps to the first configured output.
///
/// For `Json` output: We look up the first output's `json_key` from the model's
/// configuration to find the correct key in the response object.
pub fn extract_embeddings(
    output: ExecutorOutput,
    config: &ModelConfiguration,
) -> Result<Value, EngineError> {
    match output {
        ExecutorOutput::Structured(mut values) => {
            // The first structured output should contain the embeddings.
            // The order matches the `executor.outputs` array in the model's config.
            if values.is_empty() {
                return Err(EngineError::Prediction(
                    "Model returned empty structured output".to_string(),
                ));
            }
            Ok(values.remove(0))
        }
        ExecutorOutput::StructuredWithUsage {
            mut outputs,
            usage: _,
        } => {
            if outputs.is_empty() {
                return Err(EngineError::Prediction(
                    "Model returned empty structured output".to_string(),
                ));
            }
            Ok(outputs.remove(0))
        }
        ExecutorOutput::Json(obj) => {
            // For JSON output, look up the first output's json_key from config.
            let output_key = config
                .executor
                .outputs
                .first()
                .map(|o| o.json_key.as_str())
                .unwrap_or("embeddings"); // Fallback if no outputs configured

            if let Some(embeddings) = obj.get(output_key) {
                Ok(embeddings.clone())
            } else {
                // Return the entire object if the key is not found
                Ok(obj)
            }
        }
        ExecutorOutput::Binary { .. } => Err(EngineError::Prediction(
            "Model returned unexpected binary output".to_string(),
        )),
    }
}
