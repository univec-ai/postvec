//! `postvec model show`.
//!
//! Offline for installed models (identity comes from the receipt and the
//! descriptor); `--verify` hashes every installed file against the receipt —
//! explicit, because a multi-gigabyte scan is not a metadata operation.

use crate::cli::{Cli, ModelShowArgs};
use crate::commands::model::{admin, human_bytes, resolve_target, ModelTarget};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use serde::Serialize;

#[derive(Serialize)]
struct ShowDocument {
    schema_version: u32,
    command: &'static str,
    target: String,
    name: String,
    backend: String,
    owner: String,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_dim: Option<u32>,
    disk_bytes: u64,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    loaded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<crate::registry::receipt::Receipt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dependencies: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restrictions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<VerifyOutcome>,
}

#[derive(Serialize)]
struct VerifyOutcome {
    verified: bool,
    problems: Vec<String>,
}

pub async fn run(cli: &Cli, args: ModelShowArgs, output: &Output) -> Result<Exit> {
    crate::registry::index::valid_model_name(&args.name).map_err(CliError::usage)?;
    let target = resolve_target(cli, args.path.as_deref(), output).await?;
    let root = match target.root() {
        Some(root) => root,
        None => {
            return Err(CliError::precondition(
                "the selected cluster uses remote inference; there is no local install to show",
            )
            .with_fix("pass --path <DIR> for a local engine root"))
        }
    };
    let _shared = root.lock_shared()?;

    let inventory = root.installed()?;
    let Some(model) = inventory.iter().find(|m| m.dir_name == args.name) else {
        return Err(CliError::precondition(format!(
            "{} is not installed under {}",
            args.name,
            root.models_dir().display()
        ))
        .with_fix(
            "run `postvec model ls` for the installed set, or `postvec model ls --available` \
             for the catalogue",
        ));
    };

    let owner = model.ownership(cli.timeout).await;
    let loaded = match &target {
        ModelTarget::Embedded { settings, .. } => {
            admin::loaded_inventory(&settings.embedded_http_listen(), cli.timeout)
                .await
                .map(|inv| {
                    inv.enabled_names()
                        .contains(model.descriptor_name.as_deref().unwrap_or(&model.dir_name))
                })
        }
        _ => None,
    };

    // Bridge descriptors carry a licence blocklist the engine enforces;
    // report it, never reimplement it.
    let restrictions = read_restrictions(&model.path);

    let verify = if args.verify {
        match &model.receipt {
            Some(receipt) => {
                output.note("hashing every installed file against the receipt…");
                let problems = receipt.verify_files(&model.path);
                Some(VerifyOutcome {
                    verified: problems.is_empty(),
                    problems,
                })
            }
            None => Some(VerifyOutcome {
                verified: false,
                problems: vec!["no receipt: not a CLI install, nothing to verify against".into()],
            }),
        }
    } else {
        None
    };

    let document = ShowDocument {
        schema_version: crate::checks::SCHEMA_VERSION,
        command: "model show",
        target: target.label(),
        name: model.dir_name.clone(),
        backend: model.backend.clone(),
        owner: owner.describe().to_string(),
        enabled: model.enabled,
        model_type: model.model_type.clone(),
        target_dim: model.target_dim,
        disk_bytes: model.disk_bytes,
        path: model.path.display().to_string(),
        loaded,
        receipt: model.receipt.clone(),
        receipt_error: model.receipt_error.clone(),
        dependencies: model.dependencies.clone(),
        restrictions,
        verify,
    };

    let mut human = String::new();
    human.push_str(&output.style.bold(&document.name));
    human.push('\n');
    kv(&mut human, output, "path", &document.path);
    kv(&mut human, output, "backend", &document.backend);
    kv(&mut human, output, "owner", &document.owner);
    kv(
        &mut human,
        output,
        "type/dim",
        &format!(
            "{} / {}",
            document.model_type.as_deref().unwrap_or("-"),
            document
                .target_dim
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-".into())
        ),
    );
    // Same word `ls` uses, so the two reports do not describe one state with
    // two vocabularies.
    kv_toned(
        &mut human,
        output,
        "state",
        if document.enabled {
            "activated (loads at every engine start)"
        } else {
            "deactivated (`postvec model activate` turns it on)"
        },
        if document.enabled {
            crate::output::Tone::Plain
        } else {
            crate::output::Tone::Dim
        },
    );
    kv(
        &mut human,
        output,
        "disk",
        &human_bytes(document.disk_bytes),
    );
    if let Some(loaded) = document.loaded {
        let (text, tone) = match (loaded, document.enabled) {
            (true, true) => ("loaded", crate::output::Tone::Success),
            // Deactivated but still resident: an unload that did not finish.
            (true, false) => (
                "loaded despite being deactivated — rerun `postvec model deactivate`",
                crate::output::Tone::Fail,
            ),
            (false, true) => ("not loaded", crate::output::Tone::Warn),
            (false, false) => ("not loaded (deactivated)", crate::output::Tone::Dim),
        };
        kv_toned(&mut human, output, "engine", text, tone);
    }
    if !document.dependencies.is_empty() {
        kv(
            &mut human,
            output,
            "depends",
            &document.dependencies.join(", "),
        );
    }
    if !document.restrictions.is_empty() {
        kv(
            &mut human,
            output,
            "restricted",
            &document.restrictions.join(", "),
        );
    }
    if let Some(receipt) = &document.receipt {
        kv(
            &mut human,
            output,
            "revision",
            &receipt.revision().to_string(),
        );
        match &receipt.identity {
            Some(identity) => {
                if let (Some(source), Some(target)) =
                    (&identity.source_model, &identity.target_model)
                {
                    kv(&mut human, output, "space", &format!("{source} → {target}"));
                }
            }
            // Not "no spaces declared": this receipt predates the block.
            None => kv(
                &mut human,
                output,
                "space",
                "not recorded by the CLI that installed this",
            ),
        }
        kv(
            &mut human,
            output,
            "registry",
            &format!(
                "{} ({}), archive {} ({}), installed {} by postvec-cli {}",
                receipt.access,
                receipt.license.as_deref().unwrap_or("licence unlisted"),
                receipt.archive_digest,
                human_bytes(receipt.archive_size),
                receipt.installed_at,
                receipt.cli_version,
            ),
        );
        for line in terms_lines(receipt) {
            human.push_str(&line);
            human.push('\n');
        }
        if let Some(source) = &receipt.source {
            kv(&mut human, output, "upstream", source);
        }
        if let Some(host) = &receipt.source_host {
            kv(&mut human, output, "source", host);
        }
        if !receipt.postvec_requires.is_empty() {
            kv(
                &mut human,
                output,
                "companions",
                &receipt.postvec_requires.join(", "),
            );
        }
    }
    if let Some(error) = &document.receipt_error {
        kv_toned(
            &mut human,
            output,
            "receipt",
            error,
            crate::output::Tone::Fail,
        );
    }
    if let Some(verify) = &document.verify {
        if verify.verified {
            kv_toned(
                &mut human,
                output,
                "verify",
                "every file matches the receipt",
                crate::output::Tone::Success,
            );
        } else {
            kv_toned(
                &mut human,
                output,
                "verify",
                "FAILED",
                crate::output::Tone::Fail,
            );
            for problem in &verify.problems {
                human.push_str(&format!("    - {problem}\n"));
            }
        }
    }

    let failed = document
        .verify
        .as_ref()
        .is_some_and(|verify| !verify.verified);
    output.show_document(&document, &human)?;
    Ok(if failed { Exit::Failure } else { Exit::Success })
}

const KV_KEY_WIDTH: usize = 12;

fn kv(out: &mut String, output: &Output, key: &str, value: &str) {
    kv_toned(out, output, key, value, crate::output::Tone::Plain);
}

fn kv_toned(out: &mut String, output: &Output, key: &str, value: &str, tone: crate::output::Tone) {
    let label = format!("  {key:<KV_KEY_WIDTH$}");
    out.push_str(&output.style.dim(&label));
    out.push(' ');
    out.push_str(&output.style.tone(tone, value));
    out.push('\n');
}

/// The versioned-terms lines of the human report: the exact document
/// the receipt recorded, and — when a notice was acknowledged — when and
/// through which mechanism. "Locally acknowledged" is the whole claim: the
/// receipt proves this host was shown the document, never an organization
/// acceptance or assent. (JSON exposes the same fields through the
/// serialized receipt.)
fn terms_lines(receipt: &crate::registry::receipt::Receipt) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(version) = &receipt.license_version {
        lines.push(format!(
            "  terms:     {} {version}",
            receipt.license.as_deref().unwrap_or("licence unlisted")
        ));
    }
    if let Some(url) = &receipt.license_url {
        lines.push(format!("             {url}"));
    }
    if let Some(accepted_at) = &receipt.license_accepted_at {
        lines.push(format!(
            "             locally acknowledged {accepted_at}{}",
            receipt
                .license_acceptance_method
                .as_deref()
                .map(|method| format!(" ({method})"))
                .unwrap_or_default()
        ));
    }
    lines
}

/// `params.restrictions.target_models` from the raw descriptor (bridge
/// executors), tolerated as absent everywhere else.
fn read_restrictions(model_dir: &std::path::Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(model_dir.join("ninference.hub.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Vec::new();
    };
    value
        .get("params")
        .and_then(|p| p.get("restrictions"))
        .and_then(|r| r.get("target_models"))
        .and_then(|t| t.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::receipt::Receipt;

    fn receipt(value: serde_json::Value) -> Receipt {
        serde_json::from_value(value).unwrap()
    }

    fn base() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "name": "m",
            "backend": "onnx-runtime",
            "model_type": "embed",
            "access": "private",
            "license": "univec-commercial",
            "archive_digest": format!("sha256:{}", "ab".repeat(32)),
            "archive_size": 10,
            "dependencies": [],
            "postvec_requires": [],
            "registry_schema_version": 1,
            "installed_at": "2026-08-10T12:00:00Z",
            "cli_version": "0.1.0",
            "files": []
        })
    }

    /// M2: `model show` reports the exact recorded document and the local
    /// acknowledgement — as "locally acknowledged", never as acceptance or
    /// assent — and stays silent for a pre-M2 receipt.
    #[test]
    fn terms_lines_report_the_recorded_document_and_acknowledgement() {
        assert!(terms_lines(&receipt(base())).is_empty());

        let mut value = base();
        value["license_version"] = serde_json::json!("2026-08-09");
        value["license_url"] =
            serde_json::json!("https://univec.ai/legal/models/univec-commercial/2026-08-09");
        value["license_accepted_at"] = serde_json::json!("2026-08-10T09:00:00Z");
        value["license_acceptance_method"] = serde_json::json!("interactive");
        let lines = terms_lines(&receipt(value)).join("\n");
        assert!(lines.contains("univec-commercial 2026-08-09"), "{lines}");
        assert!(
            lines.contains("https://univec.ai/legal/models/univec-commercial/2026-08-09"),
            "{lines}"
        );
        assert!(
            lines.contains("locally acknowledged 2026-08-10T09:00:00Z (interactive)"),
            "{lines}"
        );
        for overclaim in ["accepted by", "assent", "organization acceptance"] {
            assert!(!lines.contains(overclaim), "{overclaim}: {lines}");
        }
    }
}
