//! End-to-end tests for `shared::vectors::ndarray_ops` — the `AsFloat*`
//! conversion traits, `NdArrayExt`, `ReshapeExt`, and `shape_match`.

mod common;

use shared::{
    shape_match, AsFloatCube, AsFloatMatrix, AsFloatPenta, AsFloatQuad, AsFloatVector, FloatVector,
    NdArrayExt, ReshapeExt, VectorError,
};

// ---------------------------------------------------------------------------
// AsFloat* conversions
// ---------------------------------------------------------------------------

#[test]
fn vec_to_float_vector() {
    // `AsFloatVector` requires `T: Into<f32>`, i.e. lossless conversions
    // (i16/u8/f32, etc.) — i32 is intentionally excluded.
    let v = vec![1i16, 2, 3].as_float_vector().unwrap();
    common::assert_vec_close(&v, &[1.0, 2.0, 3.0]);
}

#[test]
fn json_array_to_float_vector() {
    let json = serde_json::json!([1.5, 2.5, 3.5]);
    let v = (&json).as_float_vector().unwrap();
    common::assert_vec_close(&v, &[1.5, 2.5, 3.5]);
}

#[test]
fn json_non_array_rejected() {
    let json = serde_json::json!({"a": 1});
    assert!(matches!(
        (&json).as_float_vector(),
        Err(VectorError::ConversionError(_))
    ));
}

#[test]
fn json_array_with_non_number_rejected() {
    let json = serde_json::json!([1.0, "x", 3.0]);
    assert!(matches!(
        (&json).as_float_vector(),
        Err(VectorError::ConversionError(_))
    ));
}

#[test]
fn vec_to_float_matrix() {
    let m = vec![vec![1.0f32, 2.0], vec![3.0, 4.0]]
        .as_float_matrix()
        .unwrap();
    assert_eq!(m.shape(), &[2, 2]);
    common::assert_slice_close(m.as_slice().unwrap(), &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn empty_matrix_is_zero_by_zero() {
    let m = Vec::<Vec<f32>>::new().as_float_matrix().unwrap();
    assert_eq!(m.shape(), &[0, 0]);
}

#[test]
fn ragged_matrix_rejected() {
    let err = vec![vec![1.0f32, 2.0], vec![3.0]]
        .as_float_matrix()
        .unwrap_err();
    assert!(
        matches!(err, VectorError::ConversionError(_)),
        "got {err:?}"
    );
}

#[test]
fn vec_to_float_cube() {
    let c = vec![
        vec![vec![1.0f32, 2.0], vec![3.0, 4.0]],
        vec![vec![5.0, 6.0], vec![7.0, 8.0]],
    ]
    .as_float_cube()
    .unwrap();
    assert_eq!(c.shape(), &[2, 2, 2]);
    common::assert_slice_close(
        c.as_slice().unwrap(),
        &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
    );
}

#[test]
fn inconsistent_cube_rejected() {
    // Second matrix has a differing number of rows.
    let err = vec![
        vec![vec![1.0f32, 2.0], vec![3.0, 4.0]],
        vec![vec![5.0, 6.0]],
    ]
    .as_float_cube()
    .unwrap_err();
    assert!(
        matches!(err, VectorError::ConversionError(_)),
        "got {err:?}"
    );
}

#[test]
fn vec_to_float_quad_and_penta() {
    let q = vec![vec![vec![vec![1.0f32, 2.0]]]].as_float_quad().unwrap();
    assert_eq!(q.shape(), &[1, 1, 1, 2]);

    let p = vec![vec![vec![vec![vec![1.0f32, 2.0, 3.0]]]]]
        .as_float_penta()
        .unwrap();
    assert_eq!(p.shape(), &[1, 1, 1, 1, 3]);
}

#[test]
fn empty_higher_dims_have_expected_shapes() {
    assert_eq!(
        Vec::<Vec<Vec<f32>>>::new().as_float_cube().unwrap().shape(),
        &[0, 0, 0]
    );
    assert_eq!(
        Vec::<Vec<Vec<Vec<f32>>>>::new()
            .as_float_quad()
            .unwrap()
            .shape(),
        &[0, 0, 0, 0]
    );
    assert_eq!(
        Vec::<Vec<Vec<Vec<Vec<f32>>>>>::new()
            .as_float_penta()
            .unwrap()
            .shape(),
        &[0, 0, 0, 0, 0]
    );
}

// ---------------------------------------------------------------------------
// NdArrayExt
// ---------------------------------------------------------------------------

#[test]
fn expand_dimension_adds_leading_axis() {
    let v: FloatVector = FloatVector::from(vec![1.0, 2.0, 3.0]);
    let m = v.expand_dimension();
    assert_eq!(m.shape(), &[1, 3]);

    let cube = m.expand_dimension();
    assert_eq!(cube.shape(), &[1, 1, 3]);
}

#[test]
fn flatten_to_vector_from_matrix() {
    let m = vec![vec![1.0f32, 2.0], vec![3.0, 4.0]]
        .as_float_matrix()
        .unwrap();
    let flat = m.flatten_to_vector();
    common::assert_vec_close(&flat, &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn get_shape_reports_dims() {
    let m = vec![vec![1.0f32, 2.0, 3.0]].as_float_matrix().unwrap();
    assert_eq!(m.get_shape(), &[1, 3]);
}

// ---------------------------------------------------------------------------
// ReshapeExt
// ---------------------------------------------------------------------------

#[test]
fn reshape_2d_3d_4d_5d() {
    let v = FloatVector::from((0..24).map(|x| x as f32).collect::<Vec<_>>());
    assert_eq!(v.clone().reshape_2d(4, 6).unwrap().shape(), &[4, 6]);
    assert_eq!(v.clone().reshape_3d(2, 3, 4).unwrap().shape(), &[2, 3, 4]);
    assert_eq!(
        v.clone().reshape_4d(2, 3, 2, 2).unwrap().shape(),
        &[2, 3, 2, 2]
    );
    assert_eq!(
        v.reshape_5d(1, 2, 3, 2, 2).unwrap().shape(),
        &[1, 2, 3, 2, 2]
    );
}

#[test]
fn reshape_size_mismatch_errors() {
    let v = FloatVector::from(vec![1.0f32, 2.0, 3.0, 4.0, 5.0]);
    assert!(matches!(
        v.clone().reshape_2d(2, 3),
        Err(VectorError::ReshapeError { .. })
    ));
    assert!(matches!(
        v.reshape_3d(2, 2, 2),
        Err(VectorError::ReshapeError { .. })
    ));
}

#[test]
fn reshape_preserves_row_major_order() {
    let v = FloatVector::from(vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let m = v.reshape_2d(2, 3).unwrap();
    assert_eq!(m[[0, 0]], 1.0);
    assert_eq!(m[[0, 2]], 3.0);
    assert_eq!(m[[1, 0]], 4.0);
}

// ---------------------------------------------------------------------------
// shape_match
// ---------------------------------------------------------------------------

#[test]
fn shape_match_exact() {
    assert!(shape_match(&[2, 3], &[2, 3], true));
    assert!(!shape_match(&[2, 3], &[2, 4], true));
}

#[test]
fn shape_match_rank_mismatch_without_wildcard() {
    assert!(!shape_match(&[2, 3], &[2, 3, 4], true));
    assert!(!shape_match(&[2, 3], &[2, 3, 4], false));
}

#[test]
fn shape_match_rank_only_when_not_checking_dims() {
    // Same rank, differing concrete dims, but dimension_check = false.
    assert!(shape_match(&[2, 3], &[7, 9], false));
}

#[test]
fn shape_match_wildcard_is_positional() {
    // A 0 acts as a wildcard at its own position; the surrounding dims must
    // still line up. This is the case the previous (filter-both) implementation
    // got wrong.
    assert!(shape_match(&[2, 0, 3], &[2, 5, 3], true));
    assert!(shape_match(&[2, 5, 3], &[2, 0, 3], true));
    assert!(!shape_match(&[2, 0, 3], &[9, 5, 3], true)); // leading dim disagrees
    assert!(shape_match(&[0, 0], &[5, 7], true)); // all wildcards
}

#[test]
fn shape_match_wildcard_absorbs_rank_difference() {
    // Differing ranks are permitted when a wildcard is present; the concrete
    // dims are compared in order.
    assert!(shape_match(&[2, 0], &[2, 3, 4], true));
    assert!(!shape_match(&[2, 0, 9], &[2, 3, 4], true)); // concrete 9 vs 4
}
