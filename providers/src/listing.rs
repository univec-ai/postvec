//! What a provider *offers*, as opposed to what a providers.d file
//! configures.
//!
//! One contract for every connector, with an honest "cannot": UniVec's
//! public catalogue (`GET /v1/models`, unauthenticated) states model kinds
//! and dimensions, so `postvec provider add univec` can discover them; no
//! other supported provider publishes embedding dimensions, so those
//! answer [`Listing::Unsupported`] with a reason the operator can act on.
//!
//! CLI-only. The serving hosts never call this — a providers.d file is the
//! complete truth a host serves from, and it must come up offline.

use crate::{retry::retry_with_backoff, univec::DEFAULT_UNIVEC_BASE_URL, EmbeddingError};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// The catalogue body budget. UniVec's is ~100 KiB today; this is two
/// orders of headroom for `eval`/`modelCard` growth, and still a bound.
const LISTING_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ListedKind {
    Embed,
    Convert,
}

impl ListedKind {
    pub fn label(self) -> &'static str {
        match self {
            ListedKind::Embed => "embed",
            ListedKind::Convert => "convert",
        }
    }
}

/// One catalogue entry, in the provider's own vocabulary.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ListedModel {
    /// What the API wants in a request: an embed model's id, a converter's
    /// TARGET id.
    pub provider_model_id: String,
    pub kind: ListedKind,
    /// Stated by the provider; an embed's width, a converter's target width.
    pub dim: u32,
    /// Converters: `(provider_source_id, source_dim)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<(String, u32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_len: Option<u32>,
    /// UniVec `eval.cosine_mean` for converters, when published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Listing {
    /// The provider was asked and answered.
    Entries(Vec<ListedModel>),
    /// The route exists but is authenticated and no key was resolvable.
    NeedsKey { route: String },
    /// No listing route, or one that cannot identify embedding models.
    Unsupported { reason: String },
}

/// List what `provider` offers. Transport, HTTP and decode failures are
/// errors; a provider that cannot list is `Listing::Unsupported`, never an
/// error. `retries = false` makes one attempt (for best-effort callers).
pub async fn list_models(
    provider: &str,
    base_url: Option<&str>,
    timeout: Duration,
    retries: bool,
) -> Result<Listing, EmbeddingError> {
    let reason = |text: &str| {
        Ok(Listing::Unsupported {
            reason: text.to_string(),
        })
    };
    match provider {
        "univec" => {
            univec(
                base_url.unwrap_or(DEFAULT_UNIVEC_BASE_URL),
                timeout,
                retries,
            )
            .await
        }
        "openai" => reason(
            "OpenAI's model list does not state embedding dimensions; use --model with a \
             catalogue id or let `provider add` measure it",
        ),
        "openrouter" => reason(
            "OpenRouter's model list does not state embedding dimensions; use --model with a \
             catalogue id or let `provider add` measure it",
        ),
        "mistral" => reason(
            "Mistral's model list does not state embedding dimensions; use --model with a \
             catalogue id or let `provider add` measure it",
        ),
        "google" => reason(
            "the Gemini model list does not state embedding dimensions; use --model with a \
             catalogue id (postvec has per-model contracts for Gemini)",
        ),
        "cohere" => reason(
            "Cohere's model list does not state embedding dimensions; use --model with a \
             catalogue id (v3 widths are fixed, v4 takes --dim)",
        ),
        "aws" => reason(
            "Bedrock's ListFoundationModels needs SigV4 and states no dimensions; use --model \
             with a catalogue id",
        ),
        other => reason(&format!("provider {other:?} has no listing route")),
    }
}

// ---- UniVec `GET /v1/models` -------------------------------------------

/// `PublicModelView` in aphex, every field optional so an additive change
/// upstream never breaks decoding. `modelType` is an open set.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicModel {
    name: String,
    model_type: String,
    source_model: Option<String>,
    target_model: Option<String>,
    source_dim: Option<u32>,
    target_dim: Option<u32>,
    sequence_len: Option<u32>,
    eval: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    data: Option<Vec<PublicModel>>,
}

async fn univec(base: &str, timeout: Duration, retries: bool) -> Result<Listing, EmbeddingError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!("postvec/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(EmbeddingError::Network)?;
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let fetch = || async {
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(EmbeddingError::Network)?;
        if !response.status().is_success() {
            return Err(crate::api_error(response).await);
        }
        let bytes = crate::read_bounded(response, LISTING_BODY_BYTES).await?;
        crate::decode_json::<Envelope>(&bytes)
    };
    let envelope = if retries {
        retry_with_backoff(Some(Instant::now() + timeout), fetch).await?
    } else {
        fetch().await?
    };
    let models = match envelope.data {
        Some(models) if envelope.success => models,
        _ => {
            return Err(EmbeddingError::Api {
                status: 200,
                message: "UniVec answered 200 without a successful data envelope".to_string(),
            })
        }
    };
    Ok(Listing::Entries(decode_univec(models)))
}

fn decode_univec(models: Vec<PublicModel>) -> Vec<ListedModel> {
    let mut out: Vec<ListedModel> = models
        .into_iter()
        .filter_map(|m| match m.model_type.as_str() {
            "embed" => match m.target_dim {
                Some(dim) => Some(ListedModel {
                    provider_model_id: m.name,
                    kind: ListedKind::Embed,
                    dim,
                    source: None,
                    sequence_len: m.sequence_len,
                    quality: None,
                }),
                None => {
                    log::warn!(
                        "univec catalogue: embed {:?} states no dimension; skipped",
                        m.name
                    );
                    None
                }
            },
            "convert" => match (m.source_model, m.source_dim, m.target_model, m.target_dim) {
                (Some(source), Some(source_dim), Some(target), Some(dim)) => Some(ListedModel {
                    provider_model_id: target,
                    kind: ListedKind::Convert,
                    dim,
                    source: Some((source, source_dim)),
                    sequence_len: None,
                    quality: m
                        .eval
                        .as_ref()
                        .and_then(|e| e.get("cosine_mean"))
                        .and_then(serde_json::Value::as_f64),
                }),
                _ => {
                    log::warn!(
                        "univec catalogue: converter {:?} is missing a model or dimension; skipped",
                        m.name
                    );
                    None
                }
            },
            // Bridges and any future type: not something a providers.d
            // entry can describe.
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| {
        (a.kind, &a.provider_model_id, &a.source).cmp(&(b.kind, &b.provider_model_id, &b.source))
    });
    out
}

// ---- Helpers over a listing ---------------------------------------------

pub fn embed<'a>(models: &'a [ListedModel], id: &str) -> Option<&'a ListedModel> {
    models
        .iter()
        .find(|m| m.kind == ListedKind::Embed && m.provider_model_id == id)
}

pub fn converter<'a>(
    models: &'a [ListedModel],
    source: &str,
    target: &str,
) -> Option<&'a ListedModel> {
    models.iter().find(|m| {
        m.kind == ListedKind::Convert
            && m.provider_model_id == target
            && m.source.as_ref().is_some_and(|(s, _)| s == source)
    })
}

/// A converter's stated widths must agree with the listed embed models of
/// the same names, when those are listed. Aphex does not enforce this and
/// the gateway would only find out at request time.
pub fn check_converter_dims(models: &[ListedModel], conv: &ListedModel) -> Result<(), String> {
    let Some((source, source_dim)) = &conv.source else {
        return Ok(());
    };
    if let Some(e) = embed(models, source) {
        if e.dim != *source_dim {
            return Err(format!(
                "converter {source} -> {} states a source dimension of {source_dim} but the \
                 catalogue lists {source} as a {}-dimensional embed model",
                conv.provider_model_id, e.dim
            ));
        }
    }
    if let Some(e) = embed(models, &conv.provider_model_id) {
        if e.dim != conv.dim {
            return Err(format!(
                "converter {source} -> {} states a target dimension of {} but the catalogue \
                 lists {} as a {}-dimensional embed model",
                conv.provider_model_id, conv.dim, conv.provider_model_id, e.dim
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("../tests/fixtures/univec-models.json");

    fn decode(body: &str) -> Vec<ListedModel> {
        let envelope: Envelope = serde_json::from_str(body).unwrap();
        decode_univec(envelope.data.unwrap())
    }

    /// The live catalogue as captured on 2026-09-03: 18 embeds, 98
    /// converters, two bridge entries that must not appear.
    #[test]
    fn the_captured_catalogue_decodes_to_embeds_and_converters_only() {
        let models = decode(LIVE);
        let embeds = models
            .iter()
            .filter(|m| m.kind == ListedKind::Embed)
            .count();
        let converts = models
            .iter()
            .filter(|m| m.kind == ListedKind::Convert)
            .count();
        assert_eq!((embeds, converts), (18, 98));
        assert!(!models
            .iter()
            .any(|m| m.provider_model_id.contains("bridge")));

        let e = embed(&models, "alibaba-nlp-gte-base-en-v1.5").unwrap();
        assert_eq!(
            e,
            &ListedModel {
                provider_model_id: "alibaba-nlp-gte-base-en-v1.5".into(),
                kind: ListedKind::Embed,
                dim: 768,
                source: None,
                sequence_len: Some(8192),
                quality: None,
            }
        );
        let c = converter(
            &models,
            "alibaba-nlp-gte-large-en-v1.5",
            "nomic-embed-text-v1.5",
        )
        .unwrap();
        assert_eq!(c.dim, 768);
        assert_eq!(
            c.source,
            Some(("alibaba-nlp-gte-large-en-v1.5".into(), 1024))
        );
        assert_eq!(c.quality, Some(0.899327));
        // Deterministic: embeds first, then converters, each by id.
        assert!(models
            .windows(2)
            .all(|w| (w[0].kind, &w[0].provider_model_id, &w[0].source)
                <= (w[1].kind, &w[1].provider_model_id, &w[1].source)));
        // Nothing in the live catalogue contradicts itself.
        for c in models.iter().filter(|m| m.kind == ListedKind::Convert) {
            check_converter_dims(&models, c).unwrap();
        }
    }

    #[test]
    fn degenerate_entries_are_dropped_and_unknown_types_ignored() {
        let models = decode(
            r#"{"success":true,"data":[
              {"name":"no-dim","modelType":"embed"},
              {"name":"ok","modelType":"embed","targetDim":4},
              {"name":"half","modelType":"convert","sourceModel":"a","targetModel":"b"},
              {"name":"c","modelType":"convert","sourceModel":"a","sourceDim":4,"targetModel":"ok","targetDim":4},
              {"name":"future","modelType":"quantum-embed","targetDim":9},
              {"name":"bridge","modelType":"embed-bridge","restrictedTargets":["x"]}
            ]}"#,
        );
        assert_eq!(models.len(), 2);
        assert!(embed(&models, "ok").is_some());
        assert!(converter(&models, "a", "ok").is_some());
        assert!(decode(r#"{"success":true,"data":[]}"#).is_empty());
        let envelope: Envelope = serde_json::from_str(r#"{"success":false,"error":"x"}"#).unwrap();
        assert!(!envelope.success && envelope.data.is_none());
    }

    #[test]
    fn converter_dims_are_checked_against_listed_embeds() {
        let models = decode(
            r#"{"success":true,"data":[
              {"name":"s","modelType":"embed","targetDim":8},
              {"name":"t","modelType":"embed","targetDim":4},
              {"name":"c1","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":4},
              {"name":"c2","modelType":"convert","sourceModel":"s","sourceDim":9,"targetModel":"t","targetDim":4},
              {"name":"c3","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":5},
              {"name":"c4","modelType":"convert","sourceModel":"unlisted","sourceDim":3,"targetModel":"elsewhere","targetDim":2}
            ]}"#,
        );
        let with_source_dim = |n: u32| {
            models
                .iter()
                .find(|m| m.source.as_ref().is_some_and(|(_, d)| *d == n))
                .unwrap()
        };
        let c1 = converter(&models, "s", "t").unwrap();
        check_converter_dims(&models, c1).unwrap();
        let err = check_converter_dims(&models, with_source_dim(9)).unwrap_err();
        assert!(err.contains('9') && err.contains("8-dimensional"), "{err}");
        let c3 = models.iter().find(|m| m.dim == 5).unwrap();
        let err = check_converter_dims(&models, c3).unwrap_err();
        assert!(err.contains('5') && err.contains("4-dimensional"), "{err}");
        let c4 = converter(&models, "unlisted", "elsewhere").unwrap();
        check_converter_dims(&models, c4).unwrap();
    }

    /// Every connector answers without a network call when it cannot list.
    #[tokio::test]
    async fn every_connector_answers_a_listing_variant() {
        for provider in crate::config::SUPPORTED_PROVIDERS {
            if *provider == "univec" {
                continue;
            }
            match list_models(provider, None, Duration::from_millis(1), false).await {
                Ok(Listing::Unsupported { reason }) => {
                    assert!(reason.contains("--model"), "{provider}: {reason}")
                }
                other => panic!("{provider}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_univec_listing_is_fetched_and_bounded() {
        let mock = crate::testing::always(200, LIVE).await;
        match list_models("univec", Some(&mock.url), Duration::from_secs(5), false).await {
            Ok(Listing::Entries(models)) => assert_eq!(models.len(), 116),
            other => panic!("{other:?}"),
        }
        // A trailing slash on the override does not double up.
        let slashed = format!("{}/", mock.url);
        assert!(matches!(
            list_models("univec", Some(&slashed), Duration::from_secs(5), false).await,
            Ok(Listing::Entries(_))
        ));

        let broken = crate::testing::always(500, r#"{"error":"down"}"#).await;
        let err = list_models("univec", Some(&broken.url), Duration::from_secs(5), false)
            .await
            .unwrap_err();
        assert!(
            matches!(err, EmbeddingError::Api { status: 500, .. }),
            "{err:?}"
        );
        assert_eq!(broken.request_count(), 1, "no retry when retries = false");

        let flood = crate::testing::flood(200).await;
        let err = list_models("univec", Some(&flood.url), Duration::from_secs(10), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("budget"), "{err}");
        assert!(
            flood.flooded_bytes() < 16 * 1024 * 1024,
            "{}",
            flood.flooded_bytes()
        );
    }
}
