//! Staging layer between raw model tensors and pooled/normalized embeddings.
use crate::{
    error::EngineError,
    pooling::{cls_pooling, last_token_pooling, mean_pooling, PoolingStrategy},
};
use ndarray::Array2;
use shared::vectors::{FloatVector, GenericTensor, VectorMathExt};
/// One inference batch: tensors, attention mask (needed for mean pooling)
/// and token count.
pub struct SingleBatchOutput {
    pub output_tensors: Vec<GenericTensor>,
    pub attention_mask_array: Array2<i64>,
    pub token_count: u32,
}
impl SingleBatchOutput {
    /// Pool one output tensor into a `[batch, hidden]` matrix.
    pub fn pool(
        &self,
        pooling_strategy: PoolingStrategy,
        tensor_index: usize,
    ) -> Result<Array2<f32>, EngineError> {
        let tensor_to_pool = self.output_tensors.get(tensor_index).ok_or_else(|| {
            EngineError::Prediction(format!(
                "Model output tensor at index {} not found for pooling.",
                tensor_index
            ))
        })?;

        // Embeddings are Float32.
        let tensor_view =
            match &tensor_to_pool.value {
                shared::vectors::TensorValue::Float32(arr) => arr.view(),
                _ => return Err(EngineError::Prediction(
                    "Model output tensor is not of type Float32, which is required for embeddings."
                        .to_string(),
                )),
            };

        match pooling_strategy {
            PoolingStrategy::Cls => cls_pooling(&tensor_view),
            PoolingStrategy::Mean => mean_pooling(&tensor_view, &self.attention_mask_array),
            PoolingStrategy::LastToken => {
                last_token_pooling(&tensor_view, &self.attention_mask_array)
            }
            // Not implemented in the ndarray pooling path.
            PoolingStrategy::Splade => Err(EngineError::Prediction(format!(
                "Pooling strategy '{:?}' is not yet implemented for this executor.",
                pooling_strategy
            ))),
            // Callers that want unpooled tensors skip this method.
            PoolingStrategy::NoPooling => Err(EngineError::Prediction(
                "Logical error: `SingleBatchOutput::pool` was called with `NoPooling` strategy."
                    .to_string(),
            )),
        }
    }
}
/// All batches from one `execute` call, before pooling.
pub struct EmbeddingOutput {
    pub batches: Vec<SingleBatchOutput>,
}
impl EmbeddingOutput {
    pub fn new(batches: impl IntoIterator<Item = SingleBatchOutput>) -> Self {
        Self {
            batches: batches.into_iter().collect(),
        }
    }

    pub fn total_tokens(&self) -> u32 {
        self.batches.iter().map(|b| b.token_count).sum()
    }
    /// Run `transformer` over the raw batches (pool, normalize, aggregate).
    pub fn export_with_transformer<R>(
        &self,
        transformer: impl Fn(&[SingleBatchOutput]) -> Result<R, EngineError>,
    ) -> Result<R, EngineError> {
        transformer(&self.batches)
    }
}
/// Closure that pools tensor 0 (optional L2-norm) and concatenates batches.
/// A `Result` containing a `Vec` of `FloatVector`s (embeddings).
#[allow(dead_code)]
pub fn default_embedding_transformer(
    pooling_strategy: PoolingStrategy,
    normalize: bool,
) -> impl Fn(&[SingleBatchOutput]) -> Result<Vec<FloatVector>, EngineError> {
    move |batches| {
        let mut all_embeddings = Vec::new();
        for batch in batches {
            let pooled = batch.pool(pooling_strategy.clone(), 0)?;
            let embeddings: Vec<FloatVector> = if normalize {
                pooled
                    .rows()
                    .into_iter()
                    .map(|row| row.to_owned().normalized())
                    .collect()
            } else {
                pooled
                    .rows()
                    .into_iter()
                    .map(|row| row.to_owned())
                    .collect()
            };
            all_embeddings.extend(embeddings);
        }
        Ok(all_embeddings)
    }
}
