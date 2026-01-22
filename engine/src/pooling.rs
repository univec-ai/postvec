// File: engine/src/pooling.rs
//! ## Pooling Strategies
//!
//!
//!
//! This module provides functions for pooling the output of transformer models.
//!
//!
//! Pooling is the process of converting a sequence of token embeddings (a matrix)
//! into a single sentence embedding (a vector).
//! It also defines the `PoolingStrategy`
//! enum used for configuration.
use crate::error::EngineError;
use ndarray::{s, Array2, ArrayView, Axis, Dim, Ix2, IxDynImpl};
use serde::{Deserialize, Serialize};

/// Defines the pooling strategy to be used on the model's output.
/// This configuration determines how a sequence of token embeddings is converted
/// into a single fixed-size sentence embedding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PoolingStrategy {
    /// Use the embedding of the `[CLS]` token as the sequence representation.
    /// This is a common strategy for BERT-like models.
    #[default]
    Cls,
    /// Compute the mean of all token embeddings, weighted by the attention mask,
    /// to create a single averaged embedding.
    Mean,
    /// Apply SPLADE (Sparse Lexical and Expansion) to the model embeddings.
    /// This option is typically available for masked language models.
    Splade,
    /// Select the last token in the sequence as the embedding.
    LastToken,
    /// A new variant to explicitly request the unpooled, raw token embeddings.
    /// This will result in a 2D tensor (matrix) per input sequence.
    NoPooling,
}

/// Extracts the CLS token embedding from a model's output tensor.
/// The CLS token is typically the very first token in the sequence.
///
/// # Arguments
/// * `tensor` - A view of the model's output tensor.
///   It can be 2D `[batch_size, hidden_size]`
///   (if pooling is already done) or 3D `[batch_size, sequence_length, hidden_size]`.
///
/// # Returns
/// A `Result` containing a 2D array `[batch_size, hidden_size]` or an `EngineError`.
pub fn cls_pooling(tensor: &ArrayView<f32, Dim<IxDynImpl>>) -> Result<Array2<f32>, EngineError> {
    match tensor.ndim() {
        // If the input is already 2D [batch_size, hidden_size], it's likely already pooled.
        // We can return it directly.
        2 => Ok(tensor.to_owned().into_dimensionality::<Ix2>()?),
        // If the input is 3D [batch_size, sequence_length, hidden_size],
        // we extract the embedding for the first token (the CLS token) for each item in the batch.
        3 => Ok(tensor.slice(s![.., 0, ..]).to_owned()),
        _ => Err(EngineError::Prediction(format!(
            "Invalid tensor shape for CLS pooling: {:?}. Expected 2D or 3D.",
            tensor.shape()
        ))),
    }
}

/// Extracts the embedding of the last non-padding token in each sequence.
///
/// This is used by models like Qwen3-Embedding that use the last token
/// as the sequence representation (similar to how GPT-style models use the
/// last token).
///
/// # Arguments
/// * `tensor` - A view of the model's output tensor.
///   Expected to be 3D `[batch_size, sequence_length, hidden_size]`.
/// * `attention_mask` - A 2D array `[batch_size, sequence_length]` where `1` indicates a
///   real token and `0` indicates padding. Used to find the position of the last real token.
///
/// # Returns
/// A `Result` containing a 2D array `[batch_size, hidden_size]` or an `EngineError`.
pub fn last_token_pooling(
    tensor: &ArrayView<f32, Dim<IxDynImpl>>,
    attention_mask: &Array2<i64>,
) -> Result<Array2<f32>, EngineError> {
    match tensor.ndim() {
        // If the input is already 2D [batch_size, hidden_size], return it directly.
        2 => Ok(tensor.to_owned().into_dimensionality::<Ix2>()?),
        3 => {
            let batch_size = tensor.shape()[0];
            let hidden_size = tensor.shape()[2];
            let mut result = Array2::<f32>::zeros((batch_size, hidden_size));

            for i in 0..batch_size {
                // Find the index of the last non-padding token by looking at the attention mask.
                // Sum of attention mask gives the number of real tokens; subtract 1 for 0-based index.
                let seq_len: i64 = attention_mask.row(i).iter().sum();
                let last_idx = (seq_len - 1).max(0) as usize;
                result.row_mut(i).assign(&tensor.slice(s![i, last_idx, ..]));
            }
            Ok(result)
        }
        _ => Err(EngineError::Prediction(format!(
            "Invalid tensor shape for LastToken pooling: {:?}. Expected 2D or 3D.",
            tensor.shape()
        ))),
    }
}

/// Performs mean pooling over the sequence dimension of a model's output tensor,
/// correctly handling padding by using an attention mask.
///
/// # Arguments
/// * `token_embeddings` - A view of the model's output tensor, expected to be 3D
///   `[batch_size, sequence_length, hidden_size]`.
/// * `attention_mask` - A 2D array `[batch_size, sequence_length]` where `1` indicates a
///   real token and `0` indicates padding.
///
/// # Returns
/// A `Result` containing a 2D array `[batch_size, hidden_size]` or an `EngineError`.
pub fn mean_pooling(
    token_embeddings: &ArrayView<f32, Dim<IxDynImpl>>,
    attention_mask: &Array2<i64>,
) -> Result<Array2<f32>, EngineError> {
    match token_embeddings.ndim() {
        // If the input is already 2D, we assume it has been pooled and return it directly.
        2 => Ok(token_embeddings.to_owned().into_dimensionality::<Ix2>()?),
        // If the input is 3D, we proceed with the mean pooling logic.
        3 => {
            // Expand attention mask from [batch_size, seq_len] to [batch_size, seq_len, 1]
            // so it can be broadcasted for element-wise multiplication with the embeddings.
            let attention_mask_expanded = attention_mask.mapv(|x| x as f32).insert_axis(Axis(2));

            // Multiply token embeddings by the expanded attention mask.
            // This effectively zeros out the embeddings for any padding tokens.
            let masked_embeddings = token_embeddings * &attention_mask_expanded;

            // Sum the embeddings along the sequence length dimension (Axis(1)).
            // The result has shape [batch_size, hidden_size].
            let sum_embeddings = masked_embeddings.sum_axis(Axis(1));

            // Sum the attention mask along the sequence length dimension to get the
            // count of non-padding tokens for each sentence in the batch.
            let sum_mask = attention_mask_expanded.sum_axis(Axis(1));

            // Clamp the mask sum to a minimum of 1e-9 to avoid division by zero
            // for sequences that might be empty or fully masked.
            let clamped_sum_mask = sum_mask.mapv(|x| x.max(1e-9));

            // Divide the sum of embeddings by the count of non-padding tokens to get the mean.
            Ok((&sum_embeddings / &clamped_sum_mask).into_dimensionality::<Ix2>()?)
        }
        _ => Err(EngineError::Prediction(format!(
            "Invalid tensor shape for Mean pooling: {:?}. Expected 2D or 3D.",
            token_embeddings.shape()
        ))),
    }
}
