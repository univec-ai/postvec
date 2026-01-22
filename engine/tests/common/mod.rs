//! Shared helpers for the `engine` integration tests.
//!
//! Each test file is its own crate (`mod common;`), so not every helper is used
//! by every file — hence the broad `dead_code` allowance.
#![allow(dead_code)]

use ndarray::{Array, IxDyn};
use shared::vectors::{GenericTensor, TensorDataType, TensorValue};

pub const EPS: f64 = 1e-5;

#[track_caller]
pub fn assert_close(a: f64, b: f64) {
    assert!(
        (a - b).abs() <= EPS,
        "expected {a} ≈ {b} (|Δ| = {} > {EPS})",
        (a - b).abs()
    );
}

#[track_caller]
pub fn assert_slice_close(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len(), "length mismatch: {a:?} vs {b:?}");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert!(
            (*x as f64 - *y as f64).abs() <= EPS,
            "element {i} differs: {x} vs {y} (full: {a:?} vs {b:?})"
        );
    }
}

/// Build a Float32 `GenericTensor` of the given dynamic shape from row-major data.
pub fn f32_tensor(shape: &[usize], data: Vec<f32>) -> GenericTensor {
    let arr = Array::from_shape_vec(IxDyn(shape), data).expect("shape matches data length");
    GenericTensor {
        value: TensorValue::Float32(arr),
        shape: shape.to_vec(),
        dtype: TensorDataType::Numeric,
    }
}
