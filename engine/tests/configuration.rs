//! Tests for `engine::models::configuration` — `ModelConfiguration` parsing,
//! serde defaults/skips, `from_file`, `override_with`, and the supporting enums.
//! This config is the contract every `ninference.hub.json` is parsed against.

use engine::models::configuration::{ExecutionProvider, ModelBackend, QuantizationMode};
use engine::pooling::PoolingStrategy;
use engine::{ExecutorConfiguration, ModelConfiguration, OutputMapping};
use std::io::Write;

// ---------------------------------------------------------------------------
// Defaults & enums
// ---------------------------------------------------------------------------

#[test]
fn default_configuration_values() {
    let c = ModelConfiguration::default();
    assert_eq!(c.name, "");
    assert!(!c.enabled);
    assert_eq!(c.backend, ModelBackend::Generic);
    assert!(c.file_path.is_none());
    assert!(c.execution_providers.is_empty());
    assert_eq!(c.quantization, QuantizationMode::None);
    assert!(c.dependencies.is_empty());
    assert_eq!(c.executor, ExecutorConfiguration::default());
}

#[test]
fn model_backend_display_is_kebab_case() {
    assert_eq!(ModelBackend::Generic.to_string(), "generic");
    assert_eq!(ModelBackend::OnnxRuntime.to_string(), "onnx-runtime");
    assert_eq!(
        ModelBackend::LlamaCppEmbedding.to_string(),
        "llama-cpp-embedding"
    );
}

#[test]
fn execution_provider_serde_kebab() {
    let eps: Vec<ExecutionProvider> =
        serde_json::from_str(r#"["cpu", "cuda", "tensor-rt"]"#).unwrap();
    assert_eq!(
        eps,
        vec![
            ExecutionProvider::Cpu,
            ExecutionProvider::Cuda,
            ExecutionProvider::TensorRt
        ]
    );
}

#[test]
fn quantization_mode_serde_rename() {
    let q: QuantizationMode = serde_json::from_str("\"dynamic\"").unwrap();
    assert_eq!(q, QuantizationMode::Dynamic);
    assert_eq!(
        serde_json::to_string(&QuantizationMode::Static).unwrap(),
        "\"static\""
    );
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_minimal_config_applies_defaults() {
    // Only `name` present; everything else defaults.
    let c: ModelConfiguration = serde_json::from_str(r#"{"name": "m1"}"#).unwrap();
    assert_eq!(c.name, "m1");
    assert!(!c.enabled);
    assert_eq!(c.backend, ModelBackend::Generic);
    assert_eq!(c.executor.key, "");
}

#[test]
fn parse_full_executor_block() {
    let json = r#"{
        "name": "gte",
        "enabled": true,
        "backend": "onnx-runtime",
        "execution_providers": ["cuda", "cpu"],
        "executor": {
            "key": "transformer-sequence-embedding",
            "inputs": [{"json_key": "texts"}],
            "outputs": [{"json_key": "embeddings", "pooling_strategy": "mean", "normalize": true}],
            "params": {"pool_size": 4}
        }
    }"#;
    let c: ModelConfiguration = serde_json::from_str(json).unwrap();
    assert!(c.enabled);
    assert_eq!(c.backend, ModelBackend::OnnxRuntime);
    assert_eq!(
        c.execution_providers,
        vec![ExecutionProvider::Cuda, ExecutionProvider::Cpu]
    );
    assert_eq!(c.executor.key, "transformer-sequence-embedding");
    assert_eq!(c.executor.inputs.len(), 1);
    assert_eq!(c.executor.inputs[0].json_key, "texts");
    let out = &c.executor.outputs[0];
    assert_eq!(out.json_key, "embeddings");
    assert_eq!(out.pooling_strategy, Some(PoolingStrategy::Mean));
    assert!(out.normalize);
    assert_eq!(
        c.executor.params.get("pool_size").and_then(|v| v.as_u64()),
        Some(4)
    );
}

#[test]
fn output_mapping_optional_fields_default() {
    let out: OutputMapping = serde_json::from_str(r#"{"json_key": "e"}"#).unwrap();
    assert!(out.layer_name.is_none());
    assert!(out.pooling_strategy.is_none());
    assert!(!out.normalize);
}

#[test]
fn serialize_skips_empty_collections_and_none_file_path() {
    let c = ModelConfiguration {
        name: "m".into(),
        enabled: true,
        ..Default::default()
    };
    let json = serde_json::to_string(&c).unwrap();
    // Empty collections and a None file_path are omitted.
    assert!(!json.contains("execution_providers"), "{json}");
    assert!(!json.contains("file_path"), "{json}");
    assert!(!json.contains("\"params\""), "{json}");
    // NOTE: `dependencies` is the one collection field WITHOUT
    // `skip_serializing_if`, so an empty `dependencies: []` is always emitted.
    // This is harmless (it round-trips to an empty vec) but inconsistent with
    // the other collections; asserted here to pin the current behaviour.
    assert!(json.contains("\"dependencies\":[]"), "{json}");
}

#[test]
fn roundtrip_preserves_config() {
    let json = r#"{"name":"m","enabled":true,"backend":"candle","dependencies":["dep-a"]}"#;
    let c: ModelConfiguration = serde_json::from_str(json).unwrap();
    let back: ModelConfiguration =
        serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(c, back);
}

#[test]
fn from_file_reads_and_parses() {
    let dir = std::env::temp_dir().join(format!("engine_cfg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ninference.hub.json");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(br#"{"name": "from-disk", "enabled": true, "backend": "candle"}"#)
            .unwrap();
    }
    let c = ModelConfiguration::from_file(&path).unwrap();
    assert_eq!(c.name, "from-disk");
    assert!(c.enabled);
    assert_eq!(c.backend, ModelBackend::Candle);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn from_file_missing_errors() {
    assert!(ModelConfiguration::from_file("/no/such/ninference.hub.json").is_err());
}

// ---------------------------------------------------------------------------
// override_with
// ---------------------------------------------------------------------------

#[test]
fn override_with_replaces_scalars_and_merges_params() {
    let mut base: ModelConfiguration =
        serde_json::from_str(r#"{"name":"base","enabled":false,"params":{"a":1,"b":2}}"#).unwrap();
    let other: ModelConfiguration = serde_json::from_str(
        r#"{"name":"override","enabled":true,"backend":"candle","params":{"b":20,"c":3}}"#,
    )
    .unwrap();

    base.override_with(&other);

    assert_eq!(base.name, "override");
    assert!(base.enabled);
    assert_eq!(base.backend, ModelBackend::Candle);
    // params are merged key-by-key: a kept, b overwritten, c added.
    assert_eq!(base.params.get("a").and_then(|v| v.as_i64()), Some(1));
    assert_eq!(base.params.get("b").and_then(|v| v.as_i64()), Some(20));
    assert_eq!(base.params.get("c").and_then(|v| v.as_i64()), Some(3));
}

#[test]
fn override_with_keeps_file_path_when_other_is_none() {
    let mut base = ModelConfiguration {
        name: "b".into(),
        file_path: Some("/models/base.onnx".into()),
        ..Default::default()
    };
    let other = ModelConfiguration {
        name: "o".into(),
        file_path: None,
        ..Default::default()
    };
    base.override_with(&other);
    // `other.file_path` is None, so the base path is preserved.
    assert_eq!(base.file_path.as_deref(), Some("/models/base.onnx"));
}

#[test]
fn override_with_replaces_executor_wholesale() {
    let mut base: ModelConfiguration = serde_json::from_str(
        r#"{"name":"b","executor":{"key":"old","inputs":[{"json_key":"x"}]}}"#,
    )
    .unwrap();
    let other: ModelConfiguration =
        serde_json::from_str(r#"{"name":"o","executor":{"key":"new"}}"#).unwrap();
    base.override_with(&other);
    // executor is replaced entirely, not merged.
    assert_eq!(base.executor.key, "new");
    assert!(base.executor.inputs.is_empty());
}
