//! Tests for `engine::executors::{EmbeddingOutput, SingleBatchOutput}` — the
//! post-inference pooling + normalization + token-accounting staging layer.

mod common;

use engine::executors::{EmbeddingOutput, SingleBatchOutput};
use engine::pooling::PoolingStrategy;
use ndarray::Array2;
use shared::vectors::{GenericTensor, TensorDataType, TensorValue, VectorMathExt};

fn batch(shape: [usize; 3], data: Vec<f32>, mask: Vec<i64>, tokens: u32) -> SingleBatchOutput {
    let seq = shape[1];
    let bsz = shape[0];
    SingleBatchOutput {
        output_tensors: vec![common::f32_tensor(&shape, data)],
        attention_mask_array: Array2::from_shape_vec((bsz, seq), mask).unwrap(),
        token_count: tokens,
    }
}

#[test]
fn pool_mean_strategy() {
    let b = batch(
        [1, 3, 2],
        vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0],
        vec![1, 1, 1],
        3,
    );
    let pooled = b.pool(PoolingStrategy::Mean, 0).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[2.0, 2.0]);
}

#[test]
fn pool_cls_strategy() {
    let b = batch(
        [1, 3, 2],
        vec![1.0, 9.0, 2.0, 2.0, 3.0, 3.0],
        vec![1, 1, 1],
        3,
    );
    let pooled = b.pool(PoolingStrategy::Cls, 0).unwrap();
    common::assert_slice_close(pooled.as_slice().unwrap(), &[1.0, 9.0]);
}

#[test]
fn pool_missing_tensor_index_errors() {
    let b = batch([1, 2, 2], vec![1.0, 1.0, 2.0, 2.0], vec![1, 1], 2);
    assert!(b.pool(PoolingStrategy::Mean, 5).is_err());
}

#[test]
fn pool_non_float32_tensor_errors() {
    // Int64 output tensor is invalid for embedding pooling.
    let int_tensor = GenericTensor {
        value: TensorValue::Int64(
            ndarray::Array::from_shape_vec(ndarray::IxDyn(&[1, 2, 2]), vec![1i64, 2, 3, 4])
                .unwrap(),
        ),
        shape: vec![1, 2, 2],
        dtype: TensorDataType::Numeric,
    };
    let b = SingleBatchOutput {
        output_tensors: vec![int_tensor],
        attention_mask_array: Array2::from_shape_vec((1, 2), vec![1, 1]).unwrap(),
        token_count: 2,
    };
    assert!(b.pool(PoolingStrategy::Mean, 0).is_err());
}

#[test]
fn pool_splade_and_no_pooling_error() {
    let b = batch([1, 2, 2], vec![1.0, 1.0, 2.0, 2.0], vec![1, 1], 2);
    assert!(b.pool(PoolingStrategy::Splade, 0).is_err());
    assert!(b.pool(PoolingStrategy::NoPooling, 0).is_err());
}

#[test]
fn total_tokens_sums_across_batches() {
    let out = EmbeddingOutput::new(vec![
        batch([1, 2, 2], vec![1.0, 1.0, 2.0, 2.0], vec![1, 1], 2),
        batch(
            [1, 3, 2],
            vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0],
            vec![1, 1, 1],
            3,
        ),
    ]);
    assert_eq!(out.total_tokens(), 5);
}

/// Mirror of the production pooling+(optional)normalize pipeline, exercised
/// through the public `export_with_transformer` plumbing. (The crate's own
/// `default_embedding_transformer` helper is not exported, so we drive the same
/// `pool` + `normalized` path via a closure here.)
fn pool_transformer(
    strategy: PoolingStrategy,
    normalize: bool,
) -> impl Fn(&[SingleBatchOutput]) -> Result<Vec<shared::vectors::FloatVector>, engine::error::EngineError>
{
    move |batches| {
        let mut all = Vec::new();
        for b in batches {
            let pooled = b.pool(strategy.clone(), 0)?;
            for row in pooled.rows() {
                let v = row.to_owned();
                all.push(if normalize { v.normalized() } else { v });
            }
        }
        Ok(all)
    }
}

#[test]
fn transformer_without_normalize_returns_pooled() {
    let out = EmbeddingOutput::new(vec![batch(
        [1, 3, 2],
        vec![3.0, 4.0, 3.0, 4.0, 3.0, 4.0],
        vec![1, 1, 1],
        3,
    )]);
    let embeddings = out
        .export_with_transformer(pool_transformer(PoolingStrategy::Mean, false))
        .unwrap();
    assert_eq!(embeddings.len(), 1);
    // Mean over identical rows = [3,4]; not normalized.
    common::assert_slice_close(embeddings[0].as_slice().unwrap(), &[3.0, 4.0]);
}

#[test]
fn transformer_with_normalize_unit_norm() {
    let out = EmbeddingOutput::new(vec![batch(
        [1, 3, 2],
        vec![3.0, 4.0, 3.0, 4.0, 3.0, 4.0],
        vec![1, 1, 1],
        3,
    )]);
    let embeddings = out
        .export_with_transformer(pool_transformer(PoolingStrategy::Mean, true))
        .unwrap();
    // [3,4] normalized = [0.6, 0.8], unit norm.
    common::assert_slice_close(embeddings[0].as_slice().unwrap(), &[0.6, 0.8]);
    common::assert_close(embeddings[0].norm_l2() as f64, 1.0);
}

#[test]
fn transformer_flattens_multiple_batches_in_order() {
    let out = EmbeddingOutput::new(vec![
        batch([1, 2, 2], vec![1.0, 1.0, 1.0, 1.0], vec![1, 1], 2),
        batch([1, 2, 2], vec![2.0, 2.0, 2.0, 2.0], vec![1, 1], 2),
    ]);
    let embeddings = out
        .export_with_transformer(pool_transformer(PoolingStrategy::Mean, false))
        .unwrap();
    assert_eq!(embeddings.len(), 2);
    common::assert_slice_close(embeddings[0].as_slice().unwrap(), &[1.0, 1.0]);
    common::assert_slice_close(embeddings[1].as_slice().unwrap(), &[2.0, 2.0]);
}
