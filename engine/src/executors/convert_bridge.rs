//! ## Convert Bridge Executor
//!
//! This executor acts as an orchestrator, chaining two conversion models to provide a
//! combined convert + convert operation in a single step:
//! 1. A bridge conversion model (source embedding -> intermediate embedding)
//! 2. A target conversion model (intermediate embedding -> target embedding)
//!
//! This is a "generic" executor, meaning it does not load its own model file
//! but instead calls other models managed by the inference engine.

// --- Crate-internal Imports ---
use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{Executor, ExecutorOutput};
use crate::models::ModelConfiguration;

// --- External Imports ---

use tokio::runtime::Handle;

/// The `ConvertBridgeExecutor`.
///
/// This executor chains two conversion models, providing a way to convert
/// embeddings from a source format to a target format via an intermediate format.
///
/// Models are passed dynamically via input arguments:
/// Input 0: embeddings (array of arrays of floats)
/// Input 1: source_model (string) - used for context/logging (and potentially validation)
/// Input 2: bridge_model (string) - the bridge conversion model (source -> intermediate)
/// Input 3: target_model (string) - the target conversion model (intermediate -> target)
pub struct ConvertBridgeExecutor;

impl ConvertBridgeExecutor {
    /// Creates a new `ConvertBridgeExecutor`.
    pub fn new(_config: &ModelConfiguration) -> Result<Self, EngineError> {
        Ok(Self)
    }
}

impl Executor for ConvertBridgeExecutor {
    /// Executes the convert + convert bridge pipeline.
    ///
    /// Models are resolved dynamically from the input.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // Handle async calls in a blocking context, avoiding deadlocks.
        let handle = Handle::current();

        tokio::task::block_in_place(|| {
            handle.block_on(async {
                // --- 1. Get Input ---
                // Input 0: embeddings
                let embeddings_value = ctx.input_json(0)?.as_value().clone();
                // Input 1: source_model
                let source_model_name = ctx.input_json(1)?.as_string()?;
                // Input 2: bridge_model
                let bridge_model_name = ctx.input_json(2)?.as_string()?;
                // Input 3: target_model
                let target_model_name = ctx.input_json(3)?.as_string()?;

                log::debug!(
                    "Executing ConvertBridge: source='{}' -> bridge='{}' -> target='{}'",
                    source_model_name,
                    bridge_model_name,
                    target_model_name
                );

                // --- 2. Resolve Models ---
                // Rule 1: Find converter: source -> bridge
                let convert1_resolved = ctx
                    .resolver()
                    .resolve_convert(&source_model_name, &bridge_model_name)
                    .ok_or_else(|| {
                        EngineError::ConverterNotFound(format!(
                            "ConvertBridge: Could not find any converter model from '{}' to '{}'",
                            source_model_name, bridge_model_name
                        ))
                    })?;

                // Rule 2: Find converter: bridge -> target
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

                // --- 3. Call Bridge Conversion Model (Model 1) ---
                let bridge_model_handle = ctx.model(&convert1_resolved.internal_name)?;
                let bridge_config = bridge_model_handle.configuration().clone();

                let bridge_output = bridge_model_handle
                    .predict(&[embeddings_value], ctx.remaining())
                    .await?;

                // --- 4. Extract Intermediate Embeddings ---
                let intermediate_embeddings =
                    crate::utils::extract_embeddings(bridge_output, &bridge_config)?;

                // --- 5. Call Target Conversion Model (Model 2) ---
                let target_model_handle = ctx.model(&convert2_resolved.internal_name)?;
                let target_config = target_model_handle.configuration().clone();

                let target_output = target_model_handle
                    .predict(&[intermediate_embeddings], ctx.remaining())
                    .await?;

                // --- 6. Extract and Return Final Embeddings ---
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

    /// Drive a real convert-bridge request (src → mid → tgt) through
    /// `execute()` against the given resolver inventory and return the error.
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

    /// A real request with no converters loaded: the first leg must fail as
    /// `ConverterNotFound` (CONVERTER_NOT_FOUND on the wire), not a generic
    /// Prediction/INTERNAL_ERROR — clients failover and retry on the typed
    /// code instead of treating missing inventory as opaque.
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

    /// First leg resolvable, second missing: still `ConverterNotFound`,
    /// naming the (mid → tgt) leg that is actually absent.
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
