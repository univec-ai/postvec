//!
//! providers/src/openai.rs
//!
//! Implementation of the `EmbeddingBackend` trait for OpenAI's embedding models.
//!
use crate::{retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// The default base URL for the OpenAI API.
// Can be overridden per provider file (`base_url`), handled by the factory.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";

// ---- Request and Response Structs ----
// These structs are used to serialize the request body and deserialize the
// API response. They are derived from the official OpenAI API documentation.

/// Represents the JSON request body sent to the OpenAI embeddings endpoint.
#[derive(Serialize)]
struct OpenAIRequest<'a> {
    /// The ID of the model to use.
    model: &'a str,
    /// The array of input texts to embed.
    input: &'a [&'a str],
    /// Optional: The number of dimensions the resulting output vectors should have.
    /// This is only supported for `text-embedding-3` models.
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

/// Represents a single embedding object within the OpenAI API response.
#[derive(Deserialize)]
struct OpenAIEmbeddingData {
    /// The embedding vector.
    embedding: Vec<f32>,
    /// The index of the input text that this embedding corresponds to.
    index: usize,
}

/// Represents the top-level structure of a successful OpenAI API response.
#[derive(Deserialize)]
struct OpenAIResponse {
    /// A list of embedding results.
    data: Vec<OpenAIEmbeddingData>,
}

// ---- Client Implementation ----

/// A client for generating embeddings using the OpenAI API.
pub struct OpenAIClient {
    /// The shared `reqwest::Client` for making HTTP requests.
    client: Client,
    /// The base URL of the OpenAI API.
    base_url: String,
    /// The API key for authentication.
    api_key: String,
    /// The name of the model to use for embeddings (e.g., "text-embedding-3-small").
    model_name: String,
    /// The optional dimension to request for the embedding vectors.
    dimensions: Option<usize>,
}

impl OpenAIClient {
    /// Creates a new `OpenAIClient`.
    ///
    /// # Arguments
    /// * `model_name` - The specific OpenAI model to use (e.g., "text-embedding-3-large").
    /// * `api_key` - The OpenAI API key.
    /// * `dimensions` - An optional parameter to specify the desired output vector size.
    /// * `base_url` - The base URL for the API endpoint.
    /// * `http_client` - An optional shared `reqwest::Client` to reuse connections.
    pub fn new(
        model_name: String,
        api_key: String,
        dimensions: Option<usize>,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        Self {
            client: http_client.unwrap_or_default(),
            base_url,
            api_key,
            model_name,
            dimensions,
        }
    }
}

#[async_trait]
impl EmbeddingBackend for OpenAIClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        // Only include dimensions for models that support it (text-embedding-3-* models).
        // Older models like text-embedding-ada-002 return an error if dimensions is specified.
        let dimensions_for_request = if self.model_name.starts_with("text-embedding-3") {
            self.dimensions
        } else {
            None
        };

        // Construct the request body from the input texts and client configuration.
        let request_body = OpenAIRequest {
            model: &self.model_name,
            input: texts,
            dimensions: dimensions_for_request,
        };

        // Build the full URL for the API endpoint.
        let url = format!("{}/v1/embeddings", self.base_url);

        // Execute the request using the exponential backoff helper.
        // The closure captures `self`, `url`, and `request_body` by reference.
        // It defines the atomic operation to be retried.
        // What a legitimate response to *this* call can weigh. Computed once,
        // outside the retry so every attempt shares one bound.
        let budget = crate::response_budget(texts.len(), self.dimensions);
        let api_response: OpenAIResponse = retry_with_backoff(deadline, || async {
            // Send the asynchronous POST request.
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key) // Use Bearer token authentication.
                .json(&request_body)
                .send()
                .await
                .map_err(EmbeddingError::Network)?; // Convert reqwest::Error to our retriable Network error

            // Check if the request was successful.
            if !response.status().is_success() {
                return Err(crate::api_error(response).await);
            }

            // Read the body first, then decode it. A failure to read is
            // transport trouble (retriable `Network`); a body that will not
            // parse is permanent for this response. See `decode_json`.
            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<OpenAIResponse>(&bytes)
        })
        .await?;

        // Map the API response data into the crate's public `Embedding` struct.
        let mut embeddings: Vec<Embedding> = api_response
            .data
            .into_iter()
            .map(|data| Embedding {
                text_index: data.index,
                vector: data.embedding,
            })
            .collect();

        // The API is not guaranteed to return embeddings in the same order as the input.
        // We sort them by the index provided in the response to ensure the final output
        // order matches the input text order, which simplifies processing for the caller.
        embeddings.sort_by_key(|e| e.text_index);

        Ok(embeddings)
    }
}
