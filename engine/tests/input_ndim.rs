//! Higher-rank `InputValue` accessors (`as_float_cube`, `as_float_quad`,
//! `as_float_penta`). Nested JSON becomes a 3-D, 4-D or 5-D tensor.
//! Malformed or wrong-rank input is rejected.

use engine::input::InputValue;
use serde_json::json;

fn iv(v: &serde_json::Value) -> InputValue<'_> {
    InputValue::new(v)
}

#[test]
fn float_cube_ok_shape_and_errors() {
    // Shape [1, 2, 2].
    let v = json!([[[1.0, 2.0], [3.0, 4.0]]]);
    let cube = iv(&v).as_float_cube().expect("valid 3D");
    assert_eq!(cube.shape(), &[1, 2, 2]);
    assert_eq!(cube[[0, 1, 0]], 3.0);

    // Wrong rank (1D) is an error.
    assert!(iv(&json!([1.0, 2.0, 3.0])).as_float_cube().is_err());
    // A ragged inner dimension is an error.
    assert!(iv(&json!([[[1.0, 2.0], [3.0]]])).as_float_cube().is_err());
}

#[test]
fn float_quad_ok_shape_and_errors() {
    // Shape [1, 1, 2, 2].
    let v = json!([[[[1.0, 2.0], [3.0, 4.0]]]]);
    let quad = iv(&v).as_float_quad().expect("valid 4D");
    assert_eq!(quad.shape(), &[1, 1, 2, 2]);

    // A 3D value is not a 4D tensor.
    assert!(iv(&json!([[[1.0, 2.0]]])).as_float_quad().is_err());
}

#[test]
fn float_penta_ok_shape_and_errors() {
    // Shape [1, 1, 1, 2, 2].
    let v = json!([[[[[1.0, 2.0], [3.0, 4.0]]]]]);
    let penta = iv(&v).as_float_penta().expect("valid 5D");
    assert_eq!(penta.shape(), &[1, 1, 1, 2, 2]);

    // A scalar is not a 5D array.
    assert!(iv(&json!(1.0)).as_float_penta().is_err());
}
