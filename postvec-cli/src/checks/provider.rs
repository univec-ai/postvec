//! External-provider checks: the providers.d directory, per-file (and
//! referenced secret-file) permissions, key-source resolvability, descriptor
//! sanity, and the served-vs-configured diff against the running host.
//!
//! All read-only per the doctor contract, and — like every check module —
//! pure over gathered facts, with the filesystem walk in [`gather`].

use super::CheckResult;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One providers.d file, as far as a read-only inspection can see it.
#[derive(Debug, Clone)]
pub struct ProviderFileFacts {
    /// The file stem — the operator-facing provider name.
    pub name: String,
    pub provider: Option<String>,
    /// `enabled = false`: the file parses and serves nothing, on purpose.
    pub enabled: bool,
    /// The key *source* rendered for display; never a value.
    pub key_source: String,
    /// `Some(problem)` when the key source cannot resolve (missing or
    /// world-readable file; variable unset in this environment).
    pub key_problem: Option<String>,
    /// `(public name, dim)` pairs.
    pub models: Vec<(String, Option<i64>)>,
    /// A permission or parse problem with the file itself.
    pub error: Option<String>,
    /// The file loads, but something about it is worth saying out loud —
    /// today: a plaintext `base_url` on a non-loopback host, which puts the
    /// credential on the wire unencrypted.
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderFacts {
    pub dir: PathBuf,
    pub exists: bool,
    /// Permission bits of the directory itself. The CLI creates it 0700 and
    /// then never re-permissions one that is already there, so this is the
    /// only thing that observes whether it still is.
    pub mode: Option<u32>,
    pub files: Vec<ProviderFileFacts>,
    /// The directory exists and could not be scanned. Reported as a
    /// `provider.directory` failure: the serving host cannot scan it either.
    pub scan_error: Option<String>,
}

pub struct ProviderInput<'a> {
    pub facts: &'a ProviderFacts,
    /// The running host's enabled `/config` names, when a host answered.
    pub served: Option<&'a BTreeSet<String>>,
}

/// Read-only filesystem walk over a providers.d directory.
pub fn gather(dir: &Path) -> ProviderFacts {
    use std::os::unix::fs::PermissionsExt;

    let mut facts = ProviderFacts {
        dir: dir.to_path_buf(),
        exists: dir.is_dir(),
        mode: std::fs::metadata(dir)
            .ok()
            .map(|meta| meta.permissions().mode() & 0o777),
        files: Vec::new(),
        scan_error: None,
    };
    if !facts.exists {
        return facts;
    }
    let files = match crate::commands::provider::ls::provider_files(dir) {
        Ok(files) => files,
        Err(problem) => {
            // An unreadable directory is a finding, not an empty one. The
            // serving host will not scan it either, so every provider in it
            // is down — and "0 provider file(s)" would have said the
            // opposite.
            facts.scan_error = Some(problem);
            return facts;
        }
    };
    for path in files {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        // The same 0600 rule the serving host enforces, checked first so a
        // world-readable file is reported as the load failure it will be.
        let mode = std::fs::symlink_metadata(&path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .unwrap_or(0);
        if mode & 0o077 != 0 {
            facts.files.push(ProviderFileFacts {
                name,
                provider: None,
                enabled: true,
                key_source: "unknown".to_string(),
                key_problem: None,
                models: Vec::new(),
                error: Some(format!(
                    "{} is readable by other users (mode {mode:o}); the host refuses it",
                    path.display()
                )),
                warning: None,
            });
            continue;
        }
        // The loader's own schema, run before the display read. Doctor's
        // job here is to predict the serving host, and a permissive TOML
        // parse cannot: `deny_unknown_fields` (a typo'd `api_kee`), the
        // one-source-per-secret rule, the per-file ceilings and the
        // descriptor rules are all load errors the host will hit and this
        // check would otherwise miss — reporting the file as healthy and
        // then blaming `provider.served` on a missing reload. It reads no
        // secret to reach that verdict.
        if let Err(problem) = providers::config::validate_file(&path) {
            facts.files.push(ProviderFileFacts {
                name,
                provider: None,
                enabled: true,
                key_source: "unknown".to_string(),
                key_problem: None,
                models: Vec::new(),
                error: Some(problem),
                warning: None,
            });
            continue;
        }
        match crate::commands::provider::ProviderFileDoc::load(&path) {
            Ok(Some(doc)) => {
                let key_source = doc.key_source();
                let key_problem = key_source_problem(&doc);
                let models = doc
                    .models()
                    .into_iter()
                    .map(|(model_name, _)| {
                        let dim = doc
                            .value
                            .get("models")
                            .and_then(toml::Value::as_array)
                            .and_then(|list| {
                                list.iter().find(|m| {
                                    m.get("name").and_then(toml::Value::as_str)
                                        == Some(model_name.as_str())
                                })
                            })
                            .and_then(|m| m.get("dim"))
                            .and_then(toml::Value::as_integer);
                        (model_name, dim)
                    })
                    .collect();
                facts.files.push(ProviderFileFacts {
                    name,
                    provider: doc.provider_type().map(str::to_string),
                    enabled: doc.enabled(),
                    key_source,
                    key_problem,
                    models,
                    error: None,
                    warning: plaintext_transport_warning(&doc),
                });
            }
            Ok(None) => {}
            Err(e) => facts.files.push(ProviderFileFacts {
                name,
                provider: None,
                enabled: true,
                key_source: "unknown".to_string(),
                key_problem: None,
                models: Vec::new(),
                error: Some(e.to_string()),
                warning: None,
            }),
        }
    }
    facts
}

/// A file that took the `allow_insecure_transport` opt-in.
///
/// The loader refuses plaintext to a non-loopback host **unless** the file
/// says so in writing, and `provider.file` reports that refusal like any
/// other. This is the other half: a file that opted in loads fine, and the
/// operator should still be reminded on every doctor run that the API key —
/// and every document the column embeds — crosses the network in the clear.
/// An accepted risk is not the same as an invisible one.
fn plaintext_transport_warning(doc: &crate::commands::provider::ProviderFileDoc) -> Option<String> {
    let base_url = doc.value.get("base_url").and_then(toml::Value::as_str)?;
    let opted_in = doc
        .value
        .get("allow_insecure_transport")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    (opted_in && providers::config::base_url_is_plaintext_offhost(base_url)).then(|| {
        format!(
            "base_url {base_url} is plain HTTP to a non-loopback host (accepted via \
             allow_insecure_transport): the API key and every embedded document cross the \
             network unencrypted"
        )
    })
}

/// Whether the file's key sources can resolve, without printing any value.
///
/// Every source is checked, not just the first: an AWS file authenticates
/// with a *pair* (`access_key_id` + `secret_access_key`), so stopping at the
/// first field would report a world-readable secret-key file as healthy —
/// while the host refuses the whole connector over it.
fn key_source_problem(doc: &crate::commands::provider::ProviderFileDoc) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut problems: Vec<String> = Vec::new();

    for (field, path) in doc.secret_file_fields() {
        let path = Path::new(&path);
        match std::fs::symlink_metadata(path) {
            Err(e) => problems.push(format!("{field} {}: {e}", path.display())),
            Ok(meta) if !meta.is_file() => problems.push(format!(
                "{field} {} is not a regular file (the host refuses symlinks too)",
                path.display()
            )),
            Ok(meta) if meta.permissions().mode() & 0o077 != 0 => problems.push(format!(
                "{field} {} is readable by other users (mode {:o}); the host refuses it",
                path.display(),
                meta.permissions().mode() & 0o777
            )),
            Ok(_) => {}
        }
    }
    for (field, var) in doc.secret_env_fields() {
        if std::env::var(&var).is_err() {
            problems.push(format!(
                "{field} {var} is not set in this environment (the POSTMASTER's or server \
                 unit's environment is what the host resolves; this check can only observe \
                 its own)"
            ));
        }
    }

    (!problems.is_empty()).then(|| problems.join("; "))
}

pub fn checks(input: &ProviderInput) -> Vec<CheckResult> {
    let mut out = Vec::new();
    let facts = input.facts;

    if !facts.exists {
        out.push(CheckResult::pass(
            "provider.directory",
            "providers",
            format!(
                "{} does not exist — no external provider is configured (zero-config)",
                facts.dir.display()
            ),
        ));
        return out;
    }
    // Directory mode: setup and provider add create 0700 and never
    // re-permission an existing dir. Group/other read is a warning
    // (discloses which providers). Group/other write is a failure (anyone
    // can drop a connector). The loader refuses write too; doctor says so
    // first.
    if let Some(problem) = &facts.scan_error {
        out.push(
            CheckResult::fail("provider.directory", "providers", problem.clone()).with_fix(
                "the serving host cannot scan this directory either, so every provider in it \
                 is down; fix its permissions",
            ),
        );
        return out;
    }
    match facts.mode {
        Some(mode) if mode & 0o022 != 0 => out.push(
            CheckResult::fail(
                "provider.directory",
                "providers",
                format!(
                    "{} is mode {mode:o}: other users can WRITE here, so anyone with access \
                     can add a provider file and choose where this host sends source text. \
                     The serving host refuses the directory outright",
                    facts.dir.display()
                ),
            )
            .with_fix(format!("chmod 700 {}", facts.dir.display())),
        ),
        Some(mode) if mode & 0o077 != 0 => out.push(
            CheckResult::warn(
                "provider.directory",
                "providers",
                format!(
                    "{} is mode {mode:o}: other users can list the configured providers \
                     ({} file(s))",
                    facts.dir.display(),
                    facts.files.len()
                ),
            )
            .with_fix(format!("chmod 700 {}", facts.dir.display())),
        ),
        _ => out.push(CheckResult::pass(
            "provider.directory",
            "providers",
            format!(
                "{}: {} provider file(s)",
                facts.dir.display(),
                facts.files.len()
            ),
        )),
    }

    for file in &facts.files {
        let scope = format!("provider:{}", file.name);
        if let Some(error) = &file.error {
            out.push(
                CheckResult::fail("provider.file", scope.clone(), error.clone()).with_fix(
                    "the serving host skips this file (local models keep working); fix it and \
                     reload with `postvec provider add/rm`, POST /admin/providers/reload, or a \
                     restart",
                ),
            );
            continue;
        }
        let summary = format!(
            "provider {} — {} model(s), key: {}{}",
            file.provider.as_deref().unwrap_or("?"),
            file.models.len(),
            file.key_source,
            if file.enabled {
                ""
            } else {
                " (enabled = false: parses, serves nothing)"
            }
        );
        out.push(match &file.warning {
            // The file loads; something about how it loads is worth saying.
            Some(warning) => CheckResult::warn(
                "provider.file",
                scope.clone(),
                format!("{summary} — {warning}"),
            )
            .with_fix("use an https base_url, or terminate TLS on the inference host itself"),
            None => CheckResult::pass("provider.file", scope.clone(), summary),
        });

        match &file.key_problem {
            Some(problem) => out.push(
                CheckResult::fail("provider.key-source", scope.clone(), problem.clone())
                    .with_fix("without a resolvable key the host serves nothing for this provider"),
            ),
            None => out.push(CheckResult::pass(
                "provider.key-source",
                scope.clone(),
                format!("key source resolvable ({})", file.key_source),
            )),
        }

        // There is deliberately no separate descriptor check. Every rule one
        // could state here — a positive `dim`, a public name in the host's
        // charset, a non-empty `provider_model_id` — is enforced by
        // `config::validate_file` above, and the host refuses the file over
        // any of them. A second check restating them could only ever pass,
        // and a check that cannot fail teaches operators to skim.

        // A parked file is not a stale reload. Comparing it against what the
        // host serves would warn on every run, forever, for a state the
        // operator chose deliberately.
        if !file.enabled {
            out.push(CheckResult::skip(
                "provider.served",
                scope.clone(),
                "enabled = false, so the host is not expected to serve these models",
            ));
            continue;
        }
        match input.served {
            Some(served) => {
                let missing: Vec<&str> = file
                    .models
                    .iter()
                    .filter(|(name, _)| !served.contains(name))
                    .map(|(name, _)| name.as_str())
                    .collect();
                if missing.is_empty() {
                    out.push(CheckResult::pass(
                        "provider.served",
                        scope.clone(),
                        "the running host serves every configured model",
                    ));
                } else {
                    out.push(
                        CheckResult::warn(
                            "provider.served",
                            scope.clone(),
                            format!("configured but not served: {}", missing.join(", ")),
                        )
                        .with_fix(
                            "reload the host (POST /admin/providers/reload, or `postvec \
                             provider add` again) or restart it; a name colliding with a \
                             local model is served by the local model on purpose",
                        ),
                    );
                }
            }
            None => out.push(CheckResult::skip(
                "provider.served",
                scope.clone(),
                "no running host answered /config, so served-vs-configured was not compared",
            )),
        }
    }
    out
}

/// `doctor --deep` only: configured univec entries that UniVec's current
/// catalogue no longer lists. One attempt per base URL, short timeout, no
/// retry; an unreachable catalogue produces nothing. A note (PASS), never a
/// warning — the catalogue does not model removals reliably, and `--strict`
/// must not trip on a UniVec blip. Ordinary `doctor` never opens this socket.
pub async fn catalogue_notes(dir: &Path, timeout: std::time::Duration) -> Vec<CheckResult> {
    use providers::listing::{self, ListedModel, Listing};
    let mut catalogues: std::collections::BTreeMap<String, Vec<ListedModel>> = Default::default();
    let mut out = Vec::new();
    for path in crate::commands::provider::ls::provider_files(dir).unwrap_or_default() {
        let Ok(Some(doc)) = crate::commands::provider::ProviderFileDoc::load(&path) else {
            continue;
        };
        if doc.provider_type() != Some("univec") {
            continue;
        }
        let base = doc
            .value
            .get("base_url")
            .and_then(toml::Value::as_str)
            .map(str::to_string);
        let key = base.clone().unwrap_or_default();
        if !catalogues.contains_key(&key) {
            match listing::list_models("univec", base.as_deref(), timeout, false).await {
                Ok(Listing::Entries(models)) => catalogues.insert(key.clone(), models),
                _ => continue,
            };
        }
        let catalogue = &catalogues[&key];
        let missing: Vec<String> = doc
            .descriptors()
            .into_iter()
            .filter(|d| match &d.provider_source_id {
                Some(src) => listing::converter(catalogue, src, &d.provider_model_id).is_none(),
                None => listing::embed(catalogue, &d.provider_model_id).is_none(),
            })
            .map(|d| d.name)
            .collect();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push(CheckResult::pass(
            "provider.catalogue",
            format!("provider:{stem}"),
            if missing.is_empty() {
                "every entry is in UniVec's current catalogue".to_string()
            } else {
                format!(
                    "not in UniVec's current catalogue: {}; `postvec provider test {stem}` tells \
                     whether they still serve",
                    missing.join(", ")
                )
            },
        ));
    }
    out
}

/// The grpc-mode note: provider files live on the server nodes; this host
/// has nothing to check unless `--path` points somewhere.
pub fn grpc_note() -> CheckResult {
    CheckResult::skip(
        "provider.directory",
        "providers",
        "remote inference: provider files live on the postvec-server nodes \
         (<server-root>/providers.d), not on this database host",
    )
    .with_fix(
        "inspect each node with `postvec provider ls --path <server-root>` — every node of a \
         fleet must carry the same provider files",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;
    use std::os::unix::fs::PermissionsExt;

    fn write_mode(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn by_id<'a>(checks: &'a [CheckResult], id: &str, scope: &str) -> &'a CheckResult {
        checks
            .iter()
            .find(|c| c.id == id && c.scope == scope)
            .unwrap_or_else(|| panic!("no {id} in scope {scope}: {checks:#?}"))
    }

    /// Present entries pass quietly; a vanished one is a PASS-level note
    /// naming it; an unreachable catalogue yields no check at all.
    #[tokio::test]
    async fn catalogue_notes_name_vanished_entries_and_never_warn() {
        let mock = providers::testing::always(
            200,
            r#"{"success":true,"data":[{"name":"kept","modelType":"embed","targetDim":4}]}"#,
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        write_mode(
            dir.path(),
            "univec.toml",
            &format!(
                "provider = \"univec\"\napi_key = \"k\"\nbase_url = \"{}\"\n\n[[models]]\n\
                 name = \"univec-kept\"\nprovider_model_id = \"kept\"\ndim = 4\n\n[[models]]\n\
                 name = \"univec-gone\"\nprovider_model_id = \"gone\"\ndim = 4\n",
                mock.url
            ),
            0o600,
        );
        let notes = catalogue_notes(dir.path(), std::time::Duration::from_secs(2)).await;
        assert_eq!(notes.len(), 1, "{notes:#?}");
        assert_eq!(notes[0].status, CheckStatus::Pass);
        assert!(
            notes[0].summary.contains("univec-gone") && !notes[0].summary.contains("univec-kept"),
            "{}",
            notes[0].summary
        );

        let dead = providers::testing::always(503, "{}").await;
        write_mode(
            dir.path(),
            "univec.toml",
            &format!("provider = \"univec\"\napi_key = \"k\"\nbase_url = \"{}\"\n\n[[models]]\nname = \"n\"\nprovider_model_id = \"n\"\ndim = 4\n", dead.url),
            0o600,
        );
        assert!(
            catalogue_notes(dir.path(), std::time::Duration::from_secs(2))
                .await
                .is_empty()
        );
    }

    #[test]
    fn a_missing_directory_is_zero_config_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let facts = gather(&dir.path().join("providers.d"));
        let checks = checks(&ProviderInput {
            facts: &facts,
            served: None,
        });
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Pass);
        assert!(checks[0].summary.contains("zero-config"));
    }

    #[test]
    fn permissions_key_sources_and_served_diffs_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        // A healthy file whose key file is world-readable.
        let key = write_mode(dir.path(), "leaky.key", "sk-secret-value", 0o644);
        write_mode(
            dir.path(),
            "openai.toml",
            &format!(
                "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\n\
                 name = \"openai-text-embedding-3-small\"\n\
                 provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
                key.display()
            ),
            0o600,
        );
        // A world-readable provider file.
        write_mode(dir.path(), "cohere.toml", "provider = \"cohere\"", 0o644);
        // A file the serving host refuses over its schema: a typo'd key
        // field. A permissive TOML read accepts it happily, which is why
        // doctor runs the loader's rules.
        write_mode(
            dir.path(),
            "mistral.toml",
            "provider = \"mistral\"\napi_kee = \"k\"\n\n[[models]]\nname = \"m\"\n\
             provider_model_id = \"mistral-embed\"\ndim = 1024\n",
            0o600,
        );

        let facts = gather(dir.path());
        let served: BTreeSet<String> = ["something-else".to_string()].into();
        let results = checks(&ProviderInput {
            facts: &facts,
            served: Some(&served),
        });

        // No check output anywhere contains the key value.
        for check in &results {
            assert!(!check.summary.contains("sk-secret-value"), "{check:?}");
        }

        let leaky = by_id(&results, "provider.key-source", "provider:openai");
        assert_eq!(leaky.status, CheckStatus::Fail);
        assert!(
            leaky.summary.contains("readable by other users"),
            "{leaky:?}"
        );

        let world_readable = by_id(&results, "provider.file", "provider:cohere");
        assert_eq!(world_readable.status, CheckStatus::Fail);

        let schema = by_id(&results, "provider.file", "provider:mistral");
        assert_eq!(schema.status, CheckStatus::Fail);
        assert!(schema.summary.contains("api_kee"), "{schema:?}");

        let served_check = by_id(&results, "provider.served", "provider:openai");
        assert_eq!(served_check.status, CheckStatus::Warn);
        assert!(
            served_check
                .summary
                .contains("openai-text-embedding-3-small"),
            "{served_check:?}"
        );
    }

    /// The directory is created 0700 and never re-permissioned, so doctor is
    /// what notices when it stops being private. A warning, not a failure:
    /// the 0600 files still hide the keys, only the provider names leak.
    #[test]
    fn a_group_readable_directory_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(
            dir.path(),
            "openai.toml",
            "provider = \"openai\"\napi_key = \"sk\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            0o600,
        );

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let results = checks(&ProviderInput {
            facts: &gather(dir.path()),
            served: None,
        });
        assert_eq!(
            by_id(&results, "provider.directory", "providers").status,
            CheckStatus::Pass
        );

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let results = checks(&ProviderInput {
            facts: &gather(dir.path()),
            served: None,
        });
        let check = by_id(&results, "provider.directory", "providers");
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.summary.contains("755"), "{check:?}");
    }

    /// The AWS SigV4 pair is two secrets, and `provider add` never writes
    /// it — so a hand-written file is exactly where a world-readable key
    /// lands. Checking only the first source would have called this healthy
    /// while the host refused the whole connector.
    #[test]
    fn every_aws_secret_source_is_checked_not_just_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let good = write_mode(dir.path(), "id.key", "AKIA", 0o600);
        let leaky = write_mode(dir.path(), "secret.key", "sk-secret-value", 0o644);
        write_mode(
            dir.path(),
            "aws.toml",
            &format!(
                "provider = \"aws\"\nregion = \"us-east-1\"\n\
                 access_key_id_file = \"{}\"\nsecret_access_key_file = \"{}\"\n\n\
                 [[models]]\nname = \"aws-titan-embed-text-v1\"\n\
                 provider_model_id = \"amazon.titan-embed-text-v1\"\ndim = 1536\n",
                good.display(),
                leaky.display()
            ),
            0o600,
        );

        let results = checks(&ProviderInput {
            facts: &gather(dir.path()),
            served: None,
        });
        let key = by_id(&results, "provider.key-source", "provider:aws");
        assert_eq!(key.status, CheckStatus::Fail, "{key:?}");
        assert!(key.summary.contains("secret_access_key_file"), "{key:?}");
        for check in &results {
            assert!(!check.summary.contains("sk-secret-value"), "{check:?}");
        }
    }

    /// The factory refuses an unsupported connector type, and a connector
    /// with no credential declared, at gateway build — where only the host's
    /// log sees it. Both must be `provider.file` failures here, or doctor
    /// reports the file healthy and blames `provider.served` on a reload that
    /// changes nothing.
    #[test]
    fn a_connector_the_host_cannot_build_fails_the_file_check() {
        let dir = tempfile::tempdir().unwrap();
        let models = "\n\n[[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        for (stem, body, expect) in [
            (
                "typo",
                format!("provider = \"opanai\"\napi_key = \"k\"{models}"),
                "unsupported",
            ),
            ("nokey", format!("provider = \"openai\"{models}"), "API key"),
            (
                "aws",
                format!("provider = \"aws\"\nbearer_token = \"t\"{models}"),
                "region",
            ),
        ] {
            write_mode(dir.path(), &format!("{stem}.toml"), &body, 0o600);
            let results = checks(&ProviderInput {
                facts: &gather(dir.path()),
                served: Some(&Default::default()),
            });
            let file = by_id(&results, "provider.file", &format!("provider:{stem}"));
            assert_eq!(file.status, CheckStatus::Fail, "{file:?}");
            assert!(file.summary.contains(expect), "{file:?}");
            // …and nothing tells the operator to reload.
            assert!(
                !results
                    .iter()
                    .any(|c| c.id == "provider.served" && c.scope == format!("provider:{stem}")),
                "a file the host refuses has no served-vs-configured question"
            );
            std::fs::remove_file(dir.path().join(format!("{stem}.toml"))).unwrap();
        }
    }

    /// Plaintext to a non-loopback host is a **failure** unless the file
    /// opted in, and a standing **warning** when it did. An accepted risk is
    /// not an invisible one. Loopback — a sidecar, a mock — is silent.
    #[test]
    fn plaintext_off_host_fails_unless_opted_in_and_then_warns_forever() {
        let dir = tempfile::tempdir().unwrap();
        let file = |base_url: &str, opt_in: &str| {
            format!(
                "provider = \"openai\"\napi_key = \"sk\"\nbase_url = \"{base_url}\"\n{opt_in}\n\
                 [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n"
            )
        };
        let status_of = |body: &str| {
            write_mode(dir.path(), "openai.toml", body, 0o600);
            let facts = gather(dir.path());
            let results = checks(&ProviderInput {
                facts: &facts,
                served: None,
            });
            let check = by_id(&results, "provider.file", "provider:openai");
            (check.status, check.summary.clone())
        };

        let (status, summary) = status_of(&file("http://vllm.internal:8000", ""));
        assert_eq!(status, CheckStatus::Fail, "{summary}");
        assert!(summary.contains("allow_insecure_transport"), "{summary}");

        let (status, summary) = status_of(&file(
            "http://vllm.internal:8000",
            "allow_insecure_transport = true",
        ));
        assert_eq!(status, CheckStatus::Warn, "{summary}");
        assert!(summary.contains("unencrypted"), "{summary}");

        let (status, summary) = status_of(&file("http://127.0.0.1:8000", ""));
        assert_eq!(status, CheckStatus::Pass, "{summary}");
    }

    /// `enabled = false` is a state an operator chose. Reporting it as
    /// "configured but not served" would warn on every doctor run forever.
    #[test]
    fn a_disabled_file_is_not_reported_as_an_unreloaded_host() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(
            dir.path(),
            "openai.toml",
            "provider = \"openai\"\nenabled = false\napi_key = \"sk\"\n\n[[models]]\n\
             name = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
            0o600,
        );
        let served: BTreeSet<String> = Default::default();
        let results = checks(&ProviderInput {
            facts: &gather(dir.path()),
            served: Some(&served),
        });
        let file = by_id(&results, "provider.file", "provider:openai");
        assert_eq!(file.status, CheckStatus::Pass);
        assert!(file.summary.contains("enabled = false"), "{file:?}");
        assert_eq!(
            by_id(&results, "provider.served", "provider:openai").status,
            CheckStatus::Skip
        );
    }

    #[test]
    fn a_healthy_file_passes_and_an_unasked_host_skips_served() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(
            dir.path(),
            "openai.toml",
            "provider = \"openai\"\napi_key = \"sk-inline\"\n\n[[models]]\n\
             name = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
            0o600,
        );
        let facts = gather(dir.path());
        let results = checks(&ProviderInput {
            facts: &facts,
            served: None,
        });
        assert_eq!(
            by_id(&results, "provider.file", "provider:openai").status,
            CheckStatus::Pass
        );
        assert_eq!(
            by_id(&results, "provider.key-source", "provider:openai").status,
            CheckStatus::Pass
        );
        assert_eq!(
            by_id(&results, "provider.served", "provider:openai").status,
            CheckStatus::Skip
        );
        for check in &results {
            assert!(!check.summary.contains("sk-inline"), "{check:?}");
        }
    }

    #[test]
    fn the_grpc_note_points_at_the_server_nodes() {
        let note = grpc_note();
        assert_eq!(note.status, CheckStatus::Skip);
        assert!(!note.required, "informational, not blocking");
        assert!(note.remediation.as_deref().unwrap_or("").contains("--path"));
    }
}
