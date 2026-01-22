//! Read-only probing of the inference side.
//!
//! Two hard rules:
//!
//! - **No inference is ever executed.** A real embedding needs a model choice,
//!   costs real compute, could expose text, and can legitimately fail for
//!   licensing or routing reasons. Transport reachability plus a valid
//!   `/config` inventory plus the worker's own evidence is the contract.
//! - **No engine is ever instantiated.** Loading ONNX Runtime and models in a
//!   diagnostic tool would consume production memory and could collide with the
//!   launcher's listeners.

pub mod embedded;
pub mod remote;

use crate::cli::TlsPolicy;
use crate::error::{redact, CliError, Result};
use crate::facts::{ConfigInventory, InventoryModel};
use serde::Deserialize;
use std::time::Duration;

/// Upper bound on a `/config` body, matching the extension's own cap
/// (`postvec/src/client/discovery.rs`). Anything larger is a misconfigured
/// endpoint, not an inventory.
pub const MAX_CONFIG_BODY_BYTES: usize = 64 * 1024 * 1024;

/// The `{success, data: {...}}` envelope every ninference HTTP route uses.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    data: Option<EnvelopeData>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeData {
    #[serde(default)]
    models: Vec<HubModel>,
}

/// The subset of a hub model the CLI needs, tolerant of everything else.
#[derive(Debug, Deserialize)]
struct HubModel {
    name: String,
    configuration: Option<HubConfiguration>,
}

#[derive(Debug, Deserialize, Default)]
struct HubConfiguration {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    params: HubParams,
}

#[derive(Debug, Deserialize, Default)]
struct HubParams {
    model_type: Option<String>,
    source_model: Option<String>,
    target_model: Option<String>,
}

impl HubModel {
    /// Mirrors the extension's classification (`client/discovery.rs`): an
    /// explicit `model_type` wins, otherwise it is inferred from which of
    /// source/target is present.
    fn model_type(&self) -> Option<String> {
        let params = &self.configuration.as_ref()?.params;
        if let Some(declared) = &params.model_type {
            return Some(match declared.as_str() {
                "convert" | "embed" | "embed-bridge" | "convert-bridge" => declared.clone(),
                _ => "legacy".to_string(),
            });
        }
        Some(
            match (params.source_model.is_some(), params.target_model.is_some()) {
                (true, true) => "convert",
                (false, true) => "embed",
                _ => "legacy",
            }
            .to_string(),
        )
    }
}

/// Parse a `/config` response body into an inventory.
///
/// Validating the envelope rather than accepting any HTTP 200 is the point:
/// a reverse proxy, a login page or an unrelated service on the port all
/// answer 200 happily.
pub fn parse_config_body(body: &str) -> Result<ConfigInventory> {
    let envelope: Envelope = serde_json::from_str(body).map_err(|e| {
        CliError::precondition(format!(
            "the response is not a ninference /config envelope: {e}"
        ))
    })?;
    if !envelope.success {
        let detail = envelope
            .error
            .map(|e| redact(&e.to_string()))
            .unwrap_or_else(|| "no error detail".to_string());
        return Err(CliError::precondition(format!(
            "the node reported failure for /config: {detail}"
        )));
    }
    let data = envelope.data.ok_or_else(|| {
        CliError::precondition("the /config envelope has no data object".to_string())
    })?;
    let mut models: Vec<InventoryModel> = data
        .models
        .into_iter()
        .map(|model| InventoryModel {
            enabled: model.configuration.as_ref().is_some_and(|c| c.enabled),
            model_type: model.model_type(),
            name: model.name,
        })
        .collect();
    models.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ConfigInventory { models })
}

/// HTTP clients for discovery probes.
///
/// Two of them, because the honest answer to "is this endpoint healthy?"
/// includes whether its certificate verifies. The extension accepts invalid
/// certificates (mesh-only trust); the CLI reproduces that behaviour but
/// reports when it had to.
pub struct HttpProbes {
    verifying: reqwest::Client,
    accepting: Option<reqwest::Client>,
}

impl HttpProbes {
    pub fn new(policy: TlsPolicy, timeout: Duration) -> Result<Self> {
        let build = |accept_invalid: bool| {
            reqwest::Client::builder()
                .danger_accept_invalid_certs(accept_invalid)
                .timeout(timeout)
                .connect_timeout(timeout)
                // A discovery endpoint that redirects is a misconfiguration, and
                // following one could send the request somewhere unintended.
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("postvec-cli/", env!("CARGO_PKG_VERSION")))
                .build()
        };
        Ok(Self {
            verifying: build(false)
                .map_err(|e| CliError::internal(format!("cannot build HTTP client: {e}")))?,
            accepting: match policy {
                TlsPolicy::Strict => None,
                TlsPolicy::ExtensionCompatible => {
                    Some(build(true).map_err(|e| {
                        CliError::internal(format!("cannot build HTTP client: {e}"))
                    })?)
                }
            },
        })
    }

    /// GET a URL, capped and bounded.
    ///
    /// Returns the status, the body, and whether the certificate verified.
    /// `None` for the verification flag means the question does not apply
    /// (plain HTTP).
    pub async fn get(&self, url: &str, https: bool) -> ProbeOutcome {
        match fetch(&self.verifying, url).await {
            Ok((status, body)) => ProbeOutcome::Answered {
                status,
                body,
                tls_verified: https.then_some(true),
            },
            Err(verify_error) => {
                let Some(accepting) = &self.accepting else {
                    return ProbeOutcome::Failed {
                        detail: verify_error,
                    };
                };
                // Retry only over the same scheme; an HTTPS failure is never
                // retried as HTTP.
                match fetch(accepting, url).await {
                    Ok((status, body)) => ProbeOutcome::Answered {
                        status,
                        body,
                        tls_verified: https.then_some(false),
                    },
                    Err(detail) => ProbeOutcome::Failed { detail },
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Answered {
        status: u16,
        body: String,
        /// `Some(false)`: the certificate did not verify and was accepted only
        /// because the extension accepts it too.
        tls_verified: Option<bool>,
    },
    Failed {
        detail: String,
    },
}

async fn fetch(client: &reqwest::Client, url: &str) -> std::result::Result<(u16, String), String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| redact(&format!("{e}")))?;
    let status = response.status().as_u16();
    // Streamed with a cap: a `Content-Length` header is a claim, not a limit.
    let mut body = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|e| redact(&format!("reading body: {e}")))?
    {
        if body.len() + chunk.len() > MAX_CONFIG_BODY_BYTES {
            return Err(format!(
                "response body exceeds the {} MiB cap",
                MAX_CONFIG_BODY_BYTES / (1024 * 1024)
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_realistic_config_envelope() {
        let body = r#"{
            "success": true,
            "data": {"models": [
                {"name": "baai-bge-m3", "status": "local",
                 "configuration": {"enabled": true,
                     "params": {"model_type": "embed", "target_model": "baai-bge-m3",
                                "target_dim": 1024}}},
                {"name": "convert-bge-to-cohere",
                 "configuration": {"enabled": true,
                     "params": {"source_model": "baai-bge-m3",
                                "target_model": "cohere-embed-v4.0"}}},
                {"name": "disabled-thing", "configuration": {"enabled": false, "params": {}}}
            ]}
        }"#;
        let inventory = parse_config_body(body).unwrap();
        assert_eq!(inventory.models.len(), 3);
        assert_eq!(
            inventory.enabled_names(),
            [
                "baai-bge-m3".to_string(),
                "convert-bge-to-cohere".to_string()
            ]
            .into()
        );
        // model_type is inferred exactly as the extension infers it.
        let converter = inventory
            .models
            .iter()
            .find(|m| m.name == "convert-bge-to-cohere")
            .unwrap();
        assert_eq!(converter.model_type.as_deref(), Some("convert"));
    }

    #[test]
    fn tolerates_unknown_fields_and_missing_configuration() {
        let body = r#"{"success":true,"data":{"models":[
            {"name":"m","brand_new_field":1},
            {"name":"n","configuration":{"enabled":true,"params":{"model_type":"weird"},
                                         "future":true},"extra":[1,2]}
        ]},"meta":{"x":1}}"#;
        let inventory = parse_config_body(body).unwrap();
        assert_eq!(inventory.models.len(), 2);
        assert!(!inventory.models[0].enabled);
        assert_eq!(inventory.models[0].model_type, None);
        // An unrecognized declared type degrades to `legacy`, as in the
        // extension.
        assert_eq!(inventory.models[1].model_type.as_deref(), Some("legacy"));
    }

    #[test]
    fn rejects_anything_that_is_not_the_envelope() {
        for body in [
            "",
            "not json",
            "<html>login</html>",
            r#"{"models":[]}"#,
            r#"{"success":true}"#,
        ] {
            assert!(
                parse_config_body(body).is_err(),
                "{body:?} must not be accepted as an inventory"
            );
        }
    }

    #[test]
    fn reports_a_failure_envelope_without_leaking_secrets() {
        let body = r#"{"success":false,"error":{"message":"nope","token":"s3cret"}}"#;
        let err = parse_config_body(body).unwrap_err();
        assert!(err.to_string().contains("reported failure"));
        assert!(!err.to_string().contains("s3cret"));
    }

    #[test]
    fn an_empty_inventory_is_valid() {
        let inventory = parse_config_body(r#"{"success":true,"data":{"models":[]}}"#).unwrap();
        assert!(inventory.models.is_empty());
    }

    #[test]
    fn strict_tls_has_no_accepting_client() {
        let strict = HttpProbes::new(TlsPolicy::Strict, Duration::from_secs(1)).unwrap();
        assert!(strict.accepting.is_none());
        let compatible =
            HttpProbes::new(TlsPolicy::ExtensionCompatible, Duration::from_secs(1)).unwrap();
        assert!(compatible.accepting.is_some());
    }
}
