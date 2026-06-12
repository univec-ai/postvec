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
    decode_univec(models)
        .map(Listing::Entries)
        .map_err(|message| EmbeddingError::Api {
            status: 200,
            message: format!("UniVec's catalogue violates its contract: {message}"),
        })
}

/// pgvector's ceiling; a stated width outside `1..=MAX_DIM` is a broken row.
const MAX_DIM: u32 = 16_000;
/// A stated `sequenceLen` outside this is a broken row too.
const MAX_SEQUENCE_LEN: u32 = 1 << 20;
/// A model identifier goes into request bodies, derived public names and
/// terminal output: printable ASCII only, bounded (the loader's own
/// `provider_model_id` ceiling is 256 bytes; `univec-` is prepended).
const MAX_ID_BYTES: usize = 200;

/// Decode the catalogue, failing closed on a broken row of a kind postvec
/// understands. Unknown `modelType`s (bridges, future kinds) are ignored:
/// that is the forward-compatibility rule. But an `embed` without
/// `targetModel`/`targetDim`, a `convert` missing a model or width, a width
/// outside pgvector's range, or two rows that give one identity conflicting
/// widths is a contract violation — dropping it would let a zero-selector
/// add report success over a partial catalogue. Identical duplicates
/// (aliases: two `name`s with one `targetModel`) collapse to one entry.
///
/// An embed's request id is `targetModel`, not `name`: aphex resolves the
/// `model` of an embeddings request as the semantic target, and permits
/// `name != targetModel` for an alias SKU.
fn decode_univec(models: Vec<PublicModel>) -> Result<Vec<ListedModel>, String> {
    let mut out: Vec<ListedModel> = Vec::new();
    for (index, m) in models.into_iter().enumerate() {
        let row = || format!("catalogue row {index} ({})", safe_name(&m.name));
        let width = |field: &str, value: Option<u32>| -> Result<u32, String> {
            match value {
                Some(d) if (1..=MAX_DIM).contains(&d) => Ok(d),
                Some(d) => Err(format!("{}: {field} {d} is outside 1..={MAX_DIM}", row())),
                None => Err(format!("{}: {field} is missing", row())),
            }
        };
        let ident = |field: &str, value: &Option<String>| -> Result<String, String> {
            match value {
                Some(id)
                    if !id.is_empty()
                        && id.len() <= MAX_ID_BYTES
                        && id.bytes().all(|b| b.is_ascii_graphic()) =>
                {
                    Ok(id.clone())
                }
                Some(_) => Err(format!(
                    "{}: {field} is not a printable identifier of 1..={MAX_ID_BYTES} bytes",
                    row()
                )),
                None => Err(format!("{}: {field} is missing", row())),
            }
        };
        let entry = match m.model_type.as_str() {
            "embed" => ListedModel {
                dim: width("targetDim", m.target_dim)?,
                provider_model_id: ident("targetModel", &m.target_model)?,
                kind: ListedKind::Embed,
                source: None,
                sequence_len: match m.sequence_len {
                    Some(n) if !(1..=MAX_SEQUENCE_LEN).contains(&n) => {
                        return Err(format!(
                            "{}: sequenceLen {n} is outside 1..={MAX_SEQUENCE_LEN}",
                            row()
                        ))
                    }
                    other => other,
                },
                quality: None,
            },
            "convert" => ListedModel {
                dim: width("targetDim", m.target_dim)?,
                provider_model_id: ident("targetModel", &m.target_model)?,
                kind: ListedKind::Convert,
                source: Some((
                    ident("sourceModel", &m.source_model)?,
                    width("sourceDim", m.source_dim)?,
                )),
                sequence_len: None,
                quality: m
                    .eval
                    .as_ref()
                    .and_then(|e| e.get("cosine_mean"))
                    .and_then(serde_json::Value::as_f64),
            },
            _ => continue,
        };
        // One semantic identity may be listed under several SKU names, but
        // every operational fact must agree: widths and `sequenceLen`
        // (persisted as `max_tokens`). `quality` is display-only, so a
        // disagreement blanks it rather than refusing.
        match out.iter_mut().find(|e| {
            e.kind == entry.kind
                && e.provider_model_id == entry.provider_model_id
                && e.source.as_ref().map(|(s, _)| s) == entry.source.as_ref().map(|(s, _)| s)
        }) {
            None => out.push(entry),
            Some(existing)
                if existing.dim == entry.dim
                    && existing.source.as_ref().map(|(_, d)| d)
                        == entry.source.as_ref().map(|(_, d)| d) =>
            {
                if existing.sequence_len != entry.sequence_len {
                    return Err(format!(
                        "{}: embed {} is listed twice with different sequenceLen ({:?} and {:?})",
                        row(),
                        safe_name(&entry.provider_model_id),
                        existing.sequence_len,
                        entry.sequence_len
                    ));
                }
                if existing.quality != entry.quality {
                    existing.quality = None;
                }
            }
            Some(existing) => {
                return Err(format!(
                    "{}: {} {} is listed twice with different dimensions ({}{} and {}{})",
                    row(),
                    entry.kind.label(),
                    safe_name(&entry.provider_model_id),
                    existing
                        .source
                        .as_ref()
                        .map(|(_, d)| format!("{d}->"))
                        .unwrap_or_default(),
                    existing.dim,
                    entry
                        .source
                        .as_ref()
                        .map(|(_, d)| format!("{d}->"))
                        .unwrap_or_default(),
                    entry.dim
                ))
            }
        }
    }
    out.sort_by(|a, b| {
        (a.kind, &a.provider_model_id, &a.source).cmp(&(b.kind, &b.provider_model_id, &b.source))
    });
    Ok(out)
}

/// A catalogue name as it may appear in a message: the model-name charset
/// only, bounded — upstream text never reaches a log verbatim.
fn safe_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        .take(64)
        .collect();
    if out.is_empty() {
        out.push('?');
    }
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
        try_decode(body).unwrap()
    }

    fn try_decode(body: &str) -> Result<Vec<ListedModel>, String> {
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

    /// The request id is `targetModel`; an alias SKU whose `name` differs
    /// collapses onto the same identity, and the probe must carry the
    /// target name.
    #[test]
    fn an_alias_sku_resolves_to_its_target_model() {
        let models = decode(
            r#"{"success":true,"data":[
              {"name":"internal-v2","modelType":"embed","targetModel":"stable","targetDim":4,"sequenceLen":512},
              {"name":"stable","modelType":"embed","targetModel":"stable","targetDim":4,"sequenceLen":512}
            ]}"#,
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider_model_id, "stable");
        assert!(embed(&models, "internal-v2").is_none());
    }

    /// Unknown kinds are ignored; a broken row of a known kind fails the
    /// whole decode with the row named — never a silent subset.
    #[test]
    fn malformed_known_rows_fail_closed_and_unknown_kinds_are_ignored() {
        let ok = decode(
            r#"{"success":true,"data":[
              {"name":"ok","modelType":"embed","targetModel":"ok","targetDim":4},
              {"name":"future","modelType":"quantum-embed","targetDim":9},
              {"name":"bridge","modelType":"embed-bridge","restrictedTargets":["x"]}
            ]}"#,
        );
        assert_eq!(ok.len(), 1);
        assert!(decode(r#"{"success":true,"data":[]}"#).is_empty());

        let err =
            |rows: &str| try_decode(&format!(r#"{{"success":true,"data":[{rows}]}}"#)).unwrap_err();
        let e = err(r#"{"name":"no dim <b>","modelType":"embed","targetModel":"x"}"#);
        assert!(
            e.contains("row 0 (nodimb)") && e.contains("targetDim is missing"),
            "{e}"
        );
        let e = err(r#"{"name":"no target","modelType":"embed","targetDim":4}"#);
        assert!(e.contains("targetModel is missing"), "{e}");
        let e = err(r#"{"name":"zero","modelType":"embed","targetModel":"z","targetDim":0}"#);
        assert!(e.contains("outside 1..=16000"), "{e}");
        let e = err(
            r#"{"name":"huge","modelType":"convert","sourceModel":"a","sourceDim":99999,"targetModel":"b","targetDim":4}"#,
        );
        assert!(e.contains("sourceDim 99999"), "{e}");
        let e = err(
            r#"{"name":"half","modelType":"convert","sourceModel":"a","targetModel":"b","targetDim":4}"#,
        );
        assert!(e.contains("sourceDim is missing"), "{e}");
        // Conflicting widths for one identity; identical repeats collapse.
        let e = err(
            r#"{"name":"a","modelType":"embed","targetModel":"a","targetDim":4},
               {"name":"a2","modelType":"embed","targetModel":"a","targetDim":8}"#,
        );
        assert!(e.contains("row 1 (a2)") && e.contains("4 and 8"), "{e}");
        let e = err(
            r#"{"name":"c","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":4},
               {"name":"c2","modelType":"convert","sourceModel":"s","sourceDim":9,"targetModel":"t","targetDim":4}"#,
        );
        assert!(e.contains("8->4 and 9->4"), "{e}");
        let same = decode(
            r#"{"success":true,"data":[
              {"name":"c","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":4,"eval":{"cosine_mean":0.9}},
              {"name":"c-again","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":4,"eval":{"cosine_mean":0.8}}
            ]}"#,
        );
        assert_eq!(same.len(), 1);
        assert_eq!(
            same[0].quality, None,
            "disagreeing display metadata is blanked"
        );
        // Operational metadata must agree across aliases.
        let e = err(
            r#"{"name":"a","modelType":"embed","targetModel":"a","targetDim":4,"sequenceLen":512},
               {"name":"a2","modelType":"embed","targetModel":"a","targetDim":4,"sequenceLen":256}"#,
        );
        assert!(
            e.contains("different sequenceLen") && e.contains("512") && e.contains("256"),
            "{e}"
        );
        let e = err(
            r#"{"name":"z","modelType":"embed","targetModel":"z","targetDim":4,"sequenceLen":0}"#,
        );
        assert!(e.contains("sequenceLen 0"), "{e}");
        let e = err(
            r#"{"name":"z","modelType":"embed","targetModel":"z","targetDim":4,"sequenceLen":9999999}"#,
        );
        assert!(e.contains("sequenceLen 9999999"), "{e}");
        // Identifiers are untrusted structured input: empty, overlong,
        // whitespace, control characters and escapes are refused.
        for bad in [
            "",
            "has space",
            "new\nline",
            "tab\there",
            "esc\u{001b}[31m",
            "ünïcode",
        ] {
            let rows = format!(
                r#"{{"name":"x","modelType":"embed","targetModel":{},"targetDim":4}}"#,
                serde_json::to_string(bad).unwrap()
            );
            let e = err(&rows);
            assert!(
                e.contains("targetModel is not a printable identifier"),
                "{bad:?}: {e}"
            );
        }
        let long = "a".repeat(201);
        let e = err(&format!(
            r#"{{"name":"x","modelType":"convert","sourceModel":"{long}","sourceDim":4,"targetModel":"t","targetDim":4}}"#
        ));
        assert!(
            e.contains("sourceModel is not a printable identifier"),
            "{e}"
        );
        assert!(decode(&format!(r#"{{"success":true,"data":[{{"name":"x","modelType":"embed","targetModel":"{}","targetDim":4}}]}}"#, "a".repeat(200))).len() == 1);
        let envelope: Envelope = serde_json::from_str(r#"{"success":false,"error":"x"}"#).unwrap();
        assert!(!envelope.success && envelope.data.is_none());
    }

    #[test]
    fn converter_dims_are_checked_against_listed_embeds() {
        let conv = |s: &str, sd: u32, t: &str, td: u32| ListedModel {
            provider_model_id: t.into(),
            kind: ListedKind::Convert,
            dim: td,
            source: Some((s.into(), sd)),
            sequence_len: None,
            quality: None,
        };
        let models = decode(
            r#"{"success":true,"data":[
              {"name":"s","modelType":"embed","targetModel":"s","targetDim":8},
              {"name":"t","modelType":"embed","targetModel":"t","targetDim":4},
              {"name":"c1","modelType":"convert","sourceModel":"s","sourceDim":8,"targetModel":"t","targetDim":4},
              {"name":"c4","modelType":"convert","sourceModel":"unlisted","sourceDim":3,"targetModel":"elsewhere","targetDim":2}
            ]}"#,
        );
        check_converter_dims(&models, converter(&models, "s", "t").unwrap()).unwrap();
        let err = check_converter_dims(&models, &conv("s", 9, "t", 4)).unwrap_err();
        assert!(err.contains('9') && err.contains("8-dimensional"), "{err}");
        let err = check_converter_dims(&models, &conv("s", 8, "t", 5)).unwrap_err();
        assert!(err.contains('5') && err.contains("4-dimensional"), "{err}");
        check_converter_dims(
            &models,
            converter(&models, "unlisted", "elsewhere").unwrap(),
        )
        .unwrap();
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
