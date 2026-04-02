//! Pull the embedding payload out of an executor's output.

use crate::error::EngineError;
use crate::executors::ExecutorOutput;
use crate::models::ModelConfiguration;
use serde_json::Value;

/// First structured value, or the JSON object at the first output's `json_key`
/// (falls back to `"embeddings"`, then to the whole object).
pub fn extract_embeddings(
    output: ExecutorOutput,
    config: &ModelConfiguration,
) -> Result<Value, EngineError> {
    match output {
        ExecutorOutput::Structured(mut values) => {
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
            let output_key = config
                .executor
                .outputs
                .first()
                .map(|o| o.json_key.as_str())
                .unwrap_or("embeddings");

            if let Some(embeddings) = obj.get(output_key) {
                Ok(embeddings.clone())
            } else {
                Ok(obj)
            }
        }
        ExecutorOutput::Binary { .. } => Err(EngineError::Prediction(
            "Model returned unexpected binary output".to_string(),
        )),
    }
}
