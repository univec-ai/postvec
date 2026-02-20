//!
//! providers/src/openrouter.rs
//!
//! Implementation of the `EmbeddingBackend` trait for OpenRouter's embedding models.
//!
//! OpenRouter provides an OpenAI-compatible embeddings API that allows transparent
//! access to various embedding models from different providers.
//!
use crate::{body_preview, retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// The default base URL for the OpenRouter API.
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";

// ---- Request and Response Structs ----
// OpenRouter uses an OpenAI-compatible API format.

/// Represents the JSON request body sent to the OpenRouter embeddings endpoint.
#[derive(Serialize)]
struct OpenRouterRequest<'a> {
    /// The ID of the model to use (e.g., "openai/text-embedding-3-small").
    model: &'a str,
    /// The array of input texts to embed.
    input: &'a [&'a str],
    /// Optional: The number of dimensions for the output vectors.
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
}

/// Represents a single embedding object within the OpenRouter API response.
#[derive(Deserialize)]
struct OpenRouterEmbeddingData {
    /// The embedding vector.
    embedding: Vec<f32>,
    /// The index of the input text that this embedding corresponds to.
    index: usize,
}

/// Represents the top-level structure of a successful OpenRouter API response.
#[derive(Deserialize)]
struct OpenRouterResponse {
    /// A list of embedding results.
    data: Vec<OpenRouterEmbeddingData>,
}

// ---- Client Implementation ----

/// A client for generating embeddings using the OpenRouter API.
///
/// OpenRouter provides access to various embedding models through a unified,
/// OpenAI-compatible interface. This allows switching between models from
/// different providers without changing application code.
pub struct OpenRouterClient {
    /// The shared `reqwest::Client` for making HTTP requests.
    client: Client,
    /// The base URL of the OpenRouter API.
    base_url: String,
    /// The API key for authentication.
    api_key: String,
    /// The name of the model to use (e.g., "openai/text-embedding-3-large").
    model_name: String,
    /// The optional dimension to request for the embedding vectors.
    dimensions: Option<usize>,
}

impl OpenRouterClient {
    /// Creates a new `OpenRouterClient`.
    ///
    /// # Arguments
    /// * `model_name` - The OpenRouter model ID (e.g., "openai/text-embedding-3-large").
    /// * `api_key` - The OpenRouter API key.
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
impl EmbeddingBackend for OpenRouterClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        // Construct the request body from the input texts and client configuration.
        let request_body = OpenRouterRequest {
            model: &self.model_name,
            input: texts,
            dimensions: self.dimensions,
        };

        // Build the full URL for the API endpoint.
        let url = format!("{}/v1/embeddings", self.base_url);

        // Execute the request using the exponential backoff helper.
        let api_response: OpenRouterResponse = retry_with_backoff(deadline, || async {
            // Send the asynchronous POST request.
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(EmbeddingError::Network)?;

            // Check if the request was successful.
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let message = response
                    .text()
                    .await
                    .map(|body| body_preview(&body))
                    .unwrap_or_else(|_| "Unknown error".to_string());
                return Err(EmbeddingError::Api { status, message });
            }

            // Read the body first, then decode it. A failure to read is
            // transport trouble (retriable `Network`); a body that will not
            // parse is permanent for this response — OpenRouter occasionally
            // returns 200 with an `{"error": ...}` envelope, or injects
            // SSE-style keepalive whitespace. See `decode_json`.
            let bytes = response.bytes().await.map_err(EmbeddingError::Network)?;
            crate::decode_json::<OpenRouterResponse>(&bytes)
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

        // Sort by index to ensure the output order matches the input text order.
        embeddings.sort_by_key(|e| e.text_index);

        Ok(embeddings)
    }
}
