//! Tests for the higher-rank `InputValue` accessors (`as_float_cube` /
//! `as_float_quad` / `as_float_penta`). These convert nested JSON arrays into
//! 3-, 4-, and 5-D `ndarray` tensors and reject malformed / wrong-rank input.

use engine::input::InputValue;
use serde_json::json;

fn iv(v: &serde_json::Value) -> InputValue<'_> {
    InputValue::new(v)
}

#[test]
fn float_cube_ok_shape_and_errors() {
    // 1 × 2 × 2 cube.
    let v = json!([[[1.0, 2.0], [3.0, 4.0]]]);
    let cube = iv(&v).as_float_cube().expect("valid 3D");
    assert_eq!(cube.shape(), &[1, 2, 2]);
    assert_eq!(cube[[0, 1, 0]], 3.0);

    // Wrong rank (1D) → error.
    assert!(iv(&json!([1.0, 2.0, 3.0])).as_float_cube().is_err());
    // Ragged inner dimension → error.
    assert!(iv(&json!([[[1.0, 2.0], [3.0]]])).as_float_cube().is_err());
}

#[test]
fn float_quad_ok_shape_and_errors() {
    // 1 × 1 × 2 × 2 quad.
    let v = json!([[[[1.0, 2.0], [3.0, 4.0]]]]);
    let quad = iv(&v).as_float_quad().expect("valid 4D");
    assert_eq!(quad.shape(), &[1, 1, 2, 2]);

    // A 3D structure cannot deserialize into a 4D nesting → error.
    assert!(iv(&json!([[[1.0, 2.0]]])).as_float_quad().is_err());
}

#[test]
fn float_penta_ok_shape_and_errors() {
    // 1 × 1 × 1 × 2 × 2 penta.
    let v = json!([[[[[1.0, 2.0], [3.0, 4.0]]]]]);
    let penta = iv(&v).as_float_penta().expect("valid 5D");
    assert_eq!(penta.shape(), &[1, 1, 1, 2, 2]);

    // Scalar is not a 5D array → error.
    assert!(iv(&json!(1.0)).as_float_penta().is_err());
}
