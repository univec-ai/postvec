//! Gemini embeddings (`POST .../models/{id}:batchEmbedContents`).

use crate::{retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Overridable per provider file (`base_url`).
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";

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
#[serde(rename_all = "camelCase")]
struct GeminiRequest<'a> {
    model: String,
    content: GeminiContent<'a>,
    /// `RETRIEVAL_QUERY` / `RETRIEVAL_DOCUMENT`. Gemini documents these as
    /// quality-relevant for retrieval, and the purpose already travels this
    /// far — the gateway builds one client per purpose exactly as it does for
    /// Cohere.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_type: Option<&'a str>,
    /// Truncates the native embedding to the requested size. Without it a
    /// descriptor's `dim` was only honoured at the model's native width,
    /// while the file documents `dim` as authoritative and the gateway checks
    /// every response against it — so any other value failed every call.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_dimensionality: Option<usize>,
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
    /// The descriptor's declared dimension, requested explicitly.
    dimensions: Option<usize>,
    /// The Gemini task type for this client's purpose, or `None` to let the
    /// API apply its own default.
    task_type: Option<&'static str>,
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
        dimensions: Option<usize>,
        input_type: &str,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        let model_name_for_options = model_name.clone();
        Self {
            client: http_client.unwrap_or_default(),
            base_url,
            api_key,
            model_name,
            // Only what this model's contract says it accepts. An id the
            // table does not know cannot be configured at all (the loader
            // refuses it), so `None` here is belt-and-braces.
            dimensions: dimensions.filter(|_| {
                gemini_contract(&model_name_for_options).is_some_and(|c| c.dims.is_some())
            }),
            // The gateway's vocabulary is Cohere's; Gemini spells the same
            // two purposes differently. Anything else means "unspecified",
            // which is the API's own default.
            task_type: match input_type {
                "search_query" => Some("RETRIEVAL_QUERY"),
                "search_document" => Some("RETRIEVAL_DOCUMENT"),
                _ => None,
            }
            .filter(|_| gemini_contract(&model_name_for_options).is_some_and(|c| c.task_type)),
        }
    }
}

/// The header Google documents for REST access. It replaced a `?key=…` query
/// parameter here, which is the same credential in the one place that is
/// logged by every intermediary between this process and Google: proxy access
/// logs, reverse-proxy request lines, and anything an operator points
/// `base_url` at. A header is not logged by default anywhere on that path.
const GEMINI_KEY_HEADER: &str = "x-goog-api-key";

/// The request contract for one Gemini embedding model.
///
/// One table, consulted by both sides. The loader validates a descriptor
/// against it (`config::validate_connector`) and this client builds its
/// request from it, so there is no second list to drift.
///
/// Google's embedding generations do not share a contract — whether
/// `taskType` is accepted, whether `outputDimensionality` is, and the range of
/// widths a model can produce all vary, and a newer generation expresses
/// retrieval intent as a prompt prefix instead. Getting that wrong does not
/// fail: it silently changes every vector a column stores. So this table
/// carries only models whose contract has been read off Google's
/// documentation, and the loader refuses anything else.
pub struct GeminiContract {
    pub model_id: &'static str,
    /// Accepts `taskType` on `batchEmbedContents`.
    pub task_type: bool,
    /// Accepts `outputDimensionality`, and the inclusive width range it can
    /// produce.
    pub dims: Option<(u32, u32)>,
}

/// The models this crate is willing to serve.
///
/// Deliberately one entry. `text-embedding-004` and `embedding-001` are
/// retired by Google and were listed by an earlier pass without checking
/// their lifecycle — an allow-list of dead models is worse than no
/// allow-list, because it reads as verification. Adding a model means reading
/// its current contract, not copying a neighbour's.
pub const GEMINI_CONTRACTS: &[GeminiContract] = &[GeminiContract {
    model_id: "gemini-embedding-001",
    task_type: true,
    // Google documents 128..=3072, with 3072 the native (normalised) width.
    dims: Some((128, 3072)),
}];

/// The contract for `model_id`, or `None` when this crate does not know it.
pub fn gemini_contract(model_id: &str) -> Option<&'static GeminiContract> {
    GEMINI_CONTRACTS
        .iter()
        .find(|contract| contract.model_id == model_id)
}

/// `reqwest::Error`'s `Display` includes the request URL. The URL no longer
/// carries the key (see [`GEMINI_KEY_HEADER`]), so this is now defence rather
/// than the load-bearing redaction it used to be — kept because a URL in a
/// log line buys nothing and a future `base_url` could carry a token again.
fn redacted_network(e: reqwest::Error) -> EmbeddingError {
    EmbeddingError::Network(e.without_url())
}

/// Scale a vector to unit L2 norm. A zero (or non-finite) vector is returned
/// untouched: there is no meaningful direction to preserve, and the hosts
/// already refuse a non-finite component further up.
fn l2_normalize(mut values: Vec<f32>) -> Vec<f32> {
    let norm = values
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt();
    if norm.is_finite() && norm > 0.0 {
        for value in &mut values {
            *value = (*value as f64 / norm) as f32;
        }
    }
    values
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
                task_type: self.task_type,
                output_dimensionality: self.dimensions,
            })
            .collect();

        let request_body = GeminiBatchRequest { requests };

        // The URL carries no credential: the key rides `x-goog-api-key`.
        let url = format!("{}/v1beta/{}:batchEmbedContents", self.base_url, model_path);

        // What a legitimate response to *this* call can weigh. Computed once,
        // outside the retry so every attempt shares one bound.
        let budget = crate::response_budget(texts.len(), self.dimensions);
        // Execute the request using the exponential backoff helper.
        let api_response: GeminiBatchResponse = retry_with_backoff(deadline, || async {
            // Send the POST request.
            let response = self
                .client
                .post(&url)
                .header(GEMINI_KEY_HEADER, &self.api_key)
                .json(&request_body)
                .send()
                .await
                .map_err(redacted_network)?;

            // Handle non-successful responses.
            if !response.status().is_success() {
                return Err(crate::api_error(response).await);
            }

            // Read the body first, then decode it. A failure to read is
            // transport trouble (retriable `Network`, URL stripped); a body
            // that will not parse — or one that will not fit — is permanent.
            // See `decode_json` and `read_bounded`.
            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<GeminiBatchResponse>(&bytes)
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
                // Google normalises `gemini-embedding-001` at its native
                // width and documents that a *reduced* output is not
                // normalised — the caller must do it. Skipping that leaves
                // vectors whose L2 norm is not 1, which silently changes what
                // cosine distance and inner product mean against everything
                // else in the column. Renormalising a vector that is already
                // unit-length is a no-op, so this is applied whenever a size
                // was requested rather than guessing which model is native at
                // which width.
                vector: if self.dimensions.is_some() {
                    l2_normalize(data.values)
                } else {
                    data.values
                },
            })
            .collect();

        Ok(embeddings)
    }
}
