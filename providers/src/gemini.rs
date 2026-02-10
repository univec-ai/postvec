//!
//! providers/src/gemini.rs
//!
//! Implementation of the `EmbeddingBackend` trait for Google's Gemini models.
//!
use crate::{body_preview, retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// The default base URL for the Gemini API.
// Can be overridden per provider file (`base_url`), handled by the factory.
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";

// ---- Request and Response Structs ----
// These structs model the nested JSON structure required by the Gemini API.

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    parts: Vec<GeminiPart<'a>>,
}

/// Represents a single embedding request within a batch.
#[derive(Serialize)]
struct GeminiRequest<'a> {
    model: String,
    content: GeminiContent<'a>,
}

/// The top-level request body for a batch embedding operation.
#[derive(Serialize)]
struct GeminiBatchRequest<'a> {
    requests: Vec<GeminiRequest<'a>>,
}

/// Represents the embedding values in the response.
#[derive(Deserialize)]
struct GeminiEmbeddingValue {
    values: Vec<f32>,
}

/// The top-level response for a successful batch embedding operation.
#[derive(Deserialize)]
struct GeminiBatchResponse {
    embeddings: Vec<GeminiEmbeddingValue>,
}

// ---- Client Implementation ----

/// A client for generating embeddings using the Google Gemini API.
pub struct GeminiClient {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
}

impl GeminiClient {
    /// Creates a new `GeminiClient`.
    ///
    /// # Arguments
    /// * `model_name` - The model identifier (e.g., "gemini-embedding-001").
    /// * `api_key` - The Google AI API key.
    /// * `base_url` - The base URL for the API endpoint.
    /// * `http_client` - An optional shared `reqwest::Client`.
    pub fn new(
        model_name: String,
        api_key: String,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        Self {
            client: http_client.unwrap_or_default(),
            base_url,
            api_key,
            model_name,
        }
    }
}

/// Gemini authenticates via a `?key=…` URL query parameter, and
/// `reqwest::Error`'s Display includes the request URL — so every network
/// error from this client strips the URL before it can reach a log line.
fn redacted_network(e: reqwest::Error) -> EmbeddingError {
    EmbeddingError::Network(e.without_url())
}

#[async_trait]
impl EmbeddingBackend for GeminiClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        // The API requires the model name in a specific path format for each request.
        let model_path = format!("models/{}", self.model_name);

        // Create a vector of individual embedding requests, one for each input text.
        let requests: Vec<GeminiRequest> = texts
            .iter()
            .map(|text| GeminiRequest {
                model: model_path.clone(),
                content: GeminiContent {
                    parts: vec![GeminiPart { text }],
                },
            })
            .collect();

        let request_body = GeminiBatchRequest { requests };

        // Construct the full URL. Authentication is done via a URL query parameter.
        let url = format!(
            "{}/v1beta/{}:batchEmbedContents?key={}",
            self.base_url, model_path, self.api_key
        );

        // Execute the request using the exponential backoff helper.
        let api_response: GeminiBatchResponse = retry_with_backoff(deadline, || async {
            // Send the POST request.
            let response = self
                .client
                .post(&url)
                .json(&request_body)
                .send()
                .await
                .map_err(redacted_network)?;

            // Handle non-successful responses.
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let message = response
                    .text()
                    .await
                    .map(|body| body_preview(&body))
                    .unwrap_or_else(|_| "Unknown error".to_string());
                // Return an Api error for the retry logic to inspect.
                return Err(EmbeddingError::Api { status, message });
            }

            // Deserialize the successful JSON response.
            // `response.json()` returns a `Result<T, reqwest::Error>`.
            // We map this error to `EmbeddingError::Network`, which is retriable.
            response.json().await.map_err(redacted_network)
        })
        .await?;

        // The Gemini API returns embeddings in the same order as the requests.
        // We use `enumerate` to get the original index.
        let embeddings = api_response
            .embeddings
            .into_iter()
            .enumerate()
            .map(|(index, data)| Embedding {
                text_index: index,
                vector: data.values,
            })
            .collect();

        Ok(embeddings)
    }
}
