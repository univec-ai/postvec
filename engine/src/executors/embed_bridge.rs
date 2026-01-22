// File: engine/src/executors/embed_bridge.rs
//! ## Embed Bridge Executor
//!
//! This executor acts as an orchestrator, chaining two models to provide a
//! combined embed + convert operation in a single step:
//! 1. A source embedding model (text -> embedding)
//! 2. A target conversion model (embedding -> embedding)
//!
//! This is a "generic" executor, meaning it does not load its own model file
//! but instead calls other models managed by the inference engine.

// --- Crate-internal Imports ---
use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{ExecutionMetadata, Executor, ExecutorOutput};
use crate::models::ModelConfiguration;

// --- External Imports ---

use std::collections::HashSet;
use tokio::runtime::Handle;

/// The `EmbedBridgeExecutor`.
///
/// This executor chains a text embedding model with a conversion model,
/// providing a way to get embeddings in a target format from raw text input.
///
/// Models are passed dynamically via input arguments:
/// Input 0: texts (array of strings)
/// Input 1: bridge_model (string) - the source embedding model
/// Input 2: target_model (string) - the target conversion model
pub struct EmbedBridgeExecutor {
    /// `params.restrictions.target_models` — semantic target names this bridge
    /// will refuse to produce embeddings for. Matched against the resolved
    /// `target_model` (input 2). Backs the licence-driven block on
    /// TO-commercial bridge embedding (see docs/licenses/licenses.md §5.3).
    restricted_targets: HashSet<String>,
}

impl EmbedBridgeExecutor {
    /// Creates a new `EmbedBridgeExecutor`.
    pub fn new(config: &ModelConfiguration) -> Result<Self, EngineError> {
        let restricted_targets: HashSet<String> = config
            .params
            .get("restrictions")
            .and_then(|r| r.get("target_models"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if !restricted_targets.is_empty() {
            log::info!(
                "EmbedBridge '{}' configured with {} restricted target_models: {:?}",
                config.name,
                restricted_targets.len(),
                restricted_targets
            );
        }

        Ok(Self { restricted_targets })
    }
}

impl Executor for EmbedBridgeExecutor {
    /// Executes the embed + convert bridge pipeline.
    ///
    /// Models are resolved dynamically from the input.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // Since the underlying chained calls use the async `predict` API,
        // we need to bridge the sync/async boundary. The `execute` method
        // runs inside a `spawn_blocking` context (managed by `InferenceEngine::predict_raw`).
        // We use `tokio::task::block_in_place` + `Handle::current().block_on`
        // to safely run async code from this blocking context.
        let handle = Handle::current();

        tokio::task::block_in_place(|| {
            handle.block_on(async {
                // --- 1. Get Input ---
                // Input 0: texts (array of strings)
                // Input 1: bridge_model (string) - the semantic target of the embedding
                // Input 2: target_model (string) - the final desired format
                let texts_value = ctx.input_json(0)?.as_value().clone();
                let bridge_model_name = ctx.input_json(1)?.as_string()?;
                let target_model_name = ctx.input_json(2)?.as_string()?;

                // Reject restricted targets before any model resolution work.
                // Matches against the *semantic* target name aphex passes
                // through gRPC (e.g. "openai-text-embedding-3-small").
                // `TargetRestricted` (not `Prediction`) so the refusal crosses
                // gRPC as TARGET_RESTRICTED and callers (aphex, postvec) can
                // fail fast instead of retrying a policy decision.
                if self.restricted_targets.contains(&target_model_name) {
                    return Err(EngineError::TargetRestricted(format!(
                        "embed-bridge target '{}' is not available for bridge embedding on this deployment",
                        target_model_name
                    )));
                }

                log::debug!(
                    "Executing EmbedBridge: bridge='{}', target='{}'",
                    bridge_model_name, target_model_name
                );

                // --- 2. Resolve Models ---
                // Rule 1: Find an embedding model that outputs `bridge_model_name`
                let embed_resolved = ctx.resolver()
                    .resolve_embed(&bridge_model_name)
                    .ok_or_else(|| EngineError::BridgePathNotFound(format!(
                        "EmbedBridge: Could not find any embedding model that outputs '{}'",
                        bridge_model_name
                    )))?;

                // Rule 2: Find a converter that goes from `bridge_model_name` to `target_model_name`
                let convert_resolved = ctx.resolver()
                    .resolve_convert(&bridge_model_name, &target_model_name)
                    .ok_or_else(|| EngineError::ConverterNotFound(format!(
                        "EmbedBridge: Could not find any converter model from '{}' to '{}'",
                        bridge_model_name, target_model_name
                    )))?;

                log::info!(
                    "EmbedBridge resolved chain: {} -> {}", 
                    embed_resolved.internal_name, convert_resolved.internal_name
                );

                // --- 3. Call Source Embedding Model ---
                // The source model (e.g., Alibaba-NLP.gte-base-en-v1.5) expects
                // texts as its first input and returns embeddings.
                // We use the RESOLVED internal name, not the user-provided public name.
                let source_model_handle = ctx.model(&embed_resolved.internal_name)?;
                // Get the source model's configuration for output key lookup.
                let source_config = source_model_handle.configuration().clone();
                let source_output = source_model_handle
                    .predict(&[texts_value], ctx.remaining())
                    .await?;

                // --- 4. Extract Intermediate Embeddings ---
                // Attempts to extract usage metadata if available.
                let (embeddings_value, usage) = match source_output {
                    ExecutorOutput::StructuredWithUsage { mut outputs, usage } => {
                       if outputs.is_empty() {
                           return Err(EngineError::Prediction(
                               "Source model returned empty structured output".to_string()
                           ));
                       }
                       (outputs.remove(0), usage)
                    },
                    other => (crate::utils::extract_embeddings(other, &source_config)?, ExecutionMetadata::default()),
                };

                // --- 5. Call Target Conversion Model ---
                // The target model expects embeddings as its first input
                // and returns converted embeddings.
                let target_model_handle = ctx.model(&convert_resolved.internal_name)?;
                // Get the target model's configuration for output key lookup.
                let target_config = target_model_handle.configuration().clone();
                let target_output = target_model_handle
                    .predict(&[embeddings_value], ctx.remaining())
                    .await?;

                // --- 6. Extract and Return Final Embeddings ---
                // Use the target model's config to find the correct output key.
                let final_embeddings = crate::utils::extract_embeddings(target_output, &target_config)?;

                // Return in the standard Structured format, matching other executors.
                // The server will map this to the output keys defined in executor.outputs.
                Ok(ExecutorOutput::StructuredWithUsage {
                    outputs: vec![final_embeddings],
                    usage,
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_with_params(params: serde_json::Value) -> ModelConfiguration {
        serde_json::from_value(json!({ "name": "bridge", "params": params })).unwrap()
    }

    #[test]
    fn parses_restricted_target_models() {
        let exec = EmbedBridgeExecutor::new(&config_with_params(json!({
            "restrictions": { "target_models": ["ada-002", "text-embedding-3-small"] }
        })))
        .unwrap();
        assert_eq!(exec.restricted_targets.len(), 2);
        assert!(exec.restricted_targets.contains("ada-002"));
        assert!(exec.restricted_targets.contains("text-embedding-3-small"));
    }

    /// The restriction refusal must map to TARGET_RESTRICTED on the wire —
    /// a generic Prediction/INTERNAL_ERROR would make callers retry a policy
    /// decision forever.
    #[test]
    fn restricted_target_error_maps_to_target_restricted_code() {
        let err = EngineError::TargetRestricted(
            "embed-bridge target 'x' is not available for bridge embedding on this deployment"
                .to_string(),
        );
        assert_eq!(err.to_error_code(), shared::ErrorCode::TargetRestricted);
        assert_eq!(err.to_error_code().as_str(), "TARGET_RESTRICTED");
        // Display stays prefix-free (AppError/HTTP surface shows it verbatim).
        assert!(err.to_string().starts_with("embed-bridge target 'x'"));
    }

    #[test]
    fn no_restrictions_yields_empty_set() {
        let exec = EmbedBridgeExecutor::new(&config_with_params(json!({}))).unwrap();
        assert!(exec.restricted_targets.is_empty());
    }

    #[test]
    fn malformed_restrictions_are_ignored() {
        // target_models not an array → treated as no restrictions, not an error.
        let exec = EmbedBridgeExecutor::new(&config_with_params(json!({
            "restrictions": { "target_models": "not-an-array" }
        })))
        .unwrap();
        assert!(exec.restricted_targets.is_empty());
    }

    #[test]
    fn non_string_entries_filtered_out() {
        let exec = EmbedBridgeExecutor::new(&config_with_params(json!({
            "restrictions": { "target_models": ["ok", 123, true] }
        })))
        .unwrap();
        assert_eq!(exec.restricted_targets.len(), 1);
        assert!(exec.restricted_targets.contains("ok"));
    }

    use crate::executors::bridge_test_support::{execute_against_resolver, model_cfg};

    /// Drive a real embed-bridge request (embed "m", convert into "t")
    /// through `execute()` against the given resolver inventory.
    fn run_incomplete(loaded: &[ModelConfiguration]) -> EngineError {
        let exec_cfg = model_cfg("embed-bridge", json!({ "model_type": "embed-bridge" }));
        match execute_against_resolver(
            EmbedBridgeExecutor::new(&exec_cfg).unwrap(),
            &exec_cfg,
            loaded,
            vec![json!(["hello"]), json!("m"), json!("t")],
        ) {
            Err(e) => e,
            Ok(_) => panic!("an incomplete bridge chain must fail resolution"),
        }
    }

    /// A real request with no embed model for the bridge source: the embed
    /// leg must fail as `BridgePathNotFound` (BRIDGE_PATH_NOT_FOUND on the
    /// wire), not a generic Prediction/INTERNAL_ERROR.
    #[test]
    fn unresolved_embed_leg_is_bridge_path_not_found() {
        let err = run_incomplete(&[]);
        match &err {
            EngineError::BridgePathNotFound(msg) => {
                assert!(msg.contains("'m'"), "names the missing source: {msg}");
            }
            other => panic!("expected BridgePathNotFound, got {other:?}"),
        }
        assert_eq!(err.to_error_code(), shared::ErrorCode::BridgePathNotFound);
        assert_eq!(err.to_error_code().as_str(), "BRIDGE_PATH_NOT_FOUND");
    }

    /// Embed leg resolvable but no converter into the target: the converter
    /// leg must fail as `ConverterNotFound`, naming the (m → t) pair.
    #[test]
    fn unresolved_converter_leg_is_converter_not_found() {
        let loaded = [model_cfg(
            "m-internal",
            json!({ "model_type": "embed", "target_model": "m" }),
        )];
        let err = run_incomplete(&loaded);
        match &err {
            EngineError::ConverterNotFound(msg) => {
                assert!(msg.contains("'m' to 't'"), "names the missing leg: {msg}");
            }
            other => panic!("expected ConverterNotFound, got {other:?}"),
        }
        assert_eq!(err.to_error_code(), shared::ErrorCode::ConverterNotFound);
    }
}
