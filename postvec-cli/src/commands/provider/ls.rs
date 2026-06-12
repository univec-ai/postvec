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
    /// An embed model's width; a converter's TARGET width.
    dim: Option<i64>,
    /// `kind = "convert"` entries: the route in the resolver's vocabulary,
    /// `source_model[source_dim] -> target_model[dim]`. Absent for embeds.
    #[serde(skip_serializing_if = "Option::is_none")]
    converts: Option<String>,
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
    /// `Some(reason)` when the serving host refuses this file outright.
    /// Without it a broken file's models read as `NOT served (reload or
    /// restart the host)` — the one remedy that cannot work, which is
    /// exactly the misdiagnosis `doctor` stopped making when it started
    /// running the loader's own rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
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
    if args.available {
        return run_available(cli, &args, output).await;
    }
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
    let files = provider_files(&dir).map_err(|e| {
        crate::error::CliError::precondition(e)
            .with_fix("the serving host cannot scan this directory either; fix its permissions")
    })?;
    for path in files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // The loader's own verdict on this file, so `ls` cannot describe a
        // file the host refuses as one that merely needs a reload.
        let refused = providers::config::validate_file(&path).err();
        match ProviderFileDoc::load(&path) {
            Ok(Some(doc)) => {
                let enabled = doc.enabled();
                // Schema-typed entries, for the converter route display; a
                // hand-edited entry the schema refuses still lists via the
                // lenient reader below, just without one.
                let typed: std::collections::BTreeMap<String, providers::config::ModelDescriptor> =
                    doc.descriptors()
                        .into_iter()
                        .map(|d| (d.name.clone(), d))
                        .collect();
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
                        let converts = typed
                            .get(&name)
                            .filter(|d| d.kind == providers::config::ModelKind::Convert)
                            .map(|d| {
                                format!(
                                    "{}[{}] -> {}[{}]",
                                    d.source_model.as_deref().unwrap_or("?"),
                                    d.source_dim.unwrap_or(0),
                                    d.target_model.as_deref().unwrap_or("?"),
                                    d.dim
                                )
                            });
                        LsModel {
                            // A parked or refused file is not expected to be
                            // served, so do not invite a reload that would
                            // change nothing.
                            served: (enabled && refused.is_none())
                                .then(|| served.as_ref().map(|set| set.contains(&name)))
                                .flatten(),
                            name,
                            provider_model_id: id,
                            dim,
                            converts,
                        }
                    })
                    .collect();
                providers.push(LsProvider {
                    name: stem,
                    provider: doc.provider_type().unwrap_or("?").to_string(),
                    enabled,
                    key_source: doc.key_source(),
                    file: path.display().to_string(),
                    refused,
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
        if let Some(reason) = &provider.refused {
            output.progress(&format!("  ! the host REFUSES this file: {reason}"));
        }
        for model in &provider.models {
            let shape = match &model.converts {
                Some(route) => format!("converts {route}"),
                None => format!(
                    "dim {:<6}",
                    model
                        .dim
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "?".to_string())
                ),
            };
            output.progress(&format!(
                "  {:<44} {shape} {}",
                model.name,
                match (provider.refused.is_some(), provider.enabled, model.served) {
                    (true, _, _) => "not loadable (see above)",
                    (false, false, _) => "disabled in the file",
                    (false, true, Some(true)) => "served",
                    (false, true, Some(false)) => "NOT served (reload or restart the host)",
                    (false, true, None) => "(no running host to ask)",
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
///
/// A directory that exists but cannot be scanned, or an entry that cannot be
/// read, is an **error** and not an empty list. Returning nothing there made
/// `ls` and doctor describe an unreadable providers.d as "no provider is
/// configured" — indistinguishable from the zero-config state, for a
/// directory the serving host also refuses to scan. A missing directory is
/// still the ordinary empty case.
pub fn provider_files(
    dir: &std::path::Path,
) -> std::result::Result<Vec<std::path::PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot scan {}: {e}", dir.display())),
    };
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?
            .path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'));
        if path.extension().is_some_and(|ext| ext == "toml") && named {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

// ---- `provider ls --available`: what a provider OFFERS ---------------------

#[derive(Serialize)]
struct AvailableModel {
    #[serde(flatten)]
    listed: providers::listing::ListedModel,
    /// Joined against the directory by the same identity `route_models()`
    /// uses; `None` when no directory was resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    configured: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    configured_name: Option<String>,
}

#[derive(Serialize)]
struct AvailableProvider {
    name: String,
    /// Absent for a configured file that could not be classified.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// The configured file (or directory) a `failed` outcome is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// `entries` | `needs_key` | `unsupported` | `failed`.
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    models: Vec<AvailableModel>,
}

#[derive(Serialize)]
struct AvailableDocument {
    schema_version: u32,
    command: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
    available: bool,
    /// Why the `configured` join is absent, when no directory was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    configured_state_unavailable: Option<String>,
    providers: Vec<AvailableProvider>,
    /// One line per `failed` provider; non-empty makes the exit a failure.
    errors: Vec<String>,
}

/// One catalogue to ask: a configured file (its connector and `base_url`
/// override), or a bare connector type against its default endpoint.
struct Source {
    name: String,
    provider: String,
    base_url: Option<String>,
    doc: Option<ProviderFileDoc>,
}

async fn run_available(cli: &Cli, args: &ProviderLsArgs, output: &Output) -> Result<Exit> {
    use providers::listing::{self, ListedKind, Listing};

    let wanted = args
        .provider
        .as_deref()
        .map(|p| providers::catalog::canonical_provider(&p.to_lowercase()));
    // A directory is optional — the catalogue needs only the network, the
    // same way `model ls --available` works with no engine root — but only
    // when nothing was *asked for*. An explicit --path, --database-url,
    // --cluster or POSTVEC_PROVIDERS_PATH that cannot be honoured is an
    // error, not a silent degrade to `configured: ?`.
    let explicit = args.path.is_some()
        || cli.database_url.is_some()
        || cli.cluster.is_some()
        || std::env::var_os(crate::config::PROVIDERS_PATH_ENV).is_some_and(|v| !v.is_empty());
    let (target, unavailable) = match resolve_target(cli, args.path.as_deref(), output, false).await
    {
        Ok(target) => (Some(target), None),
        Err(e) if explicit => return Err(e),
        Err(e) => {
            output.note(&format!(
                "no providers directory in scope ({e}); listing the catalogue without the \
                     configured-state join"
            ));
            (None, Some(e.to_string()))
        }
    };
    let mut sources: Vec<Source> = Vec::new();
    // A file (or directory) that cannot be read is a named `failed` result,
    // not a skipped one: a request for `univec-staging` must not degrade
    // into a bare-connector lookup because its file is malformed.
    let mut broken: Vec<AvailableProvider> = Vec::new();
    let mut failures = Vec::new();
    let failed = |name: String, file: String, reason: String| AvailableProvider {
        name,
        provider: None,
        base_url: None,
        file: Some(file),
        outcome: "failed",
        reason: Some(reason),
        models: Vec::new(),
    };
    if let Some(target) = &target {
        // Per FILE, not per connector type: two univec files with different
        // base URLs are two catalogues.
        let files = match provider_files(target.dir()) {
            Ok(files) => files,
            Err(problem) => {
                failures.push(problem.clone());
                let dir = target.dir().display().to_string();
                broken.push(failed(dir.clone(), dir, problem));
                Vec::new()
            }
        };
        for path in files {
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let doc = match ProviderFileDoc::load(&path) {
                Ok(Some(doc)) => doc,
                Ok(None) => continue,
                // Unclassifiable, so it may be the connector a type filter
                // asked about: always reported.
                Err(e) => {
                    failures.push(format!("{stem}: {e}"));
                    broken.push(failed(stem, path.display().to_string(), e.to_string()));
                    continue;
                }
            };
            let provider =
                providers::catalog::canonical_provider(doc.provider_type().unwrap_or(""));
            if wanted
                .as_ref()
                .is_some_and(|w| *w != stem && *w != provider)
            {
                continue;
            }
            sources.push(Source {
                name: stem,
                base_url: doc
                    .value
                    .get("base_url")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string),
                provider,
                doc: Some(doc),
            });
        }
    }
    // Nothing configured for the request: the named connector, or UniVec —
    // the one catalogue the tree exists to promote, and it costs nothing.
    let bare = wanted.clone().unwrap_or_else(|| "univec".to_string());
    let named_file_is_broken = broken.iter().any(|b| Some(&b.name) == wanted.as_ref());
    if (sources.is_empty() && !named_file_is_broken)
        || (wanted.is_none() && !sources.iter().any(|s| s.provider == bare))
    {
        if !providers::config::SUPPORTED_PROVIDERS.contains(&bare.as_str()) {
            return Err(crate::error::CliError::usage(format!(
                "unknown provider {bare:?}: name a configured file or one of {}",
                providers::config::SUPPORTED_PROVIDERS.join(", ")
            )));
        }
        sources.push(Source {
            name: bare.clone(),
            provider: bare,
            base_url: None,
            doc: None,
        });
    }

    let mut providers_out = broken;
    for source in sources {
        let configured: Option<std::collections::BTreeMap<String, String>> =
            target.as_ref().map(|_| {
                source
                    .doc
                    .as_ref()
                    .map(|doc| {
                        doc.descriptors()
                            .into_iter()
                            .map(|d| {
                                let key = match &d.provider_source_id {
                                    Some(src) => format!("{src}->{}", d.provider_model_id),
                                    None => d.provider_model_id.clone(),
                                };
                                (key, d.name)
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            });
        let base_url = source
            .base_url
            .clone()
            .unwrap_or_else(|| default_base_url(&source.provider).to_string());
        let (outcome, reason, models) = match listing::list_models(
            &source.provider,
            source.base_url.as_deref(),
            cli.timeout,
            true,
        )
        .await
        {
            Ok(Listing::Entries(models)) => ("entries", None, models),
            Ok(Listing::NeedsKey { route }) => (
                "needs_key",
                Some(format!("{route} needs a key")),
                Vec::new(),
            ),
            Ok(Listing::Unsupported { reason }) => ("unsupported", Some(reason), Vec::new()),
            Err(e) => {
                failures.push(format!("{}: {e}", source.name));
                ("failed", Some(e.to_string()), Vec::new())
            }
        };
        let models = models
            .into_iter()
            .filter(|m| args.kind.is_none_or(|k| ListedKind::from(k) == m.kind))
            .filter(|m| {
                args.to
                    .as_deref()
                    .is_none_or(|t| m.kind == ListedKind::Convert && m.provider_model_id == t)
            })
            .filter(|m| {
                args.from
                    .as_deref()
                    .is_none_or(|f| m.source.as_ref().is_some_and(|(s, _)| s == f))
            })
            .map(|listed| {
                let key = match &listed.source {
                    Some((src, _)) => format!("{src}->{}", listed.provider_model_id),
                    None => listed.provider_model_id.clone(),
                };
                let name = configured.as_ref().and_then(|c| c.get(&key).cloned());
                AvailableModel {
                    configured: configured.as_ref().map(|_| name.is_some()),
                    configured_name: name,
                    listed,
                }
            })
            .collect();
        providers_out.push(AvailableProvider {
            name: source.name,
            provider: Some(source.provider),
            base_url: Some(base_url),
            file: None,
            outcome,
            reason,
            models,
        });
    }

    if output.is_json() {
        output.show_document(
            &AvailableDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "provider ls",
                target: target.as_ref().map(|t| t.label()),
                available: true,
                configured_state_unavailable: unavailable,
                providers: providers_out,
                errors: failures.clone(),
            },
            "",
        )?;
    } else {
        for p in &providers_out {
            let embeds = p
                .models
                .iter()
                .filter(|m| m.listed.kind == ListedKind::Embed)
                .count();
            let at = p
                .base_url
                .clone()
                .or_else(|| p.file.clone())
                .unwrap_or_default();
            match &p.reason {
                Some(reason) => {
                    output.progress(&format!("{}  ({at}, cannot list: {reason})", p.name))
                }
                None => output.progress(&format!(
                    "{}  ({at}, public catalogue, {embeds} embed, {} convert)",
                    p.name,
                    p.models.len() - embeds
                )),
            }
            if !p.models.is_empty() {
                output.progress(&format!(
                    "  {:<52} {:<8} {:<11} configured",
                    "name", "kind", "dim"
                ));
            }
            for m in &p.models {
                let (name, dim) = match &m.listed.source {
                    Some((src, sd)) => (
                        format!("{src} -> {}", m.listed.provider_model_id),
                        format!("{sd}->{}", m.listed.dim),
                    ),
                    None => (m.listed.provider_model_id.clone(), m.listed.dim.to_string()),
                };
                let configured = match (m.configured, &m.configured_name) {
                    (Some(true), Some(as_name)) => format!("yes ({as_name})"),
                    (Some(_), _) => "no".to_string(),
                    (None, _) => "?".to_string(),
                };
                let quality = m
                    .listed
                    .quality
                    .map(|q| format!("   cos {q:.3}"))
                    .unwrap_or_default();
                output.progress(&format!(
                    "  {name:<52} {:<8} {dim:<11} {configured}{quality}",
                    m.listed.kind.label()
                ));
            }
        }
    }
    // One document, one exit: a failed provider is in the output above,
    // never a second error envelope after it.
    if failures.is_empty() {
        Ok(Exit::Success)
    } else {
        if !output.is_json() {
            output.progress(&format!("! listing failed for {}", failures.join("; ")));
        }
        Ok(Exit::Failure)
    }
}

fn default_base_url(provider: &str) -> &'static str {
    match provider {
        "univec" => providers::univec::DEFAULT_UNIVEC_BASE_URL,
        "openai" => providers::openai::DEFAULT_OPENAI_BASE_URL,
        "openrouter" => providers::openrouter::DEFAULT_OPENROUTER_BASE_URL,
        "mistral" => providers::mistral::DEFAULT_MISTRAL_BASE_URL,
        "google" => providers::gemini::DEFAULT_GEMINI_BASE_URL,
        "cohere" => providers::cohere::DEFAULT_COHERE_BASE_URL,
        _ => "(derived from --region)",
    }
}
