//! End-to-end tests for `shared::vectors::utils::TryIntoFloatX`.
//!
//! `TryIntoFloatX` is not re-exported at the crate root, so it is referenced
//! through its full module path.

mod common;

use shared::vectors::utils::TryIntoFloatX;
use shared::VectorError;

#[test]
fn f64_and_f32_convert() {
    common::assert_close(2.5f64.try_into_float_x().unwrap() as f64, 2.5);
    common::assert_close(2.5f32.try_into_float_x().unwrap() as f64, 2.5);
}

#[test]
fn i64_converts() {
    common::assert_close(42i64.try_into_float_x().unwrap() as f64, 42.0);
}

#[test]
fn str_parses_or_errors() {
    common::assert_close("3.25".try_into_float_x().unwrap() as f64, 3.25);
    assert!(matches!(
        "not-a-number".try_into_float_x(),
        Err(VectorError::ParseFloatError(_))
    ));
}

#[test]
fn json_number_converts_and_non_number_errors() {
    let num = serde_json::json!(7.5);
    common::assert_close(num.try_into_float_x().unwrap() as f64, 7.5);

    let not_num = serde_json::json!("hello");
    assert!(matches!(
        not_num.try_into_float_x(),
        Err(VectorError::ConversionError(_))
    ));
}
