//! Tests for `engine::new_executor` — the factory that maps an
//! `executor.key` to a concrete executor. Only keys whose constructors don't
//! require on-disk model assets are exercised here.

use engine::{new_executor, EngineConfig, InferenceEngine, ModelConfiguration};
use serde_json::json;
use std::sync::Arc;

fn empty_engine() -> InferenceEngine {
    let config = Arc::new(EngineConfig {
        root_path: std::env::temp_dir().join("engine_factory_test_root"),
        host_policy: Default::default(),
    });
    InferenceEngine::new(config)
}

fn config_with_key(key: &str) -> ModelConfiguration {
    serde_json::from_value(json!({
        "name": "m",
        "executor": { "key": key }
    }))
    .unwrap()
}

#[test]
fn empty_key_defaults_to_passthru() {
    let engine = empty_engine();
    let cfg = ModelConfiguration {
        name: "m".into(),
        ..Default::default()
    };
    // No key set → factory must fall back to "passthru" without error.
    assert!(new_executor(&engine, &cfg).is_ok());
}

#[test]
fn known_generic_keys_construct() {
    let engine = empty_engine();
    for key in [
        "passthru",
        "dummy",
        "vector-embedding",
        "embed-bridge",
        "convert-bridge",
    ] {
        let cfg = config_with_key(key);
        assert!(
            new_executor(&engine, &cfg).is_ok(),
            "expected '{key}' to construct"
        );
    }
}

#[test]
fn unknown_key_errors() {
    let engine = empty_engine();
    let cfg = config_with_key("does-not-exist");
    // `dyn Executor` has no Debug impl, so match rather than `unwrap_err()`.
    match new_executor(&engine, &cfg) {
        Err(e) => assert!(e.to_string().contains("Unknown executor key"), "got: {e}"),
        Ok(_) => panic!("expected an error for an unknown executor key"),
    }
}

#[test]
fn embed_bridge_constructs_with_restrictions_config() {
    // The EmbedBridge constructor parses `params.restrictions.target_models`;
    // a well-formed restrictions block must construct successfully.
    let engine = empty_engine();
    let cfg: ModelConfiguration = serde_json::from_value(json!({
        "name": "bridge",
        "executor": {
            "key": "embed-bridge",
            "params": {
                "restrictions": { "target_models": ["text-embedding-ada-002"] }
            }
        }
    }))
    .unwrap();
    assert!(new_executor(&engine, &cfg).is_ok());
}
