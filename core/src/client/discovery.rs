// SPDX-License-Identifier: PostgreSQL
//! Model discovery: `GET /config` on the configured inference HTTP endpoints.
//!
//! Poll every configured node and aggregate models by internal name. The
//! SQL-cache upsert lives in api/; this module is transport and parsing.

use super::{ModelInfo, PvError};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::time::Duration;

/// Upper bound on a `/config` response body. A healthy config is a few
/// hundred KB at most; the cap keeps a misconfigured endpoint (or anything
/// else answering on that port) from ballooning worker memory.
// 16 MiB: a real /config with hundreds of models is well under 1 MiB.
const MAX_CONFIG_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Aggregate ceiling across ALL nodes in one refresh. Exceeding it fails the
/// refresh (the cache keeps its previous rows) rather than materializing an
/// unbounded model map.
const MAX_MODELS_TOTAL: usize = 20_000;

/// How many nodes one refresh polls concurrently. Callers sizing an overall
/// refresh budget must account for the resulting waves:
/// `ceil(endpoints / DISCOVERY_CONCURRENCY)` per-node timeouts back to back.
pub const DISCOVERY_CONCURRENCY: usize = 4;

/// One shared HTTP client per process: connection pooling + one TLS config,
/// instead of a fresh client (new pool, new TLS context) per refresh.
/// Per-request timeouts are applied on the request builder.
///
/// The inference host may serve HTTPS with a local self-signed certificate;
/// discovery accepts those certs too
/// (danger_accept_invalid_certs, same as the hosted gateway's discovery) —
/// private-network-only trust.
static HTTP_CLIENT: Lazy<Result<reqwest::Client, String>> = Lazy::new(|| {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| format!("reqwest client: {e}"))
});

#[derive(Debug, Clone)]
pub struct DiscoveryReport {
    pub models: Vec<ModelInfo>,
    /// True only when every configured HTTP endpoint answered successfully.
    /// Cache pruning is safe only for complete refreshes.
    pub complete: bool,
    pub ok_nodes: usize,
    pub failed_nodes: usize,
}

/// `{ "success": true, "data": ... }` envelope every inference HTTP route uses.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    data: Option<ConfigData>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ConfigData {
    #[serde(default)]
    models: Vec<HubModel>,
}

/// Mirrors the inference host's HubModel envelope (the subset postvec needs).
#[derive(Debug, Deserialize)]
struct HubModel {
    name: String,
    configuration: Option<ModelConfiguration>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct ModelConfiguration {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    params: ModelParams,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct ModelParams {
    model_type: Option<String>,
    source_model: Option<String>,
    target_model: Option<String>,
    source_dim: Option<u32>,
    target_dim: Option<u32>,
    sequence_len: Option<u32>,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

impl HubModel {
    /// Same rule as the hosted gateway: explicit param wins, else inferred
    /// from source/target presence.
    fn model_type(&self) -> String {
        let params = match &self.configuration {
            Some(c) => &c.params,
            None => return "legacy".to_string(),
        };
        if let Some(t) = &params.model_type {
            return match t.as_str() {
                "convert" | "embed" | "embed-bridge" | "convert-bridge" => t.clone(),
                _ => "legacy".to_string(),
            };
        }
        match (params.source_model.is_some(), params.target_model.is_some()) {
            (true, true) => "convert".to_string(),
            (false, true) => "embed".to_string(),
            _ => "legacy".to_string(),
        }
    }

    fn enabled(&self) -> bool {
        self.configuration.as_ref().is_some_and(|c| c.enabled)
    }

    fn into_model_info(self) -> ModelInfo {
        let model_type = self.model_type();
        let (params_snapshot, config_rest) = match &self.configuration {
            Some(c) => (
                serde_json::json!({
                    "model_type": c.params.model_type,
                    "source_model": c.params.source_model,
                    "target_model": c.params.target_model,
                    "source_dim": c.params.source_dim,
                    "target_dim": c.params.target_dim,
                    "sequence_len": c.params.sequence_len,
                    "extra": c.params.rest,
                }),
                serde_json::Value::Object(c.rest.clone()),
            ),
            None => (serde_json::Value::Null, serde_json::Value::Null),
        };
        let params = self.configuration.as_ref().map(|c| &c.params);
        ModelInfo {
            model_type,
            source_model: params.and_then(|p| p.source_model.clone()),
            target_model: params.and_then(|p| p.target_model.clone()),
            source_dim: params.and_then(|p| p.source_dim),
            target_dim: params.and_then(|p| p.target_dim),
            sequence_len: params.and_then(|p| p.sequence_len),
            raw: serde_json::json!({
                "name": self.name,
                "params": params_snapshot,
                "configuration_extra": config_rest,
                "extra": serde_json::Value::Object(self.rest),
            }),
            name: self.name,
        }
    }
}

/// Ceiling on models accepted from one `/config` response. Real fleets carry
/// low hundreds; a response past this is malformed or hostile, and each entry
/// becomes a cache row plus resolver work in every database.
const MAX_MODELS_PER_RESPONSE: usize = 10_000;

/// Parse one `/config` response body into enabled models.
pub fn parse_config(body: &str) -> Result<Vec<ModelInfo>, PvError> {
    let envelope: Envelope =
        serde_json::from_str(body).map_err(|e| PvError::Decode(format!("/config JSON: {e}")))?;
    if !envelope.success {
        return Err(PvError::Decode(format!(
            "/config returned success=false: {:?}",
            envelope.error
        )));
    }
    let data = envelope
        .data
        .ok_or_else(|| PvError::Decode("/config envelope has no data".into()))?;
    if data.models.len() > MAX_MODELS_PER_RESPONSE {
        return Err(PvError::Decode(format!(
            "/config advertises {} models; the ceiling is {MAX_MODELS_PER_RESPONSE}",
            data.models.len()
        )));
    }
    Ok(data
        .models
        .into_iter()
        .filter(HubModel::enabled)
        .map(HubModel::into_model_info)
        .collect())
}

/// Fetch and aggregate models from all configured HTTP endpoints. A node
/// failure is non-fatal as long as at least one node answers; duplicate
/// model names dedup (last node wins, like aphex's registry rebuild).
pub async fn fetch_models(
    http_endpoints: &[String],
    per_node_timeout: Duration,
) -> Result<Vec<ModelInfo>, PvError> {
    Ok(fetch_models_report(http_endpoints, per_node_timeout)
        .await?
        .models)
}

pub async fn fetch_models_report(
    http_endpoints: &[String],
    per_node_timeout: Duration,
) -> Result<DiscoveryReport, PvError> {
    if http_endpoints.is_empty() {
        return Err(PvError::NoEndpoints);
    }
    let client = HTTP_CLIENT
        .as_ref()
        .map_err(|e| PvError::Internal(e.clone()))?;

    // Poll nodes in bounded waves. `buffered(DISCOVERY_CONCURRENCY)` runs
    // at most 4 requests at once, each bounded by `per_node_timeout`, so a
    // full pass costs `per-node timeout * ceil(E/4)` in the worst case.
    // `buffered` preserves input order, so a duplicate name resolves to
    // the last configured node.
    let fetches = http_endpoints.iter().map(|endpoint| {
        let client = client.clone();
        async move {
            let url = format!("{}/config", endpoint.trim_end_matches('/'));
            let mut resp = client
                .get(&url)
                .timeout(per_node_timeout)
                .send()
                .await
                .map_err(|e| PvError::Transport {
                    endpoint: endpoint.clone(),
                    message: e.to_string(),
                })?;
            // A non-2xx (a proxy 502, an error page) is a node problem, not
            // "garbage JSON" — surface it as such instead of a Decode error.
            let status = resp.status();
            if !status.is_success() {
                return Err(PvError::Transport {
                    endpoint: endpoint.clone(),
                    message: format!("/config returned HTTP {status}"),
                });
            }
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(|e| PvError::Transport {
                endpoint: endpoint.clone(),
                message: e.to_string(),
            })? {
                if buf.len() + chunk.len() > MAX_CONFIG_BODY_BYTES {
                    return Err(PvError::Decode(format!(
                        "/config body from {endpoint} exceeds {MAX_CONFIG_BODY_BYTES} bytes"
                    )));
                }
                buf.extend_from_slice(&chunk);
            }
            let body = String::from_utf8_lossy(&buf);
            parse_config(&body)
        }
    });
    // Streamed fan-out: at most 4 nodes in flight, and each node's parsed
    // result folds into the aggregate as it arrives. Nothing retains every
    // endpoint's result until the pass completes. Past MAX_MODELS_TOTAL the
    // refresh fails (keeping the previous cache) instead of materializing
    // an unbounded map. `buffered` (not `buffer_unordered`) preserves
    // configured order so a duplicate name resolves to the last configured
    // node.
    use futures_util::StreamExt;
    let fetches: Vec<_> = fetches.collect();
    let mut stream = futures_util::stream::iter(fetches).buffered(DISCOVERY_CONCURRENCY);

    let mut by_name: std::collections::BTreeMap<String, ModelInfo> = Default::default();
    let mut last_err: Option<PvError> = None;
    let mut ok_nodes = 0usize;
    let mut failed_nodes = 0usize;
    while let Some(result) = stream.next().await {
        match result {
            Ok(models) => {
                ok_nodes += 1;
                for m in models {
                    by_name.insert(m.name.clone(), m);
                }
                if by_name.len() > MAX_MODELS_TOTAL {
                    return Err(PvError::Decode(format!(
                        "discovery aggregated more than {MAX_MODELS_TOTAL} models across \
                         nodes; refusing the refresh"
                    )));
                }
            }
            Err(e) => {
                failed_nodes += 1;
                last_err = Some(e);
            }
        }
    }

    if ok_nodes == 0 {
        return Err(last_err.unwrap_or(PvError::NoEndpoints));
    }
    Ok(DiscoveryReport {
        models: by_name.into_values().collect(),
        complete: failed_nodes == 0,
        ok_nodes,
        failed_nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "success": true,
      "data": {
        "models": [
          {
            "name": "snowflake-arctic-embed-l-v2.0",
            "status": "local",
            "local_framework": "onnx-runtime",
            "configuration": {
              "enabled": true,
              "backend": "onnx-runtime",
              "params": {
                "model_type": "embed",
                "target_model": "snowflake-arctic-embed-l-v2.0",
                "target_dim": 1024,
                "sequence_len": 8192
              }
            }
          },
          {
            "name": "convert-bge-to-cohere",
            "status": "local",
            "configuration": {
              "enabled": true,
              "params": {
                "source_model": "baai-bge-m3",
                "target_model": "cohere-embed-v4.0",
                "source_dim": 1024,
                "target_dim": 1536
              }
            }
          },
          {
            "name": "disabled-model",
            "status": "local",
            "configuration": { "enabled": false, "params": { "model_type": "embed" } }
          },
          {
            "name": "no-config-model",
            "status": "remote"
          }
        ]
      }
    }"#;

    #[test]
    fn parses_config_fixture() {
        let models = parse_config(FIXTURE).unwrap();
        // disabled and config-less models are filtered out
        assert_eq!(models.len(), 2);

        let embed = models
            .iter()
            .find(|m| m.name == "snowflake-arctic-embed-l-v2.0")
            .unwrap();
        assert_eq!(embed.model_type, "embed");
        assert_eq!(embed.target_dim, Some(1024));
        assert_eq!(embed.sequence_len, Some(8192));

        // model_type inferred from source+target presence
        let convert = models
            .iter()
            .find(|m| m.name == "convert-bge-to-cohere")
            .unwrap();
        assert_eq!(convert.model_type, "convert");
        assert_eq!(convert.source_model.as_deref(), Some("baai-bge-m3"));
        assert_eq!(convert.target_model.as_deref(), Some("cohere-embed-v4.0"));
        assert_eq!(convert.target_dim, Some(1536));
    }

    /// Serve one canned HTTP response on a loopback port, in a background
    /// thread. Returns the endpoint base URL.
    fn serve_once_with_status(status_line: &'static str, body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn serve_once(body: &'static str) -> String {
        serve_once_with_status("200 OK", body)
    }

    const FIXTURE_B: &str = r#"{
      "success": true,
      "data": {
        "models": [
          {
            "name": "snowflake-arctic-embed-l-v2.0",
            "configuration": {
              "enabled": true,
              "params": { "model_type": "embed",
                          "target_model": "snowflake-arctic-embed-l-v2.0",
                          "target_dim": 2048 }
            }
          },
          {
            "name": "only-on-node-b",
            "configuration": {
              "enabled": true,
              "params": { "model_type": "embed", "target_model": "only-on-node-b",
                          "target_dim": 8 }
            }
          }
        ]
      }
    }"#;

    /// Two live nodes polled concurrently: models aggregate across nodes and
    /// a duplicate name resolves to the last *configured* node's version.
    #[test]
    fn fetch_aggregates_across_nodes_last_configured_wins() {
        let endpoints = vec![serve_once(FIXTURE), serve_once(FIXTURE_B)];
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_models_report(&endpoints, Duration::from_secs(5)))
            .unwrap();
        assert!(report.complete);
        assert_eq!((report.ok_nodes, report.failed_nodes), (2, 0));
        assert_eq!(report.models.len(), 3, "union of both nodes' models");
        let dup = report
            .models
            .iter()
            .find(|m| m.name == "snowflake-arctic-embed-l-v2.0")
            .unwrap();
        assert_eq!(
            dup.target_dim,
            Some(2048),
            "the last configured node's version wins"
        );
    }

    /// One node down: the refresh still succeeds but reports itself partial
    /// (which gates cache pruning).
    #[test]
    fn fetch_with_a_dead_node_is_partial() {
        // Port 1 on loopback: nothing listens there, connection refused fast.
        let endpoints = vec![serve_once(FIXTURE), "http://127.0.0.1:1".to_string()];
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_models_report(&endpoints, Duration::from_secs(5)))
            .unwrap();
        assert!(!report.complete, "a failed node makes the refresh partial");
        assert_eq!((report.ok_nodes, report.failed_nodes), (1, 1));
        assert_eq!(report.models.len(), 2);
    }

    /// All nodes down: the last transport error surfaces.
    #[test]
    fn fetch_with_all_nodes_dead_errors() {
        let endpoints = vec!["http://127.0.0.1:1".to_string()];
        let res = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_models_report(&endpoints, Duration::from_secs(5)));
        assert!(matches!(res, Err(PvError::Transport { .. })));
    }

    /// A non-2xx answer (proxy 502, error page) is a node/transport problem
    /// and makes the refresh partial — not a confusing Decode error.
    #[test]
    fn http_error_status_is_transport_and_partial() {
        let endpoints = vec![
            serve_once(FIXTURE),
            serve_once_with_status("502 Bad Gateway", "upstream down"),
        ];
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_models_report(&endpoints, Duration::from_secs(5)))
            .unwrap();
        assert!(!report.complete, "an HTTP-erroring node is a failed node");
        assert_eq!((report.ok_nodes, report.failed_nodes), (1, 1));

        let only_bad = vec![serve_once_with_status("500 Internal Server Error", "boom")];
        let res = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fetch_models_report(&only_bad, Duration::from_secs(5)));
        match res {
            Err(PvError::Transport { message, .. }) => {
                assert!(
                    message.contains("HTTP 500"),
                    "surfaces the status: {message}"
                )
            }
            other => panic!("expected Transport with the HTTP status, got {other:?}"),
        }
    }

    #[test]
    fn rejects_error_envelope() {
        let body = r#"{"success": false, "error": {"message": "boom"}}"#;
        assert!(matches!(parse_config(body), Err(PvError::Decode(_))));
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(parse_config("not json"), Err(PvError::Decode(_))));
    }
}
