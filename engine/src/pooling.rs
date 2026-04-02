//! Token embeddings to a sentence vector: CLS, mean, last-token or none.
use crate::error::EngineError;
use ndarray::{s, Array2, ArrayView, Axis, Dim, Ix2, IxDynImpl};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PoolingStrategy {
    /// First token (`[CLS]`).
    #[default]
    Cls,
    /// Mean of real tokens (attention-mask weighted).
    Mean,
    /// SPLADE sparse expansion (masked LMs).
    Splade,
    /// Last real token (Qwen3-Embedding and similar).
    LastToken,
    /// Raw token embeddings, one matrix per sequence.
    NoPooling,
}

/// First-token pooling. 2D input is already pooled and returned as-is.
pub fn cls_pooling(tensor: &ArrayView<f32, Dim<IxDynImpl>>) -> Result<Array2<f32>, EngineError> {
    match tensor.ndim() {
        // Already pooled.
        2 => Ok(tensor.to_owned().into_dimensionality::<Ix2>()?),

        3 => Ok(tensor.slice(s![.., 0, ..]).to_owned()),
        _ => Err(EngineError::Prediction(format!(
            "Invalid tensor shape for CLS pooling: {:?}. Expected 2D or 3D.",
            tensor.shape()
        ))),
    }
}

/// Last real token (attention mask marks padding). 2D input is returned as-is.
pub fn last_token_pooling(
    tensor: &ArrayView<f32, Dim<IxDynImpl>>,
    attention_mask: &Array2<i64>,
) -> Result<Array2<f32>, EngineError> {
    match tensor.ndim() {
        // Already pooled.
        2 => Ok(tensor.to_owned().into_dimensionality::<Ix2>()?),
        3 => {
            let batch_size = tensor.shape()[0];
            let hidden_size = tensor.shape()[2];
            let mut result = Array2::<f32>::zeros((batch_size, hidden_size));

            for i in 0..batch_size {
                // Last real token: mask sum minus one.
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

/// Mean over real tokens. Padding is zeroed via the attention mask. 2D input
/// is returned as-is.
pub fn mean_pooling(
    token_embeddings: &ArrayView<f32, Dim<IxDynImpl>>,
    attention_mask: &Array2<i64>,
) -> Result<Array2<f32>, EngineError> {
    match token_embeddings.ndim() {
        // Already pooled.
        2 => Ok(token_embeddings.to_owned().into_dimensionality::<Ix2>()?),

        3 => {
            // Broadcast the mask over hidden size.
            let attention_mask_expanded = attention_mask.mapv(|x| x as f32).insert_axis(Axis(2));

            let masked_embeddings = token_embeddings * &attention_mask_expanded;

            let sum_embeddings = masked_embeddings.sum_axis(Axis(1));

            let sum_mask = attention_mask_expanded.sum_axis(Axis(1));

            // Avoid divide-by-zero on empty / fully-masked sequences.
            let clamped_sum_mask = sum_mask.mapv(|x| x.max(1e-9));

            Ok((&sum_embeddings / &clamped_sum_mask).into_dimensionality::<Ix2>()?)
        }
        _ => Err(EngineError::Prediction(format!(
            "Invalid tensor shape for Mean pooling: {:?}. Expected 2D or 3D.",
            token_embeddings.shape()
        ))),
    }
}
