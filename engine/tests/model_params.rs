//! Tests for `engine::models` — `GenericModel` / `BaseModel` behaviour and the
//! `param_*_or_default` typed-parameter accessors used throughout model setup.

use engine::models::base_model::BaseModel;
use engine::models::GenericModel;
use engine::{Model, ModelBackend, ModelConfiguration};
use serde_json::json;

fn model_with_params(params: serde_json::Value) -> GenericModel {
    let config: ModelConfiguration = serde_json::from_value(json!({
        "name": "m",
        "params": params,
    }))
    .unwrap();
    GenericModel::new(config)
}

#[test]
fn generic_model_is_valid_and_named() {
    let m = model_with_params(json!({}));
    assert!(m.valid());
    assert_eq!(m.name(), "m");
    // GenericModel forces its backend to Generic regardless of input config.
    assert_eq!(*m.backend(), ModelBackend::Generic);
}

#[test]
fn generic_model_query_errors() {
    let m = model_with_params(json!({}));
    // Placeholder model has no inference; query must error rather than panic.
    assert!(m.query(&[]).is_err());
}

#[test]
fn base_model_is_invalid_by_default() {
    let base = BaseModel {
        configuration: ModelConfiguration {
            name: "b".into(),
            ..Default::default()
        },
    };
    assert!(!base.valid());
    assert!(base.query(&[]).is_err());
    assert!(base.overview().is_err());
}

// ---------------------------------------------------------------------------
// param_*_or_default — present, missing, wrong-type
// ---------------------------------------------------------------------------

#[test]
fn param_int_present_missing_wrongtype() {
    let m = model_with_params(json!({"n": 7, "s": "hi"}));
    assert_eq!(m.param_int_or_default("n", -1), 7);
    assert_eq!(m.param_int_or_default("missing", -1), -1);
    // Present but wrong type → default.
    assert_eq!(m.param_int_or_default("s", -1), -1);
}

#[test]
fn param_f32_present_and_default() {
    let m = model_with_params(json!({"f": 1.5}));
    assert_eq!(m.param_f32_or_default("f", 0.0), 1.5);
    assert_eq!(m.param_f32_or_default("missing", 9.0), 9.0);
}

#[test]
fn param_bool_present_and_default() {
    let m = model_with_params(json!({"b": true}));
    assert!(m.param_bool_or_default("b", false));
    assert!(m.param_bool_or_default("missing", true));
    // Wrong type falls back.
    let m2 = model_with_params(json!({"b": "yes"}));
    assert!(!m2.param_bool_or_default("b", false));
}

#[test]
fn param_string_present_and_default() {
    let m = model_with_params(json!({"s": "hello"}));
    assert_eq!(m.param_string_or_default("s", "x"), "hello");
    assert_eq!(m.param_string_or_default("missing", "fallback"), "fallback");
}

#[test]
fn param_string_list_present_and_default() {
    let m = model_with_params(json!({"list": ["a", "b", 3]}));
    // Non-string elements are filtered out.
    assert_eq!(
        m.param_string_list_or_default("list", &["z"]),
        vec!["a", "b"]
    );
    assert_eq!(
        m.param_string_list_or_default("missing", &["z"]),
        vec!["z".to_string()]
    );
}

#[test]
fn param_list_present_and_default() {
    let m = model_with_params(json!({"l": [1, "two", true]}));
    let got = m.param_list_or_default("l", &[]);
    assert_eq!(got, vec![json!(1), json!("two"), json!(true)]);
    assert_eq!(
        m.param_list_or_default("missing", &[json!(0)]),
        vec![json!(0)]
    );
}
