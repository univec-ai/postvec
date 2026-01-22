//! Tests for `engine::utils::extract_embeddings` — the helper bridge executors
//! (convert-bridge / embed-bridge) use to pull the embedding payload out of a
//! sub-model's `ExecutorOutput` regardless of its shape.

use engine::executors::{ExecutionMetadata, ExecutorOutput};
use engine::utils::extract_embeddings;
use engine::{ExecutorConfiguration, ModelConfiguration, OutputMapping};
use serde_json::json;

fn config_with_output_key(key: &str) -> ModelConfiguration {
    ModelConfiguration {
        executor: ExecutorConfiguration {
            outputs: vec![OutputMapping {
                json_key: key.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn structured_returns_first_value() {
    let out = ExecutorOutput::Structured(vec![json!([[1.0, 2.0]]), json!("ignored")]);
    let cfg = ModelConfiguration::default();
    assert_eq!(extract_embeddings(out, &cfg).unwrap(), json!([[1.0, 2.0]]));
}

#[test]
fn structured_empty_errors() {
    let out = ExecutorOutput::Structured(vec![]);
    assert!(extract_embeddings(out, &ModelConfiguration::default()).is_err());
}

#[test]
fn structured_with_usage_returns_first_output() {
    let out = ExecutorOutput::StructuredWithUsage {
        outputs: vec![json!([[9.0]]), json!("x")],
        usage: ExecutionMetadata {
            total_tokens: 7,
            ..Default::default()
        },
    };
    assert_eq!(
        extract_embeddings(out, &ModelConfiguration::default()).unwrap(),
        json!([[9.0]])
    );
}

#[test]
fn structured_with_usage_empty_errors() {
    let out = ExecutorOutput::StructuredWithUsage {
        outputs: vec![],
        usage: ExecutionMetadata::default(),
    };
    assert!(extract_embeddings(out, &ModelConfiguration::default()).is_err());
}

#[test]
fn json_uses_configured_output_key() {
    let out = ExecutorOutput::Json(json!({"vectors": [[1.0, 2.0]], "other": 1}));
    let cfg = config_with_output_key("vectors");
    assert_eq!(extract_embeddings(out, &cfg).unwrap(), json!([[1.0, 2.0]]));
}

#[test]
fn json_falls_back_to_embeddings_key_when_no_config() {
    let out = ExecutorOutput::Json(json!({"embeddings": [[3.0]]}));
    // No outputs configured → defaults to the "embeddings" key.
    assert_eq!(
        extract_embeddings(out, &ModelConfiguration::default()).unwrap(),
        json!([[3.0]])
    );
}

#[test]
fn json_missing_key_returns_whole_object() {
    let obj = json!({"unexpected": [[1.0]]});
    let out = ExecutorOutput::Json(obj.clone());
    let cfg = config_with_output_key("vectors");
    // Key not present → the entire object is returned as a fallback.
    assert_eq!(extract_embeddings(out, &cfg).unwrap(), obj);
}

#[test]
fn binary_output_errors() {
    let out = ExecutorOutput::Binary {
        data: vec![1, 2, 3],
        content_type: "application/octet-stream".into(),
    };
    assert!(extract_embeddings(out, &ModelConfiguration::default()).is_err());
}
