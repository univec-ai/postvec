//! Two converters in a row: source space -> intermediate -> target.
//!
//! Inputs: 0 embeddings, 1 source_model, 2 bridge_model, 3 target_model.
//! Does not load its own weights; it calls other models on the engine.

use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{Executor, ExecutorOutput};
use crate::models::ModelConfiguration;

use tokio::runtime::Handle;

pub struct ConvertBridgeExecutor;

impl ConvertBridgeExecutor {
    pub fn new(_config: &ModelConfiguration) -> Result<Self, EngineError> {
        Ok(Self)
    }
}

impl Executor for ConvertBridgeExecutor {
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // Nested `predict` is async; this runs inside `spawn_blocking`.
        let handle = Handle::current();

        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let embeddings_value = ctx.input_json(0)?.as_value().clone();
                let source_model_name = ctx.input_json(1)?.as_string()?;
                let bridge_model_name = ctx.input_json(2)?.as_string()?;
                let target_model_name = ctx.input_json(3)?.as_string()?;

                log::debug!(
                    "Executing ConvertBridge: source='{}' -> bridge='{}' -> target='{}'",
                    source_model_name,
                    bridge_model_name,
                    target_model_name
                );

                let convert1_resolved = ctx
                    .resolver()
                    .resolve_convert(&source_model_name, &bridge_model_name)
                    .ok_or_else(|| {
                        EngineError::ConverterNotFound(format!(
                            "ConvertBridge: Could not find any converter model from '{}' to '{}'",
                            source_model_name, bridge_model_name
                        ))
                    })?;

                let convert2_resolved = ctx
                    .resolver()
                    .resolve_convert(&bridge_model_name, &target_model_name)
                    .ok_or_else(|| {
                        EngineError::ConverterNotFound(format!(
                            "ConvertBridge: Could not find any converter model from '{}' to '{}'",
                            bridge_model_name, target_model_name
                        ))
                    })?;

                log::info!(
                    "ConvertBridge resolved chain: {} -> {}",
                    convert1_resolved.internal_name,
                    convert2_resolved.internal_name
                );

                let bridge_model_handle = ctx.model(&convert1_resolved.internal_name)?;
                let bridge_config = bridge_model_handle.configuration().clone();

                let bridge_output = bridge_model_handle
                    .predict(&[embeddings_value], ctx.remaining())
                    .await?;

                let intermediate_embeddings =
                    crate::utils::extract_embeddings(bridge_output, &bridge_config)?;

                let target_model_handle = ctx.model(&convert2_resolved.internal_name)?;
                let target_config = target_model_handle.configuration().clone();

                let target_output = target_model_handle
                    .predict(&[intermediate_embeddings], ctx.remaining())
                    .await?;

                let final_embeddings =
                    crate::utils::extract_embeddings(target_output, &target_config)?;

                Ok(ExecutorOutput::Structured(vec![final_embeddings]))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::bridge_test_support::{execute_against_resolver, model_cfg};
    use serde_json::json;

    /// Run a convert-bridge request (src, mid, tgt) through `execute` and return the error.
    fn run_incomplete(loaded: &[ModelConfiguration]) -> EngineError {
        let exec_cfg = model_cfg("convert-bridge", json!({ "model_type": "convert-bridge" }));
        match execute_against_resolver(
            ConvertBridgeExecutor::new(&exec_cfg).unwrap(),
            &exec_cfg,
            loaded,
            vec![
                json!([[0.1, 0.2]]),
                json!("src"),
                json!("mid"),
                json!("tgt"),
            ],
        ) {
            Err(e) => e,
            Ok(_) => panic!("an incomplete converter chain must fail resolution"),
        }
    }

    /// No converters loaded: the first leg is `ConverterNotFound`, not a
    /// generic `Prediction`. Callers failover on the typed code.
    #[test]
    fn unresolved_first_leg_is_converter_not_found() {
        let err = run_incomplete(&[]);
        match &err {
            EngineError::ConverterNotFound(msg) => {
                assert!(
                    msg.contains("'src' to 'mid'"),
                    "names the missing leg: {msg}"
                );
            }
            other => panic!("expected ConverterNotFound, got {other:?}"),
        }
        assert_eq!(err.to_error_code(), shared::ErrorCode::ConverterNotFound);
        assert_eq!(err.to_error_code().as_str(), "CONVERTER_NOT_FOUND");
    }

    /// First leg present, second missing: still `ConverterNotFound`, naming mid to tgt.
    #[test]
    fn unresolved_second_leg_is_converter_not_found() {
        let loaded = [model_cfg(
            "conv-src-mid",
            json!({ "model_type": "convert", "source_model": "src", "target_model": "mid" }),
        )];
        let err = run_incomplete(&loaded);
        match &err {
            EngineError::ConverterNotFound(msg) => {
                assert!(
                    msg.contains("'mid' to 'tgt'"),
                    "names the missing leg: {msg}"
                );
            }
            other => panic!("expected ConverterNotFound, got {other:?}"),
        }
    }
}
