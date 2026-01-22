//! Client for the embedded engine's loopback admin endpoints
//! (`POST /admin/load`, `POST /admin/unload`).
//!
//! The listener is the launcher's `postvec.embedded_http_listen` (default
//! `127.0.0.1:33434`), loopback-only and unauthenticated: local OS users are
//! trusted in v1, which is exactly the trust the loopback gRPC listener
//! already extends. The wire shape mirrors the `/config` envelope.

use crate::error::{CliError, Result};
use serde::Deserialize;
use std::time::Duration;

/// Per-model outcome as the endpoint reports it.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct AdminOutcome {
    pub model: String,
    /// `loaded` / `already-loaded` / `unloaded` / `not-loaded` / `error`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AdminOutcome {
    pub fn is_error(&self) -> bool {
        self.status == "error"
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    success: bool,
    #[serde(default)]
    data: Option<Data>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Data {
    results: Vec<AdminOutcome>,
}

async fn post(
    listen: &str,
    endpoint: &str,
    models: &[String],
    timeout: Duration,
) -> Result<Vec<AdminOutcome>> {
    let url = format!("http://{listen}/admin/{endpoint}");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CliError::internal(format!("cannot build HTTP client: {e}")))?;
    let response = client
        .post(&url)
        .json(&serde_json::json!({ "models": models }))
        .send()
        .await
        .map_err(|e| {
            CliError::precondition(format!("cannot reach the embedded engine at {listen}: {e}"))
                .with_fix(
                    "the engine may still be starting (models load before the listener \
                     serves), or the cluster may be stopped; on-disk models load at the \
                     next engine start either way",
                )
        })?;
    let status = response.status();
    let body: Envelope = response.json().await.map_err(|e| {
        CliError::precondition(format!(
            "embedded engine {endpoint} answered malformed JSON: {e}"
        ))
    })?;
    if !status.is_success() || !body.success {
        return Err(CliError::precondition(format!(
            "embedded engine refused {endpoint}: {}",
            body.error.unwrap_or_else(|| format!("HTTP {status}"))
        )));
    }
    Ok(body.data.map(|d| d.results).unwrap_or_default())
}

/// Statuses that prove a requested unload really happened.
pub const UNLOADED: &[&str] = &["unloaded", "not-loaded"];
/// Statuses that prove a **fresh** load happened. `already-loaded` is
/// deliberately absent: after a confirmed unload it would mean the engine
/// still holds the previous bytes, which is precisely the state an in-place
/// replacement must never commit on.
pub const LOADED_FRESH: &[&str] = &["loaded"];
/// For a model that was not resident to begin with, either answer is proof.
pub const LOADED_ANY: &[&str] = &["loaded", "already-loaded"];

/// Ask the launcher-hosted engine to load `models` (directory names).
pub async fn load(listen: &str, models: &[String], timeout: Duration) -> Result<Vec<AdminOutcome>> {
    post(listen, "load", models, timeout).await
}

/// Every requested name must come back **exactly once**, with one of the
/// statuses that name accepts. Anything else — an `error` outcome, a missing,
/// duplicate or unrequested result — fails the caller before it mutates
/// anything.
///
/// The accepted set is per name because one request can mix intents: a
/// replacement must report a genuine `loaded`, while a model that was never
/// resident may equally report `already-loaded`.
pub fn verify_outcomes(
    expected: &[(String, &'static [&'static str])],
    outcomes: &[AdminOutcome],
) -> std::result::Result<(), String> {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for outcome in outcomes {
        let Some((_, accepted)) = expected.iter().find(|(name, _)| name == &outcome.model) else {
            return Err(format!(
                "the engine answered for {:?}, which was not requested",
                outcome.model
            ));
        };
        *counts.entry(outcome.model.as_str()).or_default() += 1;
        if !accepted.contains(&outcome.status.as_str()) {
            return Err(format!(
                "{}: reported {:?}, expected one of [{}]{}",
                outcome.model,
                outcome.status,
                accepted.join(", "),
                outcome
                    .error
                    .as_deref()
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default()
            ));
        }
    }
    for (name, _) in expected {
        match counts.get(name.as_str()) {
            Some(1) => {}
            Some(n) => return Err(format!("{name}: {n} results for one request")),
            None => return Err(format!("{name}: the engine returned no result")),
        }
    }
    Ok(())
}

/// Ask the launcher-hosted engine to unload `models`.
pub async fn unload(
    listen: &str,
    models: &[String],
    timeout: Duration,
) -> Result<Vec<AdminOutcome>> {
    post(listen, "unload", models, timeout).await
}

/// The engine's currently loaded inventory via `GET /config`; `None` when the
/// listener is unreachable (engine down or still loading).
pub async fn loaded_inventory(
    listen: &str,
    timeout: Duration,
) -> Option<crate::facts::ConfigInventory> {
    let url = format!("http://{listen}/config");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let body = client.get(&url).send().await.ok()?.text().await.ok()?;
    crate::engine::parse_config_body(&body).ok()
}
