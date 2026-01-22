//! End-to-end tests for `shared::vectors::types` — `PairList` ordering,
//! scaling, and the `IntoGenericTensorExt::to_tensor` conversions.

mod common;

use shared::vectors::types::{Pair, PairList};
use shared::{FloatMatrix, FloatVector, IntoGenericTensorExt, TensorDataType, VectorError};

fn pl(pairs: &[(&str, f64)]) -> PairList {
    PairList(
        pairs
            .iter()
            .map(|(k, v)| Pair {
                key: k.to_string(),
                value: *v,
            })
            .collect(),
    )
}

#[test]
fn len_and_is_empty() {
    assert!(PairList::default().is_empty());
    assert_eq!(PairList::default().len(), 0);
    let p = pl(&[("a", 1.0), ("b", 2.0)]);
    assert_eq!(p.len(), 2);
    assert!(!p.is_empty());
}

#[test]
fn sort_by_value_ascending() {
    let mut p = pl(&[("a", 3.0), ("b", 1.0), ("c", 2.0)]);
    p.sort_by_value();
    let keys: Vec<_> = p.0.iter().map(|x| x.key.as_str()).collect();
    assert_eq!(keys, vec!["b", "c", "a"]);
}

#[test]
fn sort_by_value_and_key_tiebreaks_lexicographically() {
    let mut p = pl(&[("z", 1.0), ("a", 1.0), ("m", 1.0)]);
    p.sort_by_value_and_key();
    let keys: Vec<_> = p.0.iter().map(|x| x.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "m", "z"]);
}

#[test]
fn max_value_of_populated_list() {
    common::assert_close(pl(&[("a", -1.0), ("b", 5.0), ("c", 2.0)]).max_value(), 5.0);
}

#[test]
fn max_value_of_empty_list_is_zero() {
    // Documented contract: empty list → 0.0 (not -inf).
    common::assert_close(PairList::default().max_value(), 0.0);
}

#[test]
fn to_unit_scales_by_max() {
    let mut p = pl(&[("a", 1.0), ("b", 2.0), ("c", 4.0)]);
    p.to_unit().unwrap();
    let vals: Vec<f64> = p.0.iter().map(|x| x.value).collect();
    common::assert_close(vals[0], 0.25);
    common::assert_close(vals[1], 0.5);
    common::assert_close(vals[2], 1.0);
}

#[test]
fn to_unit_empty_is_division_by_zero() {
    // With max_value() returning 0.0 for an empty list, to_unit reports the
    // semantically-correct DivisionByZero rather than InvalidArgument.
    let mut p = PairList::default();
    assert!(matches!(p.to_unit(), Err(VectorError::DivisionByZero)));
}

#[test]
fn to_unit_all_zero_is_division_by_zero() {
    let mut p = pl(&[("a", 0.0), ("b", 0.0)]);
    assert!(matches!(p.to_unit(), Err(VectorError::DivisionByZero)));
}

#[test]
fn cut_to_max_len_truncates() {
    let mut p = pl(&[("a", 1.0), ("b", 2.0), ("c", 3.0)]);
    p.cut_to_max_len(2);
    assert_eq!(p.len(), 2);
    // Truncation keeps the leading elements.
    assert_eq!(p.0[0].key, "a");
    assert_eq!(p.0[1].key, "b");
    // Truncating beyond the length is a no-op.
    p.cut_to_max_len(10);
    assert_eq!(p.len(), 2);
}

#[test]
fn pairlist_serde_roundtrip() {
    let p = pl(&[("a", 1.5), ("b", 2.5)]);
    let json = serde_json::to_string(&p).unwrap();
    let back: PairList = serde_json::from_str(&json).unwrap();
    assert_eq!(p, back);
}

// ---------------------------------------------------------------------------
// IntoGenericTensorExt::to_tensor (heavily used by engine executors)
// ---------------------------------------------------------------------------

#[test]
fn float_vector_to_tensor() {
    let t = FloatVector::from(vec![1.0f32, 2.0, 3.0]).to_tensor();
    assert_eq!(t.shape, vec![3]);
    assert_eq!(t.dtype, TensorDataType::Numeric);
    common::assert_slice_close(&t.value.flatten_to_f32_vec(), &[1.0, 2.0, 3.0]);
}

#[test]
fn float_matrix_to_tensor() {
    let m = FloatMatrix::from_shape_vec((2, 2), vec![1.0f32, 2.0, 3.0, 4.0]).unwrap();
    let t = m.to_tensor();
    assert_eq!(t.shape, vec![2, 2]);
    assert_eq!(t.dtype, TensorDataType::Numeric);
}
