//! `postvec login` / `logout` / `whoami`.
//!
//! `login` accepts the key from a hidden interactive prompt, `--api-key-file`,
//! or non-TTY stdin — never from argv, which is world-observable. It proves
//! the key against the authenticated index before storing anything, and it
//! states plainly that a `uv_` key can also authorize billable API requests.
//! `whoami` succeeds anonymously and reports exactly where a credential
//! search looked.

use crate::cli::{Cli, LoginArgs, WhoamiArgs};
use crate::config::owned;
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use crate::registry::auth;
use crate::registry::client::RegistryClient;
use crate::registry::urls;
use serde::Serialize;

pub async fn login(cli: &Cli, args: LoginArgs, output: &Output) -> Result<Exit> {
    let key = match &args.api_key_file {
        Some(path) => {
            let content = owned::read_regular_file(path)?
                .ok_or_else(|| CliError::usage(format!("{} does not exist", path.display())))?;
            auth::validate_key_shape(&content)?
        }
        None => {
            if !crate::proc::is_stdin_tty() && output.is_json() {
                return Err(CliError::usage(
                    "login --format json needs --api-key-file (no interactive prompt)",
                ));
            }
            if crate::proc::is_stdin_tty() {
                output.progress(
                    "Create a dedicated key at https://univec.ai/dashboard/api-keys and \
                     paste it.\nThe key can authorize UniVec API requests; set its spending \
                     limit to $0 if it is only for model downloads.",
                );
            }
            let raw = crate::proc::read_hidden_line("API key: ")
                .map_err(|e| CliError::apply(format!("cannot read the key: {e}")))?;
            auth::validate_key_shape(&raw)?
        }
    };

    // Prove the key before persisting it: a typo'd or revoked key stored now
    // would fail every later pull with a less obvious message.
    let target = urls::authenticated_index_url();
    if target.overridden {
        output.note("registry index override is active (testing only)");
    }
    let client = RegistryClient::new(cli.timeout, target.overridden)?;
    let index = client.fetch_index(&target.url, Some(&key)).await?;

    let path = auth::save(&key)?;
    output.progress(&format!(
        "Signed in — authenticated catalogue, {} models.",
        index.models.len()
    ));
    output.note(&format!("credential stored at {} (0600)", path.display()));

    #[derive(Serialize)]
    struct LoginDocument {
        schema_version: u32,
        command: &'static str,
        store: String,
        key_prefix: String,
        channel: String,
        models: usize,
    }
    if output.is_json() {
        output.show_document(
            &LoginDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "login",
                store: path.display().to_string(),
                key_prefix: auth::masked_key(&key),
                channel: index.channel,
                models: index.models.len(),
            },
            "",
        )?;
    }
    Ok(Exit::Success)
}

pub async fn logout(output: &Output) -> Result<Exit> {
    let removed = auth::remove()?;
    let store = auth::store_path()?;
    output.progress(&format!(
        "{} — model commands are anonymous (public catalogue)",
        if removed {
            "Signed out"
        } else {
            "No stored credential"
        }
    ));
    #[derive(Serialize)]
    struct LogoutDocument {
        schema_version: u32,
        command: &'static str,
        store: String,
        removed: bool,
    }
    if output.is_json() {
        output.show_document(
            &LogoutDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "logout",
                store: store.display().to_string(),
                removed,
            },
            "",
        )?;
    }
    Ok(Exit::Success)
}

#[derive(Serialize)]
struct WhoamiDocument {
    schema_version: u32,
    command: &'static str,
    signed_in: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_source: Option<String>,
    searched: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    models: Option<usize>,
    /// The entitlements the served index's `viewer` block reported — id,
    /// source and optional expiry, nothing else. Present only when an
    /// authenticated response carried a viewer; an anonymous `whoami` never
    /// invents one.
    #[serde(skip_serializing_if = "Option::is_none")]
    entitlements: Option<Vec<EntitlementReport>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct EntitlementReport {
    id: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

/// The human rendering of a reported entitlement list. Pure, so the format
/// is pinned by unit tests: id, source, optional expiry — never billing or
/// subscription language.
fn describe_entitlements(entitlements: &[EntitlementReport]) -> String {
    if entitlements.is_empty() {
        return "Entitlements: none.\n".to_string();
    }
    let described: Vec<String> = entitlements
        .iter()
        .map(|entitlement| match &entitlement.expires_at {
            Some(expires_at) => format!(
                "{} ({}, expires {expires_at})",
                entitlement.id, entitlement.source
            ),
            None => format!("{} ({})", entitlement.id, entitlement.source),
        })
        .collect();
    format!("Entitlements: {}.\n", described.join(", "))
}

pub async fn whoami(cli: &Cli, args: WhoamiArgs, output: &Output) -> Result<Exit> {
    let searched = auth::search_description(args.api_key_file.as_deref());
    let credential = auth::resolve(args.api_key_file.as_deref())?;

    let mut document = WhoamiDocument {
        schema_version: crate::checks::SCHEMA_VERSION,
        command: "whoami",
        signed_in: credential.is_some(),
        key_prefix: credential.as_ref().map(|c| c.masked()),
        credential_source: credential.as_ref().map(|c| c.source.to_string()),
        searched: searched.clone(),
        channel: None,
        models: None,
        entitlements: None,
        error: None,
    };

    let exit = match &credential {
        Some(credential) => {
            let target = urls::authenticated_index_url();
            let client = RegistryClient::new(cli.timeout, target.overridden)?;
            match client.fetch_index(&target.url, Some(&credential.key)).await {
                Ok(index) => {
                    document.channel = Some(index.channel.clone());
                    document.models = Some(index.models.len());
                    // The viewer block rides the index request the client
                    // already makes: no second endpoint. Absent on a
                    // pre-entitlement gateway; reported when present.
                    document.entitlements = index.viewer.as_ref().map(|viewer| {
                        viewer
                            .entitlements
                            .iter()
                            .map(|entitlement| EntitlementReport {
                                id: entitlement.id.clone(),
                                source: entitlement.source.clone(),
                                expires_at: entitlement.expires_at.clone(),
                            })
                            .collect()
                    });
                    Exit::Success
                }
                Err(e) => {
                    // A present-but-failing credential is exactly what whoami
                    // exists to surface; it is a finding, not a crash.
                    document.error = Some(e.to_string());
                    Exit::Failure
                }
            }
        }
        None => {
            // Anonymous: describe the public channel, best effort.
            let target = urls::public_index_url();
            if let Ok(client) = RegistryClient::new(cli.timeout, target.overridden) {
                if let Ok(index) = client.fetch_index(&target.url, None).await {
                    document.channel = Some(index.channel.clone());
                    document.models = Some(index.models.len());
                }
            }
            Exit::Success
        }
    };

    let mut human = String::new();
    match (&credential, &document.error) {
        (Some(credential), None) => {
            human.push_str(&format!(
                "Signed in as {} (from {}) — {} catalogue, {} models.\n",
                credential.masked(),
                credential.source,
                document.channel.as_deref().unwrap_or("authenticated"),
                document
                    .models
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into())
            ));
            if let Some(entitlements) = &document.entitlements {
                human.push_str(&describe_entitlements(entitlements));
            }
        }
        (Some(credential), Some(error)) => {
            human.push_str(&format!(
                "Credential {} (from {}) FAILS authentication: {error}\n",
                credential.masked(),
                credential.source
            ));
        }
        (None, _) => {
            human.push_str(&format!(
                "Not signed in — public catalogue{}.\n",
                document
                    .models
                    .map(|n| format!(", {n} entries"))
                    .unwrap_or_else(|| " (unreachable right now)".into())
            ));
        }
    }
    human.push_str(&format!("Searched: {searched}.\n"));

    output.show_document(&document, &human)?;
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(id: &str, source: &str, expires_at: Option<&str>) -> EntitlementReport {
        EntitlementReport {
            id: id.to_string(),
            source: source.to_string(),
            expires_at: expires_at.map(str::to_string),
        }
    }

    /// `whoami` reports each entitlement's id, source and optional
    /// expiry. No billing, subscription or action-required language, and
    /// an honest "none" when the viewer is empty.
    #[test]
    fn entitlements_are_described_with_id_source_and_expiry() {
        assert_eq!(
            describe_entitlements(&[report("postvec-catalog", "default", None)]),
            "Entitlements: postvec-catalog (default).\n"
        );
        assert_eq!(
            describe_entitlements(&[report(
                "postvec-catalog",
                "grant",
                Some("2027-01-01T00:00:00Z")
            ),]),
            "Entitlements: postvec-catalog (grant, expires 2027-01-01T00:00:00Z).\n"
        );
        assert_eq!(describe_entitlements(&[]), "Entitlements: none.\n");
    }

    /// The JSON document mirrors the human report: id/source/expiry only,
    /// with `expires_at` omitted when absent — and the whole field omitted
    /// when no viewer was served (an anonymous `whoami`, or a
    /// pre-entitlement gateway, never invents one).
    #[test]
    fn the_whoami_document_serializes_entitlements_faithfully() {
        let mut document = WhoamiDocument {
            schema_version: crate::checks::SCHEMA_VERSION,
            command: "whoami",
            signed_in: false,
            key_prefix: None,
            credential_source: None,
            searched: "nothing".to_string(),
            channel: None,
            models: None,
            entitlements: None,
            error: None,
        };
        let anonymous = serde_json::to_value(&document).unwrap();
        assert!(anonymous.get("entitlements").is_none());

        document.entitlements = Some(vec![report("postvec-catalog", "default", None)]);
        let signed_in = serde_json::to_value(&document).unwrap();
        assert_eq!(
            signed_in["entitlements"],
            serde_json::json!([{ "id": "postvec-catalog", "source": "default" }])
        );
    }
}
