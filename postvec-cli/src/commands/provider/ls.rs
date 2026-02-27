//! `postvec provider ls` — configured providers, key *sources* (never a
//! key), models with dimensions, and whether the running host currently
//! serves them.

use super::{resolve_target, ProviderFileDoc};
use crate::cli::{Cli, ProviderLsArgs};
use crate::error::{Exit, Result};
use crate::output::Output;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
struct LsModel {
    name: String,
    provider_model_id: String,
    dim: Option<i64>,
    /// `None` when no running host could be asked.
    served: Option<bool>,
}

#[derive(Serialize)]
struct LsProvider {
    name: String,
    provider: String,
    /// `enabled = false`: parses, serves nothing. Shown so a deliberately
    /// parked file does not read as a host that never reloaded.
    enabled: bool,
    key_source: String,
    file: String,
    models: Vec<LsModel>,
}

#[derive(Serialize)]
struct LsDocument {
    schema_version: u32,
    command: &'static str,
    target: String,
    directory: String,
    providers: Vec<LsProvider>,
    errors: Vec<String>,
}

pub async fn run(cli: &Cli, args: ProviderLsArgs, output: &Output) -> Result<Exit> {
    let target = resolve_target(cli, args.path.as_deref(), output, false).await?;
    let dir = target.dir().to_path_buf();

    // Served set from the running host, when there is one to ask.
    let served: Option<BTreeSet<String>> = match target.embedded_listen() {
        Some(listen) => crate::commands::model::admin::loaded_inventory(&listen, cli.timeout)
            .await
            .map(|inventory| inventory.enabled_names()),
        None => None,
    };

    let mut providers = Vec::new();
    let mut errors = Vec::new();
    for path in provider_files(&dir) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match ProviderFileDoc::load(&path) {
            Ok(Some(doc)) => {
                let enabled = doc.enabled();
                let models = doc
                    .models()
                    .into_iter()
                    .map(|(name, id)| {
                        let dim = doc
                            .value
                            .get("models")
                            .and_then(toml::Value::as_array)
                            .and_then(|list| {
                                list.iter().find(|m| {
                                    m.get("name").and_then(toml::Value::as_str) == Some(&name)
                                })
                            })
                            .and_then(|m| m.get("dim"))
                            .and_then(toml::Value::as_integer);
                        LsModel {
                            // A parked file is not expected to be served, so
                            // do not invite a reload that would change
                            // nothing.
                            served: enabled
                                .then(|| served.as_ref().map(|set| set.contains(&name)))
                                .flatten(),
                            name,
                            provider_model_id: id,
                            dim,
                        }
                    })
                    .collect();
                providers.push(LsProvider {
                    name: stem,
                    provider: doc.provider_type().unwrap_or("?").to_string(),
                    enabled,
                    key_source: doc.key_source(),
                    file: path.display().to_string(),
                    models,
                });
            }
            Ok(None) => {}
            Err(e) => errors.push(e.to_string()),
        }
    }

    if output.is_json() {
        output.show_document(
            &LsDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "provider ls",
                target: target.label(),
                directory: dir.display().to_string(),
                providers,
                errors,
            },
            "",
        )?;
        return Ok(Exit::Success);
    }

    output.progress(&format!("providers.d: {}", dir.display()));
    if providers.is_empty() && errors.is_empty() {
        output.progress("no provider is configured");
    }
    for provider in &providers {
        output.progress(&format!(
            "{}  ({}, key: {}){}",
            provider.name,
            provider.provider,
            provider.key_source,
            if provider.enabled {
                ""
            } else {
                "  [enabled = false]"
            }
        ));
        for model in &provider.models {
            output.progress(&format!(
                "  {:<44} dim {:<6} {}",
                model.name,
                model
                    .dim
                    .map(|d| d.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                match (provider.enabled, model.served) {
                    (false, _) => "disabled in the file",
                    (true, Some(true)) => "served",
                    (true, Some(false)) => "NOT served (reload or restart the host)",
                    (true, None) => "(no running host to ask)",
                }
            ));
        }
    }
    for error in &errors {
        output.progress(&format!("! {error}"));
    }
    Ok(Exit::Success)
}

/// `*.toml` files, lexicographic, dot-files skipped — the loader's rule.
pub fn provider_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "toml")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    files.sort();
    files
}
