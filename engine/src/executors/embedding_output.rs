// File: engine/src/executors/embedding_output.rs
//! ## Embedding Output Handling
//!
//!
//! Inspired by the `fastembed` library, this module provides a more structured
//! and flexible way to handle the outputs of embedding models. It decouples the
//! raw model inference from the post-processing steps like pooling and normalization.
//!
//! ### Core Components
//!
//!
//! - `SingleBatchOutput`: Contains all necessary data from a single inference batch,
//!   including all of the model's output tensors and the corresponding attention mask.
//!   This is crucial for correct mean pooling.
//!
//! - `EmbeddingOutput`: A container for the outputs of all batches from a single
//!   `execute` call.
//!   It acts as a staging area for the raw model outputs.
//! - `default_embedding_transformer`: A function that creates a post-processing
//!
//! pipeline (a "transformer") that can be applied to an `EmbeddingOutput` to
//!
//! extract, pool, and normalize embeddings into the final desired format.
use crate::{
    error::EngineError,
    pooling::{cls_pooling, last_token_pooling, mean_pooling, PoolingStrategy},
};
use ndarray::Array2;
use shared::vectors::{FloatVector, GenericTensor, VectorMathExt};
/// Contains the output of a single batch of inference.
///
/// This struct holds all the necessary information for post-processing to be
/// performed on a per-batch basis, such as pooling.
pub struct SingleBatchOutput {
    /// The raw output tensors from the model for this batch.
    /// This will contain all output layers from the model's inference run.
    pub output_tensors: Vec<GenericTensor>,
    /// The attention mask tensor for this batch, which is required for correct
    /// masked mean pooling.
    pub attention_mask_array: Array2<i64>,
    /// The number of tokens processed in this batch (e.g. sum of sequence lengths).
    pub token_count: u32,
}
impl SingleBatchOutput {
    /// Selects a specific output tensor and applies a pooling strategy.
    ///
    /// This function handles the logic of converting a sequence of token embeddings
    /// from a specific output layer into a set of sentence embeddings for the batch.
    ///
    /// # Arguments
    /// * `pooling_strategy` - The pooling method to apply (e.g., CLS, Mean).
    /// * `tensor_index` - The index of the tensor in `output_tensors` to pool.
    pub fn pool(
        &self,
        pooling_strategy: PoolingStrategy,
        tensor_index: usize,
    ) -> Result<Array2<f32>, EngineError> {
        // Select the specific tensor to pool based on the provided index.
        let tensor_to_pool = self.output_tensors.get(tensor_index).ok_or_else(|| {
            EngineError::Prediction(format!(
                "Model output tensor at index {} not found for pooling.",
                tensor_index
            ))
        })?;

        // Get a view of the tensor's underlying data.
        // It must be Float32 for embeddings.
        let tensor_view =
            match &tensor_to_pool.value {
                shared::vectors::TensorValue::Float32(arr) => arr.view(),
                _ => return Err(EngineError::Prediction(
                    "Model output tensor is not of type Float32, which is required for embeddings."
                        .to_string(),
                )),
            };

        // Apply the chosen pooling strategy.
        match pooling_strategy {
            PoolingStrategy::Cls => cls_pooling(&tensor_view),
            PoolingStrategy::Mean => mean_pooling(&tensor_view, &self.attention_mask_array),
            PoolingStrategy::LastToken => {
                last_token_pooling(&tensor_view, &self.attention_mask_array)
            }
            // Splade is not yet implemented in the ndarray-based pooling logic.
            PoolingStrategy::Splade => Err(EngineError::Prediction(format!(
                "Pooling strategy '{:?}' is not yet implemented for this executor.",
                pooling_strategy
            ))),
            // Handle the new `NoPooling` variant.
            // This function should only be called by logic that *wants* pooling.
            // Receiving `NoPooling` here indicates a logical error in the calling executor.
            PoolingStrategy::NoPooling => Err(EngineError::Prediction(
                "Logical error: `SingleBatchOutput::pool` was called with `NoPooling` strategy."
                    .to_string(),
            )),
        }
    }
}
/// A container for all batch outputs from an embedding generation task.
///
/// This struct acts as a staging area, holding the raw results from inference
/// before they are transformed into the final list of embeddings.
pub struct EmbeddingOutput {
    pub batches: Vec<SingleBatchOutput>,
}
impl EmbeddingOutput {
    /// Creates a new `EmbeddingOutput` from an iterator of batch results.
    pub fn new(batches: impl IntoIterator<Item = SingleBatchOutput>) -> Self {
        Self {
            batches: batches.into_iter().collect(),
        }
    }

    /// Returns the total number of tokens processed across all batches.
    pub fn total_tokens(&self) -> u32 {
        self.batches.iter().map(|b| b.token_count).sum()
    }
    /// Exports the final embeddings using a provided transformer function.
    ///
    /// The transformer is responsible for processing the raw batch outputs
    /// (e.g., by pooling and normalizing) and aggregating them into the final
    /// desired output format.
    ///
    /// # Arguments
    /// * `transformer` - A closure that takes a slice of `SingleBatchOutput`s and
    ///   returns the final result, or an error.
    pub fn export_with_transformer<R>(
        &self,
        transformer: impl Fn(&[SingleBatchOutput]) -> Result<R, EngineError>,
    ) -> Result<R, EngineError> {
        transformer(&self.batches)
    }
}
/// Creates a default "transformer" function for post-processing embedding outputs.
///
/// This function generates and returns a closure that implements a standard
/// embedding pipeline: pooling, optional normalization, and aggregation.
/// This closure can
/// then be passed to `EmbeddingOutput::export_with_transformer`.
///
/// This default implementation assumes pooling should happen on the **first**
/// output tensor (index 0).
///
/// # Arguments
/// * `pooling_strategy` - The pooling strategy to be used.
/// * `normalize` - Whether to L2-normalize the output embeddings after pooling.
///
/// # Returns
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
