//!
//! providers/src/factory.rs
//!
//! Provides a centralized factory function for creating `EmbeddingBackend` clients.
//!
//! This module abstracts away the specific details of how each provider's
//! client is instantiated. The caller resolves all configuration up front —
//! credentials (from a providers.d file's `api_key` / `api_key_file` /
//! `api_key_env` indirection), base URL overrides, and the AWS auth variant —
//! into a [`ProviderConfig`]; the factory never reads the process
//! environment itself. For operators who choose `api_key_env`, the
//! conventional variable names are `OPENAI_API_KEY`, `OPENROUTER_API_KEY`,
//! `MISTRAL_API_KEY`, `GEMINI_API_KEY`, `COHERE_API_KEY`, `UNIVEC_API_KEY`,
//! and for AWS `AWS_REGION`, `AWS_BEARER_TOKEN_BEDROCK`, `AWS_ACCESS_KEY_ID`,
//! `AWS_SECRET_ACCESS_KEY` — but the config layer, not this module, reads
//! them.
//!
use crate::{
    cohere::{CohereClient, DEFAULT_COHERE_BASE_URL},
    gemini::{GeminiClient, DEFAULT_GEMINI_BASE_URL},
    mistral::{MistralClient, DEFAULT_MISTRAL_BASE_URL},
    openai::{OpenAIClient, DEFAULT_OPENAI_BASE_URL},
    openrouter::{OpenRouterClient, DEFAULT_OPENROUTER_BASE_URL},
    titan::{TitanAuth, TitanClient},
    univec::{UnivecClient, UnivecConvertClient, DEFAULT_UNIVEC_BASE_URL},
    ConversionBackend, EmbeddingBackend, EmbeddingError,
};

/// Fully resolved connector configuration: every secret is already a value
/// (any file/env indirection was resolved by the caller at load time).
///
/// `Debug` is implemented by hand and redacts every secret field — a stray
/// `log::debug!("{config:?}")` or panic payload must never put a key in the
/// PostgreSQL log.
#[derive(Clone, Default)]
pub struct ProviderConfig {
    /// Connector type: `openai | openrouter | mistral | google | cohere |
    /// aws | univec`. `gemini` is accepted as an alias for `google`, and
    /// `amazon` for `aws`.
    pub provider: String,
    /// API key for the key-authenticated providers.
    pub api_key: Option<String>,
    /// Optional base-URL override (Azure-style fronts, mock servers).
    pub base_url: Option<String>,
    /// AWS only: the Bedrock region.
    pub region: Option<String>,
    /// AWS only: a Bedrock API bearer token (preferred over the SigV4 pair
    /// when both are set, matching the conventional env-var precedence).
    pub bearer_token: Option<String>,
    /// AWS only: SigV4 static credentials.
    pub access_key_id: Option<String>,
    /// AWS only: SigV4 static credentials.
    pub secret_access_key: Option<String>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn redacted(secret: &Option<String>) -> &'static str {
            match secret {
                Some(_) => "Some(<redacted>)",
                None => "None",
            }
        }
        f.debug_struct("ProviderConfig")
            .field("provider", &self.provider)
            .field("api_key", &redacted(&self.api_key))
            .field("base_url", &self.base_url)
            .field("region", &self.region)
            .field("bearer_token", &redacted(&self.bearer_token))
            // The access key id is not strictly secret, but nothing needs it
            // in a log either — redact the whole SigV4 pair.
            .field("access_key_id", &redacted(&self.access_key_id))
            .field("secret_access_key", &redacted(&self.secret_access_key))
            .finish()
    }
}

impl ProviderConfig {
    fn require_api_key(&self) -> Result<String, EmbeddingError> {
        self.api_key.clone().ok_or_else(|| {
            EmbeddingError::Authentication(format!(
                "provider {:?} has no API key configured",
                self.provider
            ))
        })
    }

    fn base_url_or(&self, default: &str) -> String {
        self.base_url
            .clone()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| default.to_string())
    }
}

/// Constructs a provider-specific embedding client from resolved configuration.
///
/// # Arguments
/// * `config` - The resolved provider configuration (credentials as values).
/// * `provider_model_id` - The specific model identifier required by the provider's API.
/// * `dimension` - The dimensionality of the embedding vectors for the given
///   model; `0` is the sentinel for "infer" (the provider's native size).
/// * `input_type` - The Cohere use case ("search_document" / "search_query");
///   ignored by every other connector. Kept a constructor-level choice so a
///   host can hold one client per purpose.
/// * `http_client` - An optional shared `reqwest::Client` to reuse connections
///   (one per provider, built by the gateway with rustls).
///
/// # Returns
/// A `Result` containing a trait object for the appropriate client, or an
/// `EmbeddingError`.
pub fn new_embedding_backend(
    config: &ProviderConfig,
    provider_model_id: &str,
    dimension: i32,
    input_type: &str,
    http_client: Option<reqwest::Client>,
) -> Result<Box<dyn EmbeddingBackend>, EmbeddingError> {
    // If dimension is 0 (our sentinel for 'infer'), pass `None`.
    // Otherwise, pass `Some(dimension)`.
    let dimensions_opt = if dimension > 0 {
        Some(dimension as usize)
    } else {
        None
    };

    match config.provider.to_lowercase().as_str() {
        "openai" => Ok(Box::new(OpenAIClient::new(
            provider_model_id.to_string(),
            config.require_api_key()?,
            dimensions_opt,
            config.base_url_or(DEFAULT_OPENAI_BASE_URL),
            http_client,
        ))),
        "openrouter" => Ok(Box::new(OpenRouterClient::new(
            provider_model_id.to_string(),
            config.require_api_key()?,
            dimensions_opt,
            config.base_url_or(DEFAULT_OPENROUTER_BASE_URL),
            http_client,
        ))),
        "mistral" => {
            // Mistral embedding models emit a fixed-size vector and reject the
            // `dimensions` request field, so it is intentionally not threaded
            // through to the client — the `dimension` argument is ignored here.
            Ok(Box::new(MistralClient::new(
                provider_model_id.to_string(),
                config.require_api_key()?,
                config.base_url_or(DEFAULT_MISTRAL_BASE_URL),
                http_client,
            )))
        }
        // "gemini" is a CLI-friendly alias; the canonical connector type
        // stays "google".
        // Gemini takes both the purpose and the output size: `taskType` is
        // quality-relevant for retrieval, and `outputDimensionality` is what
        // makes a descriptor's `dim` mean anything other than the model's
        // native width.
        "google" | "gemini" => Ok(Box::new(GeminiClient::new(
            provider_model_id.to_string(),
            config.require_api_key()?,
            dimensions_opt,
            input_type,
            config.base_url_or(DEFAULT_GEMINI_BASE_URL),
            http_client,
        ))),
        // UniVec's OpenAI-compatible endpoint takes both the purpose and the
        // output size: `input_type` selects the model's query/document prompt
        // template, and `dimensions` (Matryoshka truncation, re-normalised
        // server-side) is what makes the descriptor's `dim` mean anything
        // other than the model's native width.
        "univec" => Ok(Box::new(UnivecClient::new(
            provider_model_id.to_string(),
            config.require_api_key()?,
            dimensions_opt,
            input_type,
            config.base_url_or(DEFAULT_UNIVEC_BASE_URL),
            http_client,
        ))),
        "cohere" => Ok(Box::new(CohereClient::new(
            provider_model_id.to_string(),
            config.require_api_key()?,
            input_type.to_string(),
            // embed-v4.0 accepts an output size; the v3 models ignore it.
            // Same reason as Gemini: the descriptor's `dim` is authoritative
            // and the gateway checks every response against it.
            dimensions_opt,
            config.base_url_or(DEFAULT_COHERE_BASE_URL),
            http_client,
        ))),
        "aws" | "amazon" => {
            let region = config.region.clone().ok_or_else(|| {
                EmbeddingError::Configuration(format!(
                    "provider {:?} has no AWS region configured",
                    config.provider
                ))
            })?;

            let auth = if let Some(token) = &config.bearer_token {
                TitanAuth::BearerToken(token.clone())
            } else {
                let access_key = config.access_key_id.clone().ok_or_else(|| {
                    EmbeddingError::Authentication(
                        "for AWS, configure either a Bedrock bearer token or both an access \
                         key id and a secret access key"
                            .to_string(),
                    )
                })?;
                let secret_key = config.secret_access_key.clone().ok_or_else(|| {
                    EmbeddingError::Authentication(
                        "AWS secret access key is not configured".to_string(),
                    )
                })?;
                TitanAuth::SigV4 {
                    access_key,
                    secret_key,
                }
            };

            // Pass the dimension to the TitanClient constructor.
            Ok(Box::new(TitanClient::new(
                provider_model_id.to_string(),
                region,
                auth,
                dimension,
                http_client,
            )))
        }
        _ => Err(EmbeddingError::Configuration(format!(
            "Unsupported provider: {}",
            config.provider
        ))),
    }
}

/// Constructs the conversion client for a provider-backed converter entry
/// (`kind = "convert"` in a providers.d file).
///
/// Only UniVec offers hosted vector conversion; a converter entry under any
/// other connector type is refused at load
/// (`config::validate_converter_for_provider`), so a serving host never
/// reaches the error arm — it exists for the same reason the embedding
/// factory's does: the factory must not trust its callers.
///
/// # Arguments
/// * `provider_source_id` - The provider-side id of the SOURCE space.
/// * `provider_model_id` - The provider-side id of the TARGET space.
/// * `target_dim` - The descriptor's output dimension; sizes the response
///   read budget (the gateway still validates every returned vector).
pub fn new_conversion_backend(
    config: &ProviderConfig,
    provider_source_id: &str,
    provider_model_id: &str,
    target_dim: u32,
    http_client: Option<reqwest::Client>,
) -> Result<Box<dyn ConversionBackend>, EmbeddingError> {
    match config.provider.to_lowercase().as_str() {
        "univec" => Ok(Box::new(UnivecConvertClient::new(
            provider_source_id.to_string(),
            provider_model_id.to_string(),
            target_dim as usize,
            config.require_api_key()?,
            config.base_url_or(DEFAULT_UNIVEC_BASE_URL),
            http_client,
        ))),
        other => Err(EmbeddingError::Configuration(format!(
            "provider {other:?} does not offer hosted vector conversion"
        ))),
    }
}
