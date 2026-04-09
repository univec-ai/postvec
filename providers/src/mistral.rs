//! Mistral embeddings: OpenAI-compatible `/v1/embeddings`. Models emit a
//! fixed-size vector; `dimensions` is rejected (HTTP 422), so this client
//! never sends it.

use crate::{retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub const DEFAULT_MISTRAL_BASE_URL: &str = "https://api.mistral.ai";

// ---- Request and Response Structs ----

/// Represents the JSON request body sent to the Mistral embeddings endpoint.
#[derive(Serialize)]
struct MistralRequest<'a> {
    /// The ID of the model to use (e.g. "mistral-embed", "mistral-embed-2312",
    /// "codestral-embed").
    model: &'a str,
    /// The array of input texts to embed.
    input: &'a [&'a str],
}

/// Represents a single embedding object within the Mistral API response.
#[derive(Deserialize)]
struct MistralEmbeddingData {
    /// The embedding vector.
    embedding: Vec<f32>,
    /// The index of the input text that this embedding corresponds to.
    index: usize,
}

/// Represents the top-level structure of a successful Mistral API response.
#[derive(Deserialize)]
struct MistralResponse {
    /// A list of embedding results, one per input text.
    data: Vec<MistralEmbeddingData>,
}

// ---- Client Implementation ----

/// A client for generating embeddings using Mistral AI's hosted API.
///
/// The API key and an optional `base_url` override come from the resolved
/// provider configuration (see the factory).
pub struct MistralClient {
    /// Shared `reqwest::Client` for HTTP/2 connection pooling.
    client: Client,
    /// The base URL of the Mistral API.
    base_url: String,
    /// The API key for authentication.
    api_key: String,
    /// The name of the model to use (e.g. "mistral-embed-2312").
    model_name: String,
}

impl MistralClient {
    /// Creates a new `MistralClient`.
    ///
    /// # Arguments
    /// * `model_name` - The Mistral model ID (e.g. "mistral-embed-2312").
    /// * `api_key` - The Mistral API key.
    /// * `base_url` - The base URL for the API endpoint.
    /// * `http_client` - An optional shared `reqwest::Client` to reuse connections.
    pub fn new(
        model_name: String,
        api_key: String,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        // If no client was supplied, build one with a generous timeout so a
        // single slow batch doesn't bubble up as a Network error and burn
        // through the retry budget for the rest of the run.
        let client = http_client.unwrap_or_else(|| {
            Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| Client::new())
        });
        Self {
            client,
            base_url,
            api_key,
            model_name,
        }
    }
}

#[async_trait]
impl EmbeddingBackend for MistralClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let request_body = MistralRequest {
            model: &self.model_name,
            input: texts,
        };

        let url = format!("{}/v1/embeddings", self.base_url);

        // What a legitimate response to *this* call can weigh. Computed once,
        // outside the retry so every attempt shares one bound.
        let budget = crate::response_budget(texts.len(), None);
        let api_response: MistralResponse = retry_with_backoff(deadline, || async {
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(EmbeddingError::Network)?;

            if !response.status().is_success() {
                return Err(crate::api_error(response).await);
            }

            // Read the body first, then decode it. A failure to read is
            // transport trouble (retriable `Network`); a body that will not
            // parse is permanent for this response. See `decode_json`.
            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<MistralResponse>(&bytes)
        })
        .await?;

        let mut embeddings: Vec<Embedding> = api_response
            .data
            .into_iter()
            .map(|data| Embedding {
                text_index: data.index,
                vector: data.embedding,
            })
            .collect();

        // The API is not strictly guaranteed to return results in input order;
        // sort by index so the caller's mapping back to original texts is safe.
        embeddings.sort_by_key(|e| e.text_index);

        Ok(embeddings)
    }
}
