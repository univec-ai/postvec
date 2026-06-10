//! Registry credentials: where they live, how they resolve, how they fail.
//!
//! One rule dominates this module: a selected credential that fails
//! authentication stops the command. It never falls through to a
//! lower-priority source and never silently degrades to the anonymous
//! channel. Revocation must surface as "authentication failed", not as
//! "model not found".
//!
//! Resolution order: `POSTVEC_API_KEY` → `--api-key-file` → the effective
//! user's store → anonymous. The store is per effective user (root:
//! `/var/lib/postvec/auth.json`; otherwise `$XDG_CONFIG_HOME/postvec/
//! auth.json`), which is why a non-root `postvec login` is deliberately
//! invisible to a later `sudo postvec model pull`.

use crate::config::owned;
use crate::error::{CliError, Result};
use crate::proc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const API_KEY_ENV: &str = "POSTVEC_API_KEY";
const ROOT_STORE: &str = "/var/lib/postvec/auth.json";
const STORE_SCHEMA_VERSION: u32 = 1;

/// Where a resolved key came from — reported by `whoami`, never with the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialSource {
    Environment,
    KeyFile,
    Store,
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CredentialSource::Environment => API_KEY_ENV,
            CredentialSource::KeyFile => "--api-key-file",
            CredentialSource::Store => "the credential store",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub key: String,
    pub source: CredentialSource,
}

impl Credential {
    /// `uv_abcdefghi…` — the 12-character prefix that is also the server-side
    /// lookup key, safe to print.
    pub fn masked(&self) -> String {
        masked_key(&self.key)
    }
}

pub fn masked_key(key: &str) -> String {
    let prefix: String = key.chars().take(12).collect();
    format!("{prefix}…")
}

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    schema_version: u32,
    api_key: String,
}

/// The effective user's store path. Root gets the system path so a root
/// `login` survives for root `pull`s; everyone else gets XDG.
pub fn store_path() -> Result<PathBuf> {
    if proc::is_root() {
        return Ok(PathBuf::from(ROOT_STORE));
    }
    let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                CliError::precondition("cannot locate a credential store: HOME is not set")
            })?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(config_home.join("postvec").join("auth.json"))
}

/// Basic shape check before a key is stored or sent anywhere. Deliberately
/// loose about content (the server is the authority) and strict about the
/// things that indicate a pasted mistake.
pub fn validate_key_shape(raw: &str) -> Result<String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err(CliError::usage("the API key is empty"));
    }
    if key.len() < 12 {
        return Err(CliError::usage("the API key is too short to be valid"));
    }
    if !key.starts_with("uv_") {
        return Err(CliError::usage(
            "the API key does not start with uv_; paste a key from the UniVec dashboard",
        ));
    }
    if key.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err(CliError::usage(
            "the API key contains whitespace or non-printable characters",
        ));
    }
    Ok(key.to_string())
}

/// Resolve a credential without prompting. `Ok(None)` means anonymous by
/// absence of any credential — the only path to the public channel.
pub fn resolve(api_key_file: Option<&Path>) -> Result<Option<Credential>> {
    if let Some(raw) = std::env::var_os(API_KEY_ENV) {
        let raw = raw
            .to_str()
            .ok_or_else(|| CliError::usage(format!("{API_KEY_ENV} is not valid UTF-8")))?;
        // An explicitly set but empty variable is treated as unset: shells
        // produce that shape too easily for it to be an error.
        if !raw.trim().is_empty() {
            return Ok(Some(Credential {
                key: validate_key_shape(raw)?,
                source: CredentialSource::Environment,
            }));
        }
    }
    if let Some(path) = api_key_file {
        let content = owned::read_regular_file(path)?
            .ok_or_else(|| CliError::usage(format!("{} does not exist", path.display())))?;
        return Ok(Some(Credential {
            key: validate_key_shape(&content)?,
            source: CredentialSource::KeyFile,
        }));
    }
    let store = store_path()?;
    let euid = unsafe { libc::geteuid() };
    Ok(read_private_store(&store, euid)?.map(|key| Credential {
        key,
        source: CredentialSource::Store,
    }))
}

/// The key in a store file, provided the store is still private to `owner`:
/// a regular file (no symlink), 0600, owned by that uid. `Ok(None)` when
/// there is no store. `provider add univec` reads the invoking user's store
/// through this as root, so the owner is a parameter, not the effective uid.
pub fn read_private_store(store: &Path, owner: u32) -> Result<Option<String>> {
    let Some(content) = owned::read_regular_file(store)? else {
        return Ok(None);
    };
    check_store_permissions(store, owner)?;
    let parsed: StoreFile = serde_json::from_str(&content).map_err(|e| {
        CliError::precondition(format!("{} is malformed: {e}", store.display()))
            .with_fix("run `postvec logout` and log in again")
    })?;
    if parsed.schema_version != STORE_SCHEMA_VERSION {
        return Err(CliError::precondition(format!(
            "{} has schema_version {}, this postvec-cli understands {STORE_SCHEMA_VERSION}",
            store.display(),
            parsed.schema_version
        ))
        .with_fix("upgrade postvec-cli"));
    }
    Ok(Some(validate_key_shape(&parsed.api_key)?))
}

/// A stored credential is only used when the store is still private: a
/// regular file owned by the effective user with no group/other
/// permission bits. `login` writes it that way; a copied, restored or
/// hand-edited store that drifted is refused with the exact remediation
/// rather than silently continuing to expose a long-lived key.
/// `--api-key-file` stays more flexible on purpose: container secret
/// mounts are often group-readable by design.
pub fn check_store_permissions(store: &Path, owner: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(store)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", store.display())))?;
    if !meta.is_file() {
        return Err(CliError::precondition(format!(
            "{} is not a regular file; refusing to use the stored key",
            store.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CliError::precondition(format!(
            "{} is readable by other users (mode {mode:o}); refusing to use the stored key",
            store.display()
        ))
        .with_fix(format!(
            "chmod 600 {} (and rotate the key if others could have read it)",
            store.display()
        )));
    }
    if meta.uid() != owner {
        return Err(CliError::precondition(format!(
            "{} is owned by uid {}, not uid {owner}",
            store.display(),
            meta.uid()
        ))
        .with_fix(
            "chown the store to the invoking account, or run `postvec logout` and log in again",
        ));
    }
    Ok(())
}

/// Persist a key to the effective user's store, `0600`, atomically.
pub fn save(key: &str) -> Result<PathBuf> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        // 0700: the parent may not exist yet, and a credential directory has
        // no business being world-readable even though the file is 0600.
        owned::create_dir_all_checked(parent, 0o700)?;
    }
    let body = serde_json::to_vec_pretty(&StoreFile {
        schema_version: STORE_SCHEMA_VERSION,
        api_key: key.to_string(),
    })
    .map_err(|e| CliError::internal(format!("cannot serialize credential store: {e}")))?;
    owned::write_atomic(&path, &body, 0o600)?;
    Ok(path)
}

/// Remove the effective user's store. `Ok(false)` when there was none.
pub fn remove() -> Result<bool> {
    let path = store_path()?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CliError::apply(format!(
            "cannot remove {}: {e}",
            path.display()
        ))),
    }
}

/// The `whoami` description of where a credential search looked — accurate
/// even when nothing was found.
pub fn search_description(api_key_file: Option<&Path>) -> String {
    let env_state = if std::env::var_os(API_KEY_ENV).is_some_and(|v| !v.is_empty()) {
        "set"
    } else {
        "unset"
    };
    let store = store_path()
        .map(|p| {
            let state = if p.exists() { "present" } else { "absent" };
            format!("{} ({state})", p.display())
        })
        .unwrap_or_else(|_| "credential store (unavailable)".to_string());
    match api_key_file {
        Some(path) => format!(
            "{API_KEY_ENV} ({env_state}), --api-key-file {}, {store}",
            path.display()
        ),
        None => format!("{API_KEY_ENV} ({env_state}), {store}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_shape_validation_rejects_pasted_mistakes() {
        assert!(validate_key_shape("").is_err());
        assert!(validate_key_shape("short").is_err());
        assert!(validate_key_shape("sk_notunivec_key").is_err());
        assert!(validate_key_shape("uv_abc def ghij").is_err());
        assert!(validate_key_shape("uv_abc\ndefghij").is_err());
        assert_eq!(
            validate_key_shape("  uv_abcdefghijklmnop\n").unwrap(),
            "uv_abcdefghijklmnop"
        );
    }

    #[test]
    fn masking_keeps_only_the_lookup_prefix() {
        assert_eq!(masked_key("uv_abcdefghijSECRETSECRET"), "uv_abcdefghi…");
    }
}
