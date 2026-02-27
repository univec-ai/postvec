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
    };
    if !facts.exists {
        return facts;
    }
    for path in crate::commands::provider::ls::provider_files(dir) {
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
            }),
        }
    }
    facts
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
    // The directory holds credentials, so its own mode matters. `setup` and
    // `provider add` create it 0700 and deliberately leave an existing one
    // alone, which is why nothing else notices when it drifts. Group or
    // other bits do not expose a key (the files are 0600) but they do expose
    // which providers a host is configured for, so this is a warning rather
    // than a failure.
    match facts.mode {
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
        out.push(CheckResult::pass(
            "provider.file",
            scope.clone(),
            format!(
                "provider {} — {} model(s), key: {}{}",
                file.provider.as_deref().unwrap_or("?"),
                file.models.len(),
                file.key_source,
                if file.enabled {
                    ""
                } else {
                    " (enabled = false: parses, serves nothing)"
                }
            ),
        ));

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
