//! Text through an embed model then a converter, one call.
//!
//! Inputs: 0 texts, 1 bridge_model (source embed space), 2 target_model.
//! Does not load its own weights; it calls other models on the engine.

use crate::context::Context;
use crate::error::EngineError;
use crate::executors::{ExecutionMetadata, Executor, ExecutorOutput};
use crate::models::ModelConfiguration;

use std::collections::HashSet;
use tokio::runtime::Handle;

pub struct EmbedBridgeExecutor {
    /// Semantic names this bridge will not produce
    /// (`params.restrictions.target_models`, checked against input 2).
    restricted_targets: HashSet<String>,
}

impl EmbedBridgeExecutor {
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
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        // `execute` runs inside `spawn_blocking`. Nested `predict` is async,
        // so hop back onto the runtime with `block_in_place` + `block_on`.
        let handle = Handle::current();

        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let texts_value = ctx.input_json(0)?.as_value().clone();
                let bridge_model_name = ctx.input_json(1)?.as_string()?;
                let target_model_name = ctx.input_json(2)?.as_string()?;

                // Refuse restricted targets before resolving models. The check
                // is the semantic name from the request, not an internal name.
                // `TargetRestricted` so callers don't retry a policy decision.
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

                let embed_resolved = ctx.resolver()
                    .resolve_embed(&bridge_model_name)
                    .ok_or_else(|| EngineError::BridgePathNotFound(format!(
                        "EmbedBridge: Could not find any embedding model that outputs '{}'",
                        bridge_model_name
                    )))?;

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

                // Use the resolved internal name, not the public name from the request.
                let source_model_handle = ctx.model(&embed_resolved.internal_name)?;
                let source_config = source_model_handle.configuration().clone();
                let source_output = source_model_handle
                    .predict(&[texts_value], ctx.remaining())
                    .await?;

                // Keep usage when the source model reported it.
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

                let target_model_handle = ctx.model(&convert_resolved.internal_name)?;
                let target_config = target_model_handle.configuration().clone();
                let target_output = target_model_handle
                    .predict(&[embeddings_value], ctx.remaining())
                    .await?;

                let final_embeddings = crate::utils::extract_embeddings(target_output, &target_config)?;

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

    /// The restriction refusal must map to TARGET_RESTRICTED. A generic
    /// Prediction would make callers retry a policy decision.
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
        // A non-array `target_models` means no restrictions, not an error.
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

    /// Run an embed-bridge request (embed "m", convert into "t") and return the error.
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

    /// No embed model for the bridge source: `BridgePathNotFound`, not a generic `Prediction`.
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

    /// Embed leg present, no converter into the target: `ConverterNotFound`, naming m to t.
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
