//! End-to-end tests for `shared::vectors::tensor` — the `GenericTensor` /
//! `TensorValue` data path, which is by far the most heavily-used part of the
//! crate (the engine builds tensors for every inference request).

mod common;

use shared::vectors::{
    new_2d_tensor_from_float_vecs, new_2d_tensor_from_i64_vecs, new_3d_tensor_from_float_vecs,
    new_dictionary_tensor, new_string_tensor, new_tensor_with_shape,
    new_tensor_with_shape_inference, GenericTensor, IntoGenericTensor, TensorDataType, TensorValue,
};
use shared::VectorError;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

#[test]
fn i64_2d_constructor_sets_shape_and_values() {
    let t = new_2d_tensor_from_i64_vecs(vec![vec![1, 2, 3], vec![4, 5, 6]]).unwrap();
    assert_eq!(t.shape, vec![2, 3]);
    assert_eq!(t.dtype, TensorDataType::Numeric);
    let view = t.as_int64_array().unwrap();
    assert_eq!(view.shape(), &[2, 3]);
    assert_eq!(
        view.iter().copied().collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
}

#[test]
fn i64_2d_constructor_empty_is_zero_by_zero() {
    let t = new_2d_tensor_from_i64_vecs(vec![]).unwrap();
    assert_eq!(t.shape, vec![0, 0]);
    assert_eq!(t.value.len(), 0);
    assert!(t.value.is_empty());
}

#[test]
fn i64_2d_constructor_rejects_ragged_rows() {
    // 2 rows claimed at 3 cols but second row has 2 → flat len 5 != 2*3.
    let err = new_2d_tensor_from_i64_vecs(vec![vec![1, 2, 3], vec![4, 5]]).unwrap_err();
    // ndarray shape error surfaces as ShapeError via the `?` conversion.
    assert!(matches!(err, VectorError::ShapeError(_)), "got {err:?}");
}

#[test]
fn f32_2d_constructor_roundtrips() {
    let t = new_2d_tensor_from_float_vecs(vec![vec![1.5, 2.5], vec![3.5, 4.5]]).unwrap();
    assert_eq!(t.shape, vec![2, 2]);
    let m = t.as_float_matrix().unwrap();
    common::assert_slice_close(m.as_slice().unwrap(), &[1.5, 2.5, 3.5, 4.5]);
}

#[test]
fn f32_2d_constructor_empty() {
    let t = new_2d_tensor_from_float_vecs(vec![]).unwrap();
    assert_eq!(t.shape, vec![0, 0]);
}

#[test]
fn f32_3d_constructor_shape_and_flatten() {
    let t = new_3d_tensor_from_float_vecs(vec![vec![vec![1.0, 2.0], vec![3.0, 4.0]]]).unwrap();
    assert_eq!(t.shape, vec![1, 2, 2]);
    let cube = t.as_float_cube().unwrap();
    assert_eq!(cube.shape(), &[1, 2, 2]);
    common::assert_slice_close(cube.as_slice().unwrap(), &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn f32_3d_constructor_empty() {
    let t = new_3d_tensor_from_float_vecs(vec![]).unwrap();
    assert_eq!(t.shape, vec![0, 0, 0]);
}

#[test]
fn string_and_dictionary_constructors() {
    let s = new_string_tensor("hello".into());
    assert_eq!(s.dtype, TensorDataType::String);
    assert!(s.shape.is_empty());
    assert_eq!(s.value.len(), 1);

    let mut map = HashMap::new();
    map.insert("a".to_string(), new_string_tensor("x".into()));
    map.insert("b".to_string(), new_string_tensor("y".into()));
    let d = new_dictionary_tensor(map);
    assert_eq!(d.dtype, TensorDataType::Dictionary);
    assert_eq!(d.value.len(), 2);
}

// ---------------------------------------------------------------------------
// IntoGenericTensor (numeric heuristic + shape inference)
// ---------------------------------------------------------------------------

#[test]
fn into_tensor_integer_vec_picks_int64() {
    let t = new_tensor_with_shape_inference(vec![1i32, 2, 3]).unwrap();
    assert_eq!(t.shape, vec![3]);
    // Integral values round-trip losslessly so the heuristic stores Int64.
    assert!(matches!(t.value, TensorValue::Int64(_)), "expected Int64");
    assert_eq!(
        t.as_int64_array()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn into_tensor_fractional_vec_picks_float32() {
    let t = new_tensor_with_shape_inference(vec![1.5f64, 2.25]).unwrap();
    assert_eq!(t.shape, vec![2]);
    assert!(
        matches!(t.value, TensorValue::Float32(_)),
        "expected Float32"
    );
}

#[test]
fn into_tensor_whole_float_is_float32_not_int64() {
    // serde_json serialises f64 `1.0` as `1.0`; `as_i64()` returns None for it,
    // so the i64/f32 length-equality heuristic falls back to Float32. This
    // documents the (intentional) behaviour for whole-valued floats.
    let t = new_tensor_with_shape_inference(vec![1.0f64, 2.0]).unwrap();
    assert!(
        matches!(t.value, TensorValue::Float32(_)),
        "expected Float32"
    );
}

#[test]
fn into_tensor_nested_infers_2d_shape() {
    let t = new_tensor_with_shape_inference(vec![vec![1i64, 2, 3], vec![4, 5, 6]]).unwrap();
    assert_eq!(t.shape, vec![2, 3]);
    assert!(matches!(t.value, TensorValue::Int64(_)));
}

#[test]
fn into_tensor_from_json_value() {
    let json: serde_json::Value = serde_json::json!([[1.0, 2.0], [3.0, 4.0]]);
    let t: GenericTensor = (&json).into_generic_tensor().unwrap();
    assert_eq!(t.shape, vec![2, 2]);
    let m = t.as_float_matrix().unwrap();
    common::assert_slice_close(m.as_slice().unwrap(), &[1.0, 2.0, 3.0, 4.0]);
}

// ---------------------------------------------------------------------------
// new_tensor_with_shape (inference + reshape)
// ---------------------------------------------------------------------------

#[test]
fn new_tensor_with_shape_reshapes() {
    let t = new_tensor_with_shape(vec![1i64, 2, 3, 4, 5, 6], &[2, 3]).unwrap();
    assert_eq!(t.shape, vec![2, 3]);
}

#[test]
fn new_tensor_with_shape_infers_dynamic_dim() {
    let t = new_tensor_with_shape(vec![1i64, 2, 3, 4, 5, 6], &[-1, 3]).unwrap();
    assert_eq!(t.shape, vec![2, 3]);
}

// ---------------------------------------------------------------------------
// GenericTensor::reshape
// ---------------------------------------------------------------------------

#[test]
fn reshape_empty_shape_is_noop() {
    let t = new_tensor_with_shape_inference(vec![1i64, 2, 3]).unwrap();
    let r = t.clone().reshape(&[]).unwrap();
    assert_eq!(r.shape, t.shape);
}

#[test]
fn reshape_single_dynamic_dim_infers() {
    let t = new_tensor_with_shape_inference(vec![1.5f32, 2.5, 3.5, 4.5, 5.5, 6.5]).unwrap();
    let r = t.reshape(&[-1, 2]).unwrap();
    assert_eq!(r.shape, vec![3, 2]);
}

#[test]
fn reshape_rejects_two_dynamic_dims() {
    let t = new_tensor_with_shape_inference(vec![1.0f32, 2.0, 3.0, 4.0]).unwrap();
    let err = t.reshape(&[-1, -1]).unwrap_err();
    assert!(
        matches!(err, VectorError::ReshapeError { .. }),
        "got {err:?}"
    );
}

#[test]
fn reshape_rejects_size_mismatch() {
    let t = new_tensor_with_shape_inference(vec![1.0f32, 2.0, 3.0, 4.0]).unwrap();
    let err = t.reshape(&[3, 2]).unwrap_err();
    assert!(
        matches!(err, VectorError::ReshapeError { .. }),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Conversions out of GenericTensor
// ---------------------------------------------------------------------------

#[test]
fn try_into_vector_flattens_numeric() {
    let t = new_2d_tensor_from_float_vecs(vec![vec![1.0, 2.0], vec![3.0, 4.0]]).unwrap();
    let v = t.try_into_vector().unwrap();
    common::assert_vec_close(&v, &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn try_into_vector_rejects_string() {
    let err = new_string_tensor("x".into()).try_into_vector().unwrap_err();
    assert!(
        matches!(err, VectorError::ConversionError(_)),
        "got {err:?}"
    );
}

#[test]
fn as_float_vector_casts_int64() {
    // Int64 tensor flattened to f32 vector (lossy cast path).
    let t = new_2d_tensor_from_i64_vecs(vec![vec![1, 2], vec![3, 4]]).unwrap();
    let v = t.as_float_vector().unwrap();
    common::assert_vec_close(&v, &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn as_float_matrix_wrong_rank_errors() {
    let t = new_tensor_with_shape_inference(vec![1.0f32, 2.0, 3.0]).unwrap(); // 1D
    let err = t.as_float_matrix().unwrap_err();
    assert!(
        matches!(err, VectorError::DimensionMismatch { .. }),
        "got {err:?}"
    );
}

#[test]
fn as_float_cube_quad_penta_roundtrip() {
    let cube = new_tensor_with_shape(vec![0.0f32; 24], &[2, 3, 4])
        .unwrap()
        .as_float_cube()
        .unwrap();
    assert_eq!(cube.shape(), &[2, 3, 4]);

    let quad = new_tensor_with_shape(vec![0.0f32; 24], &[1, 2, 3, 4])
        .unwrap()
        .as_float_quad()
        .unwrap();
    assert_eq!(quad.shape(), &[1, 2, 3, 4]);

    let penta = new_tensor_with_shape(vec![0.0f32; 24], &[1, 1, 2, 3, 4])
        .unwrap()
        .as_float_penta()
        .unwrap();
    assert_eq!(penta.shape(), &[1, 1, 2, 3, 4]);
}

#[test]
fn as_int64_array_rejects_float() {
    let t = new_2d_tensor_from_float_vecs(vec![vec![1.0, 2.0]]).unwrap();
    let err = t.as_int64_array().unwrap_err();
    assert!(
        matches!(err, VectorError::ConversionError(_)),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// TensorValue helpers
// ---------------------------------------------------------------------------

#[test]
fn tensor_value_len_and_is_empty_per_variant() {
    let f = TensorValue::Float32(ndarray::Array::zeros(ndarray::IxDyn(&[2, 3])));
    assert_eq!(f.len(), 6);
    assert!(!f.is_empty());

    let s = TensorValue::String("hi".into());
    assert_eq!(s.len(), 1); // strings count as a single logical element

    let empty = TensorValue::Int64(ndarray::Array::zeros(ndarray::IxDyn(&[0])));
    assert!(empty.is_empty());
}

#[test]
fn tensor_value_reshape_numeric_and_reject_string() {
    let f = TensorValue::Float32(ndarray::Array::zeros(ndarray::IxDyn(&[6])));
    let r = f.reshape(&[2, 3]).unwrap();
    assert_eq!(r.get_shape(), &[2, 3]);

    let s = TensorValue::String("x".into());
    assert!(matches!(
        s.reshape(&[1]),
        Err(VectorError::ConversionError(_))
    ));
}

#[test]
fn tensor_value_flatten_int64_casts_to_f32() {
    let i = TensorValue::Int64(
        ndarray::Array::from_shape_vec(ndarray::IxDyn(&[3]), vec![7i64, 8, 9]).unwrap(),
    );
    common::assert_slice_close(&i.flatten_to_f32_vec(), &[7.0, 8.0, 9.0]);
}

#[test]
fn tensor_value_try_into_ndarray_i64_requires_2d() {
    let ok = TensorValue::Int64(
        ndarray::Array::from_shape_vec(ndarray::IxDyn(&[2, 2]), vec![1i64, 2, 3, 4]).unwrap(),
    );
    assert!(ok.try_into_ndarray_i64().is_ok());

    let one_d = TensorValue::Int64(
        ndarray::Array::from_shape_vec(ndarray::IxDyn(&[4]), vec![1i64, 2, 3, 4]).unwrap(),
    );
    assert!(one_d.try_into_ndarray_i64().is_err());
}

#[test]
fn serde_roundtrip_preserves_tensor() {
    let t = new_2d_tensor_from_i64_vecs(vec![vec![1, 2], vec![3, 4]]).unwrap();
    let json = serde_json::to_string(&t).unwrap();
    let back: GenericTensor = serde_json::from_str(&json).unwrap();
    assert_eq!(t, back);
}
