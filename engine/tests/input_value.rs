//! Tests for `engine::input::InputValue` — the fallible JSON→type conversion
//! helper used by every executor to read request fields. The
//! `as_string_or_string_list` path encodes the contract that pre-tokenised
//! (token-ID) inputs are explicitly rejected.

mod common;

use engine::input::InputValue;
use serde_json::json;

fn iv(v: &serde_json::Value) -> InputValue<'_> {
    InputValue::new(v)
}

// ---------------------------------------------------------------------------
// Scalar conversions
// ---------------------------------------------------------------------------

#[test]
fn as_string_ok_and_err() {
    let s = json!("hello");
    assert_eq!(iv(&s).as_string().unwrap(), "hello");
    let n = json!(42);
    assert!(iv(&n).as_string().is_err());
}

#[test]
fn as_i64_ok_and_err() {
    let n = json!(42);
    assert_eq!(iv(&n).as_i64().unwrap(), 42);
    let f = json!(1.5);
    assert!(iv(&f).as_i64().is_err());
}

#[test]
fn as_f64_accepts_int_and_float() {
    assert_eq!(iv(&json!(1.5)).as_f64().unwrap(), 1.5);
    assert_eq!(iv(&json!(3)).as_f64().unwrap(), 3.0);
    assert!(iv(&json!("x")).as_f64().is_err());
}

#[test]
fn as_bool_ok_and_err() {
    assert!(iv(&json!(true)).as_bool().unwrap());
    assert!(iv(&json!("true")).as_bool().is_err());
}

#[test]
fn as_u8_range_checked() {
    assert_eq!(iv(&json!(255)).as_u8().unwrap(), 255);
    assert!(iv(&json!(256)).as_u8().is_err()); // out of u8 range
    assert!(iv(&json!(-1)).as_u8().is_err());
}

// ---------------------------------------------------------------------------
// Collection conversions
// ---------------------------------------------------------------------------

#[test]
fn as_vec_and_string_list() {
    let arr = json!([1, 2, 3]);
    assert_eq!(iv(&arr).as_vec::<i64>().unwrap(), vec![1, 2, 3]);
    let strs = json!(["a", "b"]);
    assert_eq!(iv(&strs).as_string_list().unwrap(), vec!["a", "b"]);
}

#[test]
fn as_string_map() {
    let obj = json!({"a": 1, "b": 2});
    let map = iv(&obj).as_string_map::<i64>().unwrap();
    assert_eq!(map.get("a"), Some(&1));
    assert_eq!(map.get("b"), Some(&2));
}

#[test]
fn as_float_vector_and_matrix() {
    let v = json!([1.0, 2.0, 3.0]);
    assert_eq!(
        iv(&v).as_float_vector().unwrap().to_vec(),
        vec![1.0, 2.0, 3.0]
    );
    let m = json!([[1.0, 2.0], [3.0, 4.0]]);
    let mat = iv(&m).as_float_matrix().unwrap();
    assert_eq!(mat.shape(), &[2, 2]);
}

// ---------------------------------------------------------------------------
// as_string_or_string_list — the token-ID rejection contract
// ---------------------------------------------------------------------------

#[test]
fn single_string_becomes_one_element_list() {
    let s = json!("just one");
    assert_eq!(iv(&s).as_string_or_string_list().unwrap(), vec!["just one"]);
}

#[test]
fn array_of_strings_passes_through() {
    let arr = json!(["a", "b", "c"]);
    assert_eq!(
        iv(&arr).as_string_or_string_list().unwrap(),
        vec!["a", "b", "c"]
    );
}

#[test]
fn empty_array_rejected() {
    let arr = json!([]);
    assert!(iv(&arr).as_string_or_string_list().is_err());
}

#[test]
fn flat_token_id_array_rejected() {
    // [1,2,3] looks like token IDs → explicitly unsupported.
    let arr = json!([1, 2, 3]);
    let err = iv(&arr).as_string_or_string_list().unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("token id"),
        "expected a token-ID message, got: {err}"
    );
}

#[test]
fn nested_token_id_array_rejected() {
    // [[1,2],[3,4]] = batched token IDs → also unsupported.
    let arr = json!([[1, 2], [3, 4]]);
    let err = iv(&arr).as_string_or_string_list().unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("token id"),
        "got: {err}"
    );
}

#[test]
fn mixed_array_rejected() {
    let arr = json!(["a", 1]);
    assert!(iv(&arr).as_string_or_string_list().is_err());
}

#[test]
fn scalar_non_string_types_rejected() {
    assert!(iv(&json!(5)).as_string_or_string_list().is_err()); // number
    assert!(iv(&json!(null)).as_string_or_string_list().is_err()); // null
    assert!(iv(&json!(true)).as_string_or_string_list().is_err()); // bool
    assert!(iv(&json!({"k": "v"})).as_string_or_string_list().is_err()); // object
}
