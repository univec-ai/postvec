//! Tests for `engine::pooling` — CLS / mean / last-token pooling and the
//! `PoolingStrategy` enum (serde + default). Pure tensor math, no model needed.

mod common;

use engine::pooling::{cls_pooling, last_token_pooling, mean_pooling, PoolingStrategy};
use ndarray::{Array, Array2, IxDyn};

/// 3D token-embedding tensor `[batch, seq, hidden]` as a dynamic-dim view source.
fn token_embeddings(shape: [usize; 3], data: Vec<f32>) -> Array<f32, IxDyn> {
    Array::from_shape_vec(IxDyn(&shape), data).unwrap()
}

// ---------------------------------------------------------------------------
// CLS pooling
// ---------------------------------------------------------------------------

#[test]
fn cls_pooling_takes_first_token() {
    // batch=1, seq=3, hidden=2. CLS = first token => [10, 11].
    let t = token_embeddings([1, 3, 2], vec![10.0, 11.0, 20.0, 21.0, 30.0, 31.0]);
    let pooled = cls_pooling(&t.view()).unwrap();
    assert_eq!(pooled.shape(), &[1, 2]);
    common::assert_slice_close(pooled.as_slice().unwrap(), &[10.0, 11.0]);
}

#[test]
fn cls_pooling_2d_passthrough() {
    // Already-pooled 2D input is returned as-is.
    let t = token_embeddings_2d(&[2, 2], vec![1.0, 2.0, 3.0, 4.0]);
    let pooled = cls_pooling(&t.view()).unwrap();
    assert_eq!(pooled.shape(), &[2, 2]);
    common::assert_slice_close(pooled.as_slice().unwrap(), &[1.0, 2.0, 3.0, 4.0]);
}

#[test]
fn cls_pooling_rejects_1d() {
    let t = Array::from_shape_vec(IxDyn(&[3]), vec![1.0, 2.0, 3.0]).unwrap();
    assert!(cls_pooling(&t.view()).is_err());
}

// ---------------------------------------------------------------------------
// Mean pooling
// ---------------------------------------------------------------------------

#[test]
fn mean_pooling_averages_unmasked_tokens() {
    // batch=1, seq=3, hidden=2; all tokens real → plain average.
    let t = token_embeddings([1, 3, 2], vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 3), vec![1, 1, 1]).unwrap();
    let pooled = mean_pooling(&t.view(), &mask).unwrap();
    assert_eq!(pooled.shape(), &[1, 2]);
    common::assert_slice_close(pooled.as_slice().unwrap(), &[2.0, 2.0]); // mean of 1,2,3
}

#[test]
fn mean_pooling_ignores_padding() {
    // Third token is padding (mask 0); mean should be over first two only → 1.5.
    let t = token_embeddings([1, 3, 2], vec![1.0, 1.0, 2.0, 2.0, 99.0, 99.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 3), vec![1, 1, 0]).unwrap();
    let pooled = mean_pooling(&t.view(), &mask).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[1.5, 1.5]);
}

#[test]
fn mean_pooling_fully_masked_is_zero_not_nan() {
    // All padding → clamped denominator avoids division by zero (result ~0).
    let t = token_embeddings([1, 2, 2], vec![5.0, 5.0, 6.0, 6.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 2), vec![0, 0]).unwrap();
    let pooled = mean_pooling(&t.view(), &mask).unwrap();
    for &v in pooled.as_slice().unwrap() {
        assert!(v.is_finite(), "expected finite, got {v}");
        common::assert_close(v as f64, 0.0);
    }
}

#[test]
fn mean_pooling_2d_passthrough() {
    let t = token_embeddings_2d(&[1, 3], vec![7.0, 8.0, 9.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 1), vec![1]).unwrap();
    let pooled = mean_pooling(&t.view(), &mask).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[7.0, 8.0, 9.0]);
}

// ---------------------------------------------------------------------------
// Last-token pooling
// ---------------------------------------------------------------------------

#[test]
fn last_token_pooling_picks_last_real_token() {
    // seq=3, two real tokens then padding → last real token is index 1 = [20,21].
    let t = token_embeddings([1, 3, 2], vec![10.0, 11.0, 20.0, 21.0, 0.0, 0.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 3), vec![1, 1, 0]).unwrap();
    let pooled = last_token_pooling(&t.view(), &mask).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[20.0, 21.0]);
}

#[test]
fn last_token_pooling_all_real() {
    let t = token_embeddings([1, 3, 2], vec![10.0, 11.0, 20.0, 21.0, 30.0, 31.0]);
    let mask = Array2::<i64>::from_shape_vec((1, 3), vec![1, 1, 1]).unwrap();
    let pooled = last_token_pooling(&t.view(), &mask).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[30.0, 31.0]);
}

#[test]
fn last_token_pooling_handles_batch() {
    // Two sequences with different real lengths.
    let t = token_embeddings(
        [2, 2, 2],
        vec![1.0, 1.0, 2.0, 2.0, /*seq2*/ 3.0, 3.0, 4.0, 4.0],
    );
    let mask = Array2::<i64>::from_shape_vec((2, 2), vec![1, 0, 1, 1]).unwrap();
    let pooled = last_token_pooling(&t.view(), &mask).unwrap();
    assert_eq!(pooled.shape(), &[2, 2]);
    // seq1 last real = index 0 = [1,1]; seq2 last real = index 1 = [4,4].
    common::assert_slice_close(pooled.as_slice().unwrap(), &[1.0, 1.0, 4.0, 4.0]);
}

// ---------------------------------------------------------------------------
// PoolingStrategy enum
// ---------------------------------------------------------------------------

#[test]
fn pooling_strategy_default_is_cls() {
    assert_eq!(PoolingStrategy::default(), PoolingStrategy::Cls);
}

#[test]
fn pooling_strategy_serde_kebab_case() {
    assert_eq!(
        serde_json::to_string(&PoolingStrategy::LastToken).unwrap(),
        "\"last-token\""
    );
    assert_eq!(
        serde_json::to_string(&PoolingStrategy::NoPooling).unwrap(),
        "\"no-pooling\""
    );
    let parsed: PoolingStrategy = serde_json::from_str("\"mean\"").unwrap();
    assert_eq!(parsed, PoolingStrategy::Mean);
}

// Helper: build a 2D dynamic-dim array for the "already pooled" passthrough cases.
fn token_embeddings_2d(shape: &[usize], data: Vec<f32>) -> Array<f32, IxDyn> {
    Array::from_shape_vec(IxDyn(shape), data).unwrap()
}
