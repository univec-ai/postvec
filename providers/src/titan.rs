//! AWS Titan embeddings via Bedrock `InvokeModel`. No batching: one signed
//! call per text. Auth is SigV4 (access key) or a short-term bearer token.
//! Request schemas differ by Titan version.

use crate::{aws_sigv4, retry::retry_with_backoff, Embedding, EmbeddingBackend, EmbeddingError};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Titan v1 request body.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TitanV1Request<'a> {
    input_text: &'a str,
}

/// Represents the request body for Titan v2 models ("amazon.titan-embed-text-v2:0").
/// This schema is different and requires the output dimensions.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TitanV2Request<'a> {
    input_text: &'a str,
    dimensions: i32,
}

/// Represents the successful response from Titan models (v1 and v2 are compatible).
#[derive(Deserialize)]
struct TitanResponse {
    embedding: Vec<f32>,
}

// ---- Authentication Configuration ----

/// Defines the authentication methods supported by the Titan client.
pub enum TitanAuth {
    SigV4 {
        access_key: String,
        secret_key: String,
    },
    BearerToken(String),
}

// ---- Client Implementation ----

/// A client for generating embeddings using AWS Titan models on Bedrock.
pub struct TitanClient {
    client: Client,
    region: String,
    auth: TitanAuth,
    model_id: String,
    /// The vector dimension is stored in the client to build the correct request for v2 models.
    dimension: i32,
}

impl TitanClient {
    /// Creates a new `TitanClient`.
    pub fn new(
        model_id: String,
        region: String,
        auth: TitanAuth,
        dimension: i32, // Dimension is a required parameter
        http_client: Option<Client>,
    ) -> Self {
        Self {
            client: http_client.unwrap_or_default(),
            region,
            auth,
            model_id,
            dimension,
        }
    }

    /// Helper function to build the appropriate JSON request body based on the model ID.
    /// This handles schema differences between Titan model versions.
    fn build_request_body(&self, text: &str) -> Result<String, EmbeddingError> {
        // Check if the model ID is the specific v2 model.
        if self.model_id == "amazon.titan-embed-text-v2:0" {
            // If it's the v2 model, use the new request structure with `dimensions`.
            let body = TitanV2Request {
                input_text: text,
                dimensions: self.dimension,
            };
            serde_json::to_string(&body).map_err(EmbeddingError::Deserialization)
        } else {
            // Otherwise, fall back to the original v1 schema.
            let body = TitanV1Request { input_text: text };
            serde_json::to_string(&body).map_err(EmbeddingError::Deserialization)
        }
    }

    /// Helper function to parse the response body and extract the embedding vector.
    fn parse_response_body(&self, body_text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // The response schema is compatible for both v1 and v2.
        let response: TitanResponse =
            serde_json::from_str(body_text).map_err(EmbeddingError::Deserialization)?;
        Ok(response.embedding)
    }
}

#[async_trait]
impl EmbeddingBackend for TitanClient {
    /// Generates embeddings for a batch of texts.
    ///
    /// The Bedrock `InvokeModel` API does not batch, so each text is one
    /// signed HTTP call with its own retry budget. A failed item fails the
    /// **whole batch** with that item's error: silently skipping it would
    /// return fewer embeddings than inputs and lose rows — the caller's
    /// batch bisection is what isolates a genuinely poison row.
    async fn embed(
        &self,
        texts: &[&str],
        deadline: Option<Instant>,
    ) -> Result<Vec<Embedding>, EmbeddingError> {
        let mut results = Vec::with_capacity(texts.len());
        // One text per InvokeModel call, so one vector's worth of body.
        let budget = crate::response_budget(1, Some(self.dimension.max(0) as usize));

        for (index, text) in texts.iter().enumerate() {
            let host = format!("bedrock-runtime.{}.amazonaws.com", self.region);
            let url = format!("https://{}/model/{}/invoke", host, self.model_id);
            // Dereference `text` to `&str` so it can be captured by the closure.
            let text_str = *text;

            // This operation will be retried on failure.
            // It captures `self`, `url`, and `text_str` by reference.
            let response_text = retry_with_backoff(deadline, || async {
                // Build the request body. This can fail with a non-retriable
                // DeserializationError, which is the correct behavior.
                let body_str = self.build_request_body(text_str)?;

                // Execute the correct authentication flow (Bearer or SigV4)
                let response = match &self.auth {
                    TitanAuth::BearerToken(token) => self
                        .client
                        .post(&url)
                        .bearer_auth(token)
                        .header("content-type", "application/json")
                        .body(body_str)
                        .send()
                        .await
                        .map_err(EmbeddingError::Network)?, // Retriable network error
                    TitanAuth::SigV4 {
                        access_key,
                        secret_key,
                    } => {
                        // Building the request can fail (e.g., bad header value).
                        // This is a configuration error, not a network error, so
                        // it should not be retried.
                        let mut request = self
                            .client
                            .post(&url)
                            .header("content-type", "application/json")
                            .body(body_str)
                            .build()
                            .map_err(|e| {
                                EmbeddingError::Configuration(format!(
                                    "Failed to build SigV4 request: {}",
                                    e
                                ))
                            })?;

                        // Sign the request in place. A credential that
                        // cannot be put in a header is a configuration
                        // error, not a panic — this runs inside the
                        // PostgreSQL launcher in embedded mode.
                        aws_sigv4::sign_request(
                            &mut request,
                            access_key,
                            secret_key,
                            &self.region,
                            "bedrock",
                        )?;

                        // Execute the signed request
                        self.client
                            .execute(request)
                            .await
                            .map_err(EmbeddingError::Network)? // Retriable network error
                    }
                };

                // Check for API errors (e.g., 429, 503)
                if !response.status().is_success() {
                    return Err(crate::api_error(response).await);
                }

                // Read the body, bounded. Titan invokes one text per request,
                // so the budget is one vector's worth.
                let bytes = crate::read_bounded(response, budget).await?;
                String::from_utf8(bytes.clone()).map_err(|_| EmbeddingError::Api {
                    status: 200,
                    message: format!("response body is not valid UTF-8 ({} bytes)", bytes.len()),
                })
            })
            .await
            .map_err(|e| {
                log::warn!("Titan embed failed on item {index}: {e}");
                e
            })?;

            // Parsing the response body is done *after* the retry block,
            // as a deserialization error is permanent and should not be retried.
            let vector = self.parse_response_body(&response_text).map_err(|e| {
                log::warn!("Titan response parse failed on item {index}: {e}");
                e
            })?;
            results.push(Embedding {
                text_index: index,
                vector,
            });
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(model_id: &str, dimension: i32) -> TitanClient {
        TitanClient::new(
            model_id.to_string(),
            "us-east-1".to_string(),
            TitanAuth::BearerToken("token".to_string()),
            dimension,
            None,
        )
    }

    #[test]
    fn v2_request_body_includes_dimensions() {
        // The v2 model schema carries `dimensions`; the field is camelCased.
        let c = client("amazon.titan-embed-text-v2:0", 512);
        let body = c.build_request_body("hello").unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["inputText"], "hello");
        assert_eq!(v["dimensions"], 512);
    }

    #[test]
    fn v1_request_body_omits_dimensions() {
        // Any non-v2 model uses the v1 schema, which has no `dimensions` field.
        let c = client("amazon.titan-embed-text-v1", 512);
        let body = c.build_request_body("hi").unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["inputText"], "hi");
        assert!(v.get("dimensions").is_none(), "v1 must omit dimensions");
    }

    #[test]
    fn parses_embedding_response() {
        let c = client("amazon.titan-embed-text-v1", 0);
        let vec = c
            .parse_response_body(r#"{"embedding":[0.1,0.2,0.3]}"#)
            .unwrap();
        assert_eq!(vec, vec![0.1, 0.2, 0.3]);
    }

    #[test]
    fn parse_response_rejects_malformed_body() {
        let c = client("amazon.titan-embed-text-v1", 0);
        let err = c.parse_response_body("garbage").unwrap_err();
        assert!(matches!(err, EmbeddingError::Deserialization(_)), "{err:?}");
    }

    /// The silent-drop fix: a failed item must fail the batch, never return
    /// `Ok` with fewer embeddings than inputs. An already-exhausted deadline
    /// forces the first item's attempt to be refused deterministically
    /// without any network access.
    #[tokio::test]
    async fn a_failed_item_fails_the_whole_batch() {
        let c = client("amazon.titan-embed-text-v1", 0);
        let deadline = Instant::now() - std::time::Duration::from_millis(1);
        let err = c.embed(&["a", "b"], Some(deadline)).await.unwrap_err();
        assert!(matches!(err, EmbeddingError::Deadline(_)), "{err:?}");
    }
}
