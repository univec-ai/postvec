//! End-to-end tests for `shared::vectors::matrix_math` — the `MatrixMathExt`
//! trait over 2D float matrices.

mod common;

use shared::{AsFloatMatrix, FloatMatrix, MatrixMathExt, VectorError};

fn matrix(rows: &[&[f32]]) -> FloatMatrix {
    rows.iter()
        .map(|r| r.to_vec())
        .collect::<Vec<_>>()
        .as_float_matrix()
        .unwrap()
}

#[test]
fn column_extracts_and_bounds_checks() {
    let m = matrix(&[&[1.0, 2.0], &[3.0, 4.0], &[5.0, 6.0]]);
    // `column_checked` is reachable via method syntax (does not collide with
    // ndarray's panicking inherent `column`) and returns an owned, bounds-checked
    // result.
    common::assert_vec_close(&m.column_checked(1).unwrap(), &[2.0, 4.0, 6.0]);
    common::assert_vec_close(&m.column_checked(0).unwrap(), &[1.0, 3.0, 5.0]);
    assert!(matches!(
        m.column_checked(2),
        Err(VectorError::InvalidArgument(_))
    ));
}

#[test]
fn arg_max_rows_returns_index_and_value() {
    let m = matrix(&[&[1.0, 9.0, 3.0], &[7.0, 2.0, 5.0]]);
    let res = m.arg_max_rows();
    assert_eq!(res[0].0, 1);
    common::assert_close(res[0].1 as f64, 9.0);
    assert_eq!(res[1].0, 0);
    common::assert_close(res[1].1 as f64, 7.0);
}

#[test]
fn arg_max_rows_indices_only() {
    let m = matrix(&[&[1.0, 9.0, 3.0], &[7.0, 2.0, 5.0]]);
    assert_eq!(m.arg_max_rows_indices(), vec![1, 0]);
}

#[test]
fn grouped_by_reshapes_into_cube() {
    // 4 rows × 2 cols, grouped in sequences of 2 → (2, 2, 2).
    let m = matrix(&[&[1.0, 2.0], &[3.0, 4.0], &[5.0, 6.0], &[7.0, 8.0]]);
    let cube = m.grouped_by(2).unwrap();
    assert_eq!(cube.shape(), &[2, 2, 2]);
    // First group is the first two rows.
    assert_eq!(cube[[0, 0, 0]], 1.0);
    assert_eq!(cube[[0, 1, 1]], 4.0);
    assert_eq!(cube[[1, 0, 0]], 5.0);
}

#[test]
fn grouped_by_non_divisible_errors() {
    let m = matrix(&[&[1.0, 2.0], &[3.0, 4.0], &[5.0, 6.0]]);
    assert!(matches!(
        m.grouped_by(2),
        Err(VectorError::ReshapeError { .. })
    ));
}

#[test]
fn grouped_by_zero_sequence_errors() {
    let m = matrix(&[&[1.0, 2.0]]);
    assert!(matches!(
        m.grouped_by(0),
        Err(VectorError::ReshapeError { .. })
    ));
}

#[test]
fn as_sequences_sliding_window_with_padding() {
    // 5 rows × 2 cols, window 3, step 2.
    // Windows: rows[0..3], rows[2..5], rows[4..5]+1 padded row → 3 windows.
    let m = matrix(&[
        &[1.0, 1.0],
        &[2.0, 2.0],
        &[3.0, 3.0],
        &[4.0, 4.0],
        &[5.0, 5.0],
    ]);
    let seqs = m.as_sequences(3, 2).unwrap();
    assert_eq!(seqs.shape(), &[3, 3, 2]);
    // Last window starts at row 4 (value 5) then zero-padded.
    assert_eq!(seqs[[2, 0, 0]], 5.0);
    assert_eq!(seqs[[2, 1, 0]], 0.0);
    assert_eq!(seqs[[2, 2, 0]], 0.0);
}

#[test]
fn as_sequences_exact_fit_no_padding() {
    let m = matrix(&[&[1.0], &[2.0], &[3.0], &[4.0]]);
    let seqs = m.as_sequences(2, 2).unwrap();
    assert_eq!(seqs.shape(), &[2, 2, 1]);
    assert_eq!(seqs[[0, 0, 0]], 1.0);
    assert_eq!(seqs[[1, 1, 0]], 4.0);
}

#[test]
fn as_sequences_zero_params_error() {
    let m = matrix(&[&[1.0], &[2.0]]);
    assert!(matches!(
        m.as_sequences(0, 1),
        Err(VectorError::InvalidArgument(_))
    ));
    assert!(matches!(
        m.as_sequences(2, 0),
        Err(VectorError::InvalidArgument(_))
    ));
}
