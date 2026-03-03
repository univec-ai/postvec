//!
//! providers/src/cohere.rs
//!
//! Implementation of the `EmbeddingBackend` trait for Cohere's embedding models.
//!
//! This client requires an `input_type` to be specified, which is critical for
//! achieving optimal performance with Cohere's models.
//!
use crate::{retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// Default base URL for the Cohere API.
// Can be overridden per provider file (`base_url`), handled by the factory.
pub const DEFAULT_COHERE_BASE_URL: &str = "https://api.cohere.com";

// ---- Request and Response Structs ----

/// Represents the JSON request body for the Cohere v2 embed endpoint.
#[derive(Serialize)]
struct CohereRequest<'a> {
    /// The ID of the model to use.
    model: &'a str,
    /// An array of strings to embed.
    texts: &'a [&'a str],
    /// The intended use case for the embeddings, e.g., "search_document" or "search_query".
    input_type: &'a str,
    /// The types of embeddings to return. Using ["float"] for standard float vectors.
    embedding_types: Vec<&'a str>,
}

/// Represents the nested embeddings structure in the Cohere v2 API response (v4+ models).
/// V4 models like embed-v4.0 return embeddings under `embeddings.float`.
#[derive(Deserialize)]
struct CohereEmbeddingsNested {
    /// Float embeddings - array of embedding vectors.
    float: Vec<Vec<f32>>,
}

/// Enum to handle both v3 and v4 response formats.
/// V3 models return embeddings directly as an array.
/// V4 models return embeddings nested under a `float` field.
#[derive(Deserialize)]
#[serde(untagged)]
enum CohereEmbeddingsFormat {
    /// V4+ format: nested under `float` field
    Nested(CohereEmbeddingsNested),
    /// V3 format: direct array of vectors
    Direct(Vec<Vec<f32>>),
}

impl CohereEmbeddingsFormat {
    /// Extract the embeddings as a Vec<Vec<f32>> regardless of format.
    fn into_vectors(self) -> Vec<Vec<f32>> {
        match self {
            CohereEmbeddingsFormat::Nested(nested) => nested.float,
            CohereEmbeddingsFormat::Direct(vectors) => vectors,
        }
    }
}

/// Represents the successful response from the Cohere v2 embed endpoint.
/// Handles both v3 (direct array) and v4 (nested float) response formats.
#[derive(Deserialize)]
struct CohereResponse {
    /// The embeddings, either as a direct array (v3) or nested under float (v4).
    embeddings: CohereEmbeddingsFormat,
}

// ---- Client Implementation ----

/// A client for generating embeddings using the Cohere API.
pub struct CohereClient {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
    /// The input type for the embeddings (e.g., "search_document").
    input_type: String,
}

impl CohereClient {
    /// Creates a new `CohereClient`.
    ///
    /// The `input_type` is a mandatory parameter to ensure correct usage of the API,
    /// as Cohere models are optimized for specific use cases.
    ///
    /// # Arguments
    /// * `model_name` - The Cohere model ID (e.g., "embed-english-v3.0").
    /// * `api_key` - The Cohere API key.
    /// * `input_type` - The use case, e.g., "search_document", "search_query".
    /// * `base_url` - The base URL for the API endpoint.
    /// * `http_client` - An optional shared `reqwest::Client`.
    pub fn new(
        model_name: String,
        api_key: String,
        input_type: String,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        // Use provided client or create one with a sensible timeout to prevent hangs.
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
            input_type,
        }
    }
}

#[async_trait]
impl EmbeddingBackend for CohereClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        // Construct the request body for Cohere v2 API.
        let request_body = CohereRequest {
            model: &self.model_name,
            texts,
            input_type: &self.input_type,
            embedding_types: vec!["float"],
        };

        let url = format!("{}/v2/embed", self.base_url);

        // Execute the request using the exponential backoff helper.
        // What a legitimate response to *this* call can weigh. Computed once,
        // outside the retry so every attempt shares one bound.
        let budget = crate::response_budget(texts.len(), None);
        let api_response: CohereResponse = retry_with_backoff(deadline, || async {
            // Send the POST request with Bearer token authentication.
            let response = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(EmbeddingError::Network)?;

            // Handle non-successful responses.
            if !response.status().is_success() {
                return Err(crate::api_error(response, Some(&self.api_key)).await);
            }

            // Read the body first, then decode it. A failure to read is
            // transport trouble (retriable `Network`); a body that will not
            // parse is permanent for this response. See `decode_json`.
            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<CohereResponse>(&bytes)
        })
        .await?;

        // Cohere returns embeddings in the same order as the input texts.
        // We use `enumerate` to get the original index.
        // The `into_vectors()` method handles both v3 (direct) and v4 (nested) formats.
        let embeddings = api_response
            .embeddings
            .into_vectors()
            .into_iter()
            .enumerate()
            .map(|(index, vector)| Embedding {
                text_index: index,
                vector,
            })
            .collect();

        Ok(embeddings)
    }
}
