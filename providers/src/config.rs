//! The `providers.d` configuration directory (feature `wire`).
//!
//! One TOML file per provider; the files on disk are the complete serving
//! truth. Credentials resolve here — at load time, on the inference host —
//! and nowhere else: the resolved [`crate::ProviderConfig`] never leaves
//! this process, and no error message produced by this module ever carries
//! a secret value (paths and env-var *names* only).
//!
//! Loading rules (docs/external-providers.md §6.1):
//! - read `*.toml` in lexicographic order; skip dot-files;
//! - `enabled = false` files parse but serve nothing;
//! - a TOML file or referenced secret file with mode broader than 0600 is
//!   refused with a named error, never silently accepted;
//! - one broken file must not take down the others: per-file failures are
//!   reported alongside whatever loaded (the caller logs them);
//! - duplicate public model names across files: first wins, warning logged.

use crate::ProviderConfig;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Per-provider outbound concurrency cap when the file does not set one.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// Per-attempt HTTP timeout when the file does not set one.
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// Provider items-per-request cap when the descriptor does not set one.
/// Conservative: every supported provider accepts at least this many.
pub const DEFAULT_MAX_BATCH: usize = 96;

/// Hard ceilings against a corrupt or hostile file. `max_concurrent` sizes a
/// semaphore and widens the hosts' ingress gate (§7.3), so it must not be
/// operator-unbounded.
const MAX_MAX_CONCURRENT: usize = 64;
const MAX_MODELS_PER_FILE: usize = 64;
const MAX_NAME_BYTES: usize = 128;

fn default_true() -> bool {
    true
}
fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}
fn default_max_batch() -> usize {
    DEFAULT_MAX_BATCH
}

/// The on-disk TOML schema. `deny_unknown_fields` on purpose: a typo'd
/// credential field must be a load error, not a silently ignored key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderFile {
    /// Connector type: openai | openrouter | mistral | google | cohere | aws
    /// (the CLI accepts "gemini"/"amazon" aliases and the factory honors
    /// them, but files should carry the canonical names).
    provider: String,
    #[serde(default = "default_true")]
    enabled: bool,

    // Key-authenticated providers: exactly one of the triad.
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,

    base_url: Option<String>,
    #[serde(default = "default_max_concurrent")]
    max_concurrent: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,

    // AWS: region plus either the bearer-token triad or the SigV4 pair
    // (each via its own file/env/inline triad).
    region: Option<String>,
    bearer_token: Option<String>,
    bearer_token_file: Option<PathBuf>,
    bearer_token_env: Option<String>,
    access_key_id: Option<String>,
    access_key_id_file: Option<PathBuf>,
    access_key_id_env: Option<String>,
    secret_access_key: Option<String>,
    secret_access_key_file: Option<PathBuf>,
    secret_access_key_env: Option<String>,

    #[serde(default)]
    models: Vec<ModelDescriptor>,
}

/// One provider-backed model as declared in a provider file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    /// Public name served in `/config` (e.g. `openai-text-embedding-3-small`).
    pub name: String,
    /// The identifier the provider's API expects (e.g. `text-embedding-3-small`).
    pub provider_model_id: String,
    /// Vector dimension; authoritative for the response-shape check.
    pub dim: u32,
    /// Provider items-per-request cap; the gateway sub-batches to it.
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
    /// Advertised as `sequence_len` in the discovery descriptor.
    pub max_tokens: Option<u32>,
}

/// A provider file with every secret resolved to a value.
#[derive(Debug)]
pub struct ResolvedProvider {
    /// The file stem (`openai` for `openai.toml`) — the operator-facing name.
    pub name: String,
    /// Resolved connector configuration for the factory.
    pub config: ProviderConfig,
    pub max_concurrent: usize,
    pub timeout_ms: u64,
    pub models: Vec<ModelDescriptor>,
}

/// One file that failed to load. The message never contains a secret.
#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// The outcome of scanning a providers.d directory. Per-file failures are
/// isolated: `providers` holds everything that loaded.
#[derive(Debug, Default)]
pub struct LoadOutcome {
    pub providers: Vec<ResolvedProvider>,
    pub errors: Vec<LoadError>,
}

/// Refuse any file whose mode grants group/other bits — same discipline as
/// the CLI's credential store. Applies to the TOML itself and to every
/// referenced secret file: the TOML being secret-free is not enough if the
/// key file is world-readable.
fn assert_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users (mode {mode:o}); chmod 600 it (and rotate the key \
             if others could have read it)",
            path.display()
        ));
    }
    Ok(())
}

/// Resolve one secret from its inline/file/env triad. Exactly one source may
/// be set; a referenced file must be 0600 and is trimmed of trailing
/// whitespace (key files conventionally end with a newline). Returns
/// `Ok(None)` when the whole triad is absent — presence requirements are the
/// factory's per-provider business.
fn resolve_secret(
    field: &str,
    inline: &Option<String>,
    file: &Option<PathBuf>,
    env: &Option<String>,
) -> Result<Option<String>, String> {
    let set = [inline.is_some(), file.is_some(), env.is_some()]
        .iter()
        .filter(|s| **s)
        .count();
    if set > 1 {
        return Err(format!(
            "{field}, {field}_file and {field}_env are mutually exclusive; set exactly one"
        ));
    }
    let value = if let Some(v) = inline {
        v.clone()
    } else if let Some(path) = file {
        assert_private(path)?;
        std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {field}_file {}: {e}", path.display()))?
    } else if let Some(var) = env {
        std::env::var(var).map_err(|_| {
            format!("{field}_env names {var:?}, which is not set in this process's environment")
        })?
    } else {
        return Ok(None);
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("{field} resolved to an empty value"));
    }
    Ok(Some(value))
}

/// Public-name rule: the same charset the hosts accept for model names.
fn validate_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model name is empty".to_string());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("model name exceeds {MAX_NAME_BYTES} bytes"));
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!("model name {name:?} must start with [a-z0-9]"));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "model name {name:?} contains characters outside [a-z0-9._-]"
        ));
    }
    Ok(())
}

/// Parse and resolve one provider file. `Ok(None)` = disabled (parses, serves
/// nothing).
fn load_file(path: &Path) -> Result<Option<ResolvedProvider>, String> {
    assert_private(path)?;
    let raw = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let file: ProviderFile = toml::from_str(&raw).map_err(|e| format!("cannot parse: {e}"))?;

    if !file.enabled {
        return Ok(None);
    }
    if file.models.is_empty() {
        return Err("no [[models]] entries; an enabled provider must serve something".to_string());
    }
    if file.models.len() > MAX_MODELS_PER_FILE {
        return Err(format!(
            "{} [[models]] entries exceeds the {MAX_MODELS_PER_FILE} per-file ceiling",
            file.models.len()
        ));
    }
    if file.max_concurrent == 0 || file.max_concurrent > MAX_MAX_CONCURRENT {
        return Err(format!(
            "max_concurrent must be between 1 and {MAX_MAX_CONCURRENT}"
        ));
    }
    if file.timeout_ms == 0 {
        return Err("timeout_ms must be positive".to_string());
    }
    for model in &file.models {
        validate_model_name(&model.name).map_err(|e| format!("[[models]]: {e}"))?;
        if model.provider_model_id.trim().is_empty() {
            return Err(format!(
                "[[models]] {:?}: provider_model_id is empty",
                model.name
            ));
        }
        if model.dim == 0 || model.dim as i64 > 100_000 {
            return Err(format!(
                "[[models]] {:?}: dim {} is not plausible",
                model.name, model.dim
            ));
        }
        if model.max_batch == 0 {
            return Err(format!(
                "[[models]] {:?}: max_batch must be ≥ 1",
                model.name
            ));
        }
    }

    let config = ProviderConfig {
        provider: file.provider.clone(),
        api_key: resolve_secret(
            "api_key",
            &file.api_key,
            &file.api_key_file,
            &file.api_key_env,
        )?,
        base_url: file.base_url.clone(),
        region: file.region.clone(),
        bearer_token: resolve_secret(
            "bearer_token",
            &file.bearer_token,
            &file.bearer_token_file,
            &file.bearer_token_env,
        )?,
        access_key_id: resolve_secret(
            "access_key_id",
            &file.access_key_id,
            &file.access_key_id_file,
            &file.access_key_id_env,
        )?,
        secret_access_key: resolve_secret(
            "secret_access_key",
            &file.secret_access_key,
            &file.secret_access_key_file,
            &file.secret_access_key_env,
        )?,
    };

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| file.provider.clone());

    Ok(Some(ResolvedProvider {
        name,
        config,
        max_concurrent: file.max_concurrent,
        timeout_ms: file.timeout_ms,
        models: file.models,
    }))
}

/// Scan a providers.d directory. A missing directory is the zero-config
/// case: an empty outcome, no error. An existing-but-unreadable directory is
/// a structural error (`Err`), distinct from per-file failures.
pub fn load_dir(dir: &Path) -> Result<LoadOutcome, String> {
    let mut outcome = LoadOutcome::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(format!("cannot scan {}: {e}", dir.display())),
    };

    // Lexicographic order makes the duplicate-name rule ("first wins")
    // deterministic across platforms and readdir orders.
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "toml")
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| !n.starts_with('.'))
        })
        .collect();
    paths.sort();

    let mut seen_models: std::collections::BTreeSet<String> = Default::default();
    for path in paths {
        match load_file(&path) {
            Ok(Some(mut provider)) => {
                // Duplicate public names across files: first wins.
                provider.models.retain(|m| {
                    let fresh = seen_models.insert(m.name.clone());
                    if !fresh {
                        log::warn!(
                            "providers.d: model {:?} in {} duplicates an earlier file; \
                             first definition wins",
                            m.name,
                            path.display()
                        );
                    }
                    fresh
                });
                if provider.models.is_empty() {
                    log::warn!(
                        "providers.d: every model in {} was already defined earlier; \
                         the file serves nothing",
                        path.display()
                    );
                } else {
                    outcome.providers.push(provider);
                }
            }
            Ok(None) => log::info!("providers.d: {} is disabled; skipping", path.display()),
            Err(message) => outcome.errors.push(LoadError { path, message }),
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_mode(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    const OPENAI_TOML: &str = r#"
provider = "openai"
api_key = "sk-inline"
max_concurrent = 2

[[models]]
name = "openai-text-embedding-3-small"
provider_model_id = "text-embedding-3-small"
dim = 1536
max_batch = 512
max_tokens = 8191
"#;

    #[test]
    fn loads_a_valid_file_and_applies_defaults() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.providers.len(), 1);
        let p = &outcome.providers[0];
        assert_eq!(p.name, "openai");
        assert_eq!(p.config.provider, "openai");
        assert_eq!(p.config.api_key.as_deref(), Some("sk-inline"));
        assert_eq!(p.max_concurrent, 2);
        assert_eq!(p.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(p.models[0].dim, 1536);
        assert_eq!(p.models[0].max_batch, 512);
        assert_eq!(p.models[0].max_tokens, Some(8191));
    }

    #[test]
    fn refuses_a_world_readable_toml() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o644);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("readable by other users"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn refuses_a_world_readable_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = write_mode(dir.path(), "openai.key", "sk-from-file\n", 0o640);
        let body = format!(
            "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            key.display()
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(
            outcome.errors[0]
                .message
                .contains("readable by other users"),
            "{}",
            outcome.errors[0]
        );
        // Errors must reference the path, never the key value.
        assert!(!outcome.errors[0].message.contains("sk-from-file"));
    }

    #[test]
    fn resolves_key_files_and_trims_trailing_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let key = write_mode(dir.path(), "openai.key", "sk-from-file\n", 0o600);
        let body = format!(
            "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            key.display()
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(
            outcome.providers[0].config.api_key.as_deref(),
            Some("sk-from-file")
        );
    }

    #[test]
    fn key_source_triad_is_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let body = "provider = \"openai\"\napi_key = \"a\"\napi_key_env = \"OPENAI_API_KEY\"\n\n\
                    [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        write_mode(dir.path(), "openai.toml", body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(
            outcome.errors[0].message.contains("mutually exclusive"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn a_missing_env_var_names_the_variable_not_a_value() {
        let dir = tempfile::tempdir().unwrap();
        let body = "provider = \"openai\"\napi_key_env = \"POSTVEC_TEST_NO_SUCH_VAR\"\n\n\
                    [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        write_mode(dir.path(), "openai.toml", body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(
            outcome.errors[0]
                .message
                .contains("POSTVEC_TEST_NO_SUCH_VAR"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn disabled_files_parse_but_serve_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let body = OPENAI_TOML.replace(
            "provider = \"openai\"",
            "provider = \"openai\"\nenabled = false",
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn one_broken_file_does_not_take_down_the_others() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(dir.path(), "aaa-broken.toml", "not toml at all", 0o600);
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.providers.len(), 1, "the good file still loads");
        assert_eq!(outcome.errors.len(), 1);
    }

    #[test]
    fn duplicate_model_names_across_files_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        // Lexicographic order: a.toml before b.toml.
        let a = "provider = \"openai\"\napi_key = \"a\"\n\n[[models]]\nname = \"shared-name\"\n\
                 provider_model_id = \"first\"\ndim = 4\n";
        let b = "provider = \"mistral\"\napi_key = \"b\"\n\n[[models]]\nname = \"shared-name\"\n\
                 provider_model_id = \"second\"\ndim = 8\n";
        write_mode(dir.path(), "a.toml", a, 0o600);
        write_mode(dir.path(), "b.toml", b, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.providers.len(), 1, "b.toml serves nothing");
        assert_eq!(outcome.providers[0].models[0].provider_model_id, "first");
    }

    #[test]
    fn missing_directory_is_zero_config_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("providers.d");
        let outcome = load_dir(&missing).unwrap();
        assert!(outcome.providers.is_empty() && outcome.errors.is_empty());
    }

    #[test]
    fn dot_files_and_non_toml_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_mode(dir.path(), ".hidden.toml", "garbage", 0o600);
        write_mode(dir.path(), "README.md", "docs", 0o644);
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty() && outcome.errors.is_empty());
    }

    #[test]
    fn unknown_fields_are_a_load_error() {
        // A typo'd credential field must fail loudly, not be ignored.
        let dir = tempfile::tempdir().unwrap();
        let body = OPENAI_TOML.replace("api_key =", "api_kee =");
        write_mode(dir.path(), "openai.toml", &body, 0o600);
        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0].message.contains("api_kee"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn implausible_descriptors_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (marker, body) in [
            (
                "dim",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 0\n",
            ),
            (
                "model name",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"Bad Name\"\n\
                 provider_model_id = \"m\"\ndim = 4\n",
            ),
            (
                "max_concurrent",
                "provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = 0\n\n[[models]]\n\
                 name = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n",
            ),
            ("models", "provider = \"openai\"\napi_key = \"k\"\n"),
        ] {
            let file = write_mode(dir.path(), "p.toml", body, 0o600);
            let outcome = load_dir(dir.path()).unwrap();
            assert_eq!(outcome.errors.len(), 1, "case {marker}");
            std::fs::remove_file(file).unwrap();
        }
    }
}
