//! UniVec hosted API: embeddings and vector-space conversion.
//!
//! Embed: OpenAI-compatible `POST {base}/v1/embeddings`, plus `input_type`
//! (query/document template; safe to send even when the model has none) and
//! `dimensions` (Matryoshka truncation, re-normalised server-side).
//! Convert: native `POST {base}/v1/convert` with provider-side model ids
//! and a `{"success", "data"}` envelope. Auth is Bearer. HTTP 402 (out of
//! credit) maps with 401/403: an ops problem, never a row's fault.

use crate::{
    retry::retry_with_backoff, ConversionBackend, Embedding, EmbeddingBackend, EmbeddingError,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// The default base URL for the UniVec API.
pub const DEFAULT_UNIVEC_BASE_URL: &str = "https://api.univec.ai";

// ---- Embedding (OpenAI-compatible endpoint) ----

/// The OpenAI-shaped request body, plus UniVec's documented extensions.
#[derive(Serialize)]
struct UnivecEmbedRequest<'a> {
    /// The UniVec public model name (with or without the `univec/` prefix;
    /// this connector sends the descriptor's `provider_model_id` verbatim).
    model: &'a str,
    input: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    /// Not a stock OpenAI field: the model's prompt-template selector,
    /// `search_document` or `search_query`.
    input_type: &'a str,
}

#[derive(Deserialize)]
struct UnivecEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Deserialize)]
struct UnivecEmbedResponse {
    data: Vec<UnivecEmbeddingData>,
}

/// A client for UniVec's OpenAI-compatible embeddings endpoint.
pub struct UnivecClient {
    client: Client,
    base_url: String,
    api_key: String,
    model_name: String,
    dimensions: Option<usize>,
    /// Constructor-level, like every purpose-aware connector: the gateway
    /// holds one client per purpose rather than threading a per-call flag.
    input_type: String,
}

impl UnivecClient {
    pub fn new(
        model_name: String,
        api_key: String,
        dimensions: Option<usize>,
        input_type: &str,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        Self {
            client: http_client.unwrap_or_default(),
            base_url,
            api_key,
            model_name,
            dimensions,
            input_type: input_type.to_string(),
        }
    }
}

#[async_trait]
impl EmbeddingBackend for UnivecClient {
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let request_body = UnivecEmbedRequest {
            model: &self.model_name,
            input: texts,
            dimensions: self.dimensions,
            input_type: &self.input_type,
        };

        let url = format!("{}/v1/embeddings", self.base_url);

        // What a legitimate response to *this* call can weigh. Computed once,
        // outside the retry, so every attempt shares one bound.
        let budget = crate::response_budget(texts.len(), self.dimensions);
        let api_response: UnivecEmbedResponse = retry_with_backoff(deadline, || async {
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

            // Read then decode: a failure to read is transport trouble
            // (retriable `Network`); a body that will not parse is permanent
            // for this response. See `decode_json`.
            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<UnivecEmbedResponse>(&bytes)
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

        // Sort by index so the output order matches the input text order.
        embeddings.sort_by_key(|e| e.text_index);

        Ok(embeddings)
    }
}

// ---- Conversion (native endpoint) ----

#[derive(Serialize)]
struct UnivecConvertRequest<'a> {
    /// UniVec public name of the space the input vectors are in.
    source_model: &'a str,
    /// UniVec public name of the space the output vectors land in.
    target_model: &'a str,
    embeddings: &'a [Vec<f32>],
}

#[derive(Deserialize)]
struct UnivecConvertData {
    embeddings: Vec<Vec<f32>>,
}

/// UniVec's native endpoints wrap every success in this envelope.
#[derive(Deserialize)]
struct UnivecEnvelope<T> {
    #[serde(default)]
    success: bool,
    data: Option<T>,
}

/// A client for UniVec's native conversion endpoint.
pub struct UnivecConvertClient {
    client: Client,
    base_url: String,
    api_key: String,
    source_model: String,
    target_model: String,
    /// Expected output width; sizes the response read budget. The gateway
    /// still validates every returned vector against the descriptor.
    target_dim: usize,
}

impl UnivecConvertClient {
    pub fn new(
        source_model: String,
        target_model: String,
        target_dim: usize,
        api_key: String,
        base_url: String,
        http_client: Option<Client>,
    ) -> Self {
        Self {
            client: http_client.unwrap_or_default(),
            base_url,
            api_key,
            source_model,
            target_model,
            target_dim,
        }
    }
}

#[async_trait]
impl ConversionBackend for UnivecConvertClient {
    async fn convert(
        &self,
        embeddings: &[Vec<f32>],
        deadline: Option<Instant>,
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let request_body = UnivecConvertRequest {
            source_model: &self.source_model,
            target_model: &self.target_model,
            embeddings,
        };

        let url = format!("{}/v1/convert", self.base_url);

        let budget = crate::response_budget(embeddings.len(), Some(self.target_dim));
        let envelope: UnivecEnvelope<UnivecConvertData> = retry_with_backoff(deadline, || async {
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

            let bytes = crate::read_bounded(response, budget).await?;
            crate::decode_json::<UnivecEnvelope<UnivecConvertData>>(&bytes)
        })
        .await?;

        // A 200 whose envelope says otherwise is permanent for this response,
        // exactly like a body that will not parse — and like every other
        // upstream-body path, the message here is authored locally, never
        // quoted from the response.
        match envelope.data {
            Some(data) if envelope.success => Ok(data.embeddings),
            _ => Err(EmbeddingError::Api {
                status: 200,
                message: "UniVec answered 200 without a successful data envelope".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The request bodies are the documented wire shapes — pinned here
    /// because a silently renamed field would not fail, it would change what
    /// the API does (a dropped `input_type` embeds queries and documents
    /// identically while the descriptor claims otherwise).
    #[test]
    fn request_bodies_are_the_documented_wire_shapes() {
        let embed = UnivecEmbedRequest {
            model: "snowflake-arctic-embed-l-v2.0",
            input: &["a", "b"],
            dimensions: Some(512),
            input_type: "search_query",
        };
        let value = serde_json::to_value(&embed).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "model": "snowflake-arctic-embed-l-v2.0",
                "input": ["a", "b"],
                "dimensions": 512,
                "input_type": "search_query",
            })
        );

        let convert = UnivecConvertRequest {
            source_model: "openai-ada-002",
            target_model: "gemini-text-embedding-004",
            embeddings: &[vec![0.25, -0.5]],
        };
        let value = serde_json::to_value(&convert).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "source_model": "openai-ada-002",
                "target_model": "gemini-text-embedding-004",
                "embeddings": [[0.25, -0.5]],
            })
        );
    }

    /// `dimensions` is omitted, not null, when unset — UniVec treats an
    /// explicit null as a bad request.
    #[test]
    fn unset_dimensions_are_omitted() {
        let embed = UnivecEmbedRequest {
            model: "m",
            input: &["a"],
            dimensions: None,
            input_type: "search_document",
        };
        let value = serde_json::to_value(&embed).unwrap();
        assert!(value.get("dimensions").is_none());
    }

    /// The native envelope refusal is authored locally: a 200 with
    /// `success: false` (or no data) must never quote the body.
    #[tokio::test]
    async fn a_success_false_envelope_is_a_permanent_local_error() {
        let envelope: UnivecEnvelope<UnivecConvertData> =
            serde_json::from_str(r#"{"success": false, "error": {"message": "secret text"}}"#)
                .unwrap();
        assert!(!envelope.success);
        assert!(envelope.data.is_none());
    }
}
