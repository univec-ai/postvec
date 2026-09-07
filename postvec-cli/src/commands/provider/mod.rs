//! `postvec provider ...`: providers.d connector files.
//!
//! `--path` wins (filesystem only). Else `POSTVEC_PROVIDERS_PATH`. Else
//! the selected cluster: embedded uses `postvec.providers_path`; grpc is
//! refused (files live on the server nodes) with `--path` named.
//! Credentials never ride argv. Files are 0600 in a 0700 directory.

pub mod add;
pub mod ls;
pub mod rm;
pub mod test;
pub mod univec;

use crate::cli::Cli;
use crate::cli::Mode;
use crate::commands::Context;
use crate::error::{CliError, Result};
use crate::facts::SettingsSnapshot;
use crate::output::Output;
use crate::plan::ApplyJournal;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The postvec-server loopback admin port (`postvec-server/src/cli.rs`),
/// where `--path` roots get their best-effort reload.
const POSTVEC_SERVER_ADMIN: &str = "127.0.0.1:22223";

/// Who new provider files belong to when this process is root and is acting
/// on someone else's behalf. Only the numeric pair is ever used, and a
/// `--path` root supplies one that has no account name to look up here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileOwner {
    pub uid: u32,
    pub gid: u32,
}

impl From<&crate::proc::OsAccount> for FileOwner {
    fn from(account: &crate::proc::OsAccount) -> Self {
        FileOwner {
            uid: account.uid,
            gid: account.gid,
        }
    }
}

/// The owner of `dir`, or of its nearest existing ancestor.
///
/// This is where a `--path` target gets its owner. A server root exists and
/// belongs to the account `postvec-server` runs as, so inheriting from it
/// makes `sudo postvec provider add --path <root>` write files that node can
/// actually read. Without it the files land root-owned 0600 and the node
/// skips them with a permission error, which looks like the provider was
/// never configured.
pub fn owner_of_nearest_existing(dir: &Path) -> Option<FileOwner> {
    use std::os::unix::fs::MetadataExt;
    let mut candidate = Some(dir);
    while let Some(path) = candidate {
        if let Ok(meta) = std::fs::metadata(path) {
            return Some(FileOwner {
                uid: meta.uid(),
                gid: meta.gid(),
            });
        }
        candidate = path.parent();
    }
    None
}

/// What a provider command works against.
pub enum ProviderTarget {
    /// `--path DIR` or `POSTVEC_PROVIDERS_PATH`: a providers.d directory,
    /// no cluster involved. `source` names which, for messages.
    Path {
        dir: PathBuf,
        source: &'static str,
        /// Inherited from the root the operator named, so files written
        /// under `sudo` still belong to the account that serves them.
        owner: Option<FileOwner>,
    },
    /// The selected cluster, embedded mode.
    Embedded {
        dir: PathBuf,
        settings: SettingsSnapshot,
        /// `None` when the command could not open a database session and is
        /// working from the owned configuration snippet (read-only commands
        /// only).
        context: Option<Box<Context>>,
        cluster_id: String,
    },
}

impl ProviderTarget {
    pub fn dir(&self) -> &Path {
        match self {
            ProviderTarget::Path { dir, .. } => dir,
            ProviderTarget::Embedded { dir, .. } => dir,
        }
    }

    /// `--path` or `POSTVEC_PROVIDERS_PATH` for a files-only target, which
    /// checks no columns.
    pub fn files_only(&self) -> Option<&'static str> {
        match self {
            ProviderTarget::Path { source, .. } => Some(source),
            ProviderTarget::Embedded { .. } => None,
        }
    }

    /// The label that stands in for `cluster` in result envelopes.
    pub fn label(&self) -> String {
        match self {
            ProviderTarget::Path { dir, .. } => format!("path:{}", dir.display()),
            ProviderTarget::Embedded { cluster_id, .. } => cluster_id.clone(),
        }
    }

    /// Who a file written for this target should belong to. A cluster target
    /// uses the cluster owner; a `--path` root uses the account that owns
    /// the root.
    pub fn owner(&self) -> Option<FileOwner> {
        match self {
            ProviderTarget::Path { owner, .. } => *owner,
            ProviderTarget::Embedded { context, .. } => context
                .as_ref()
                .and_then(|ctx| ctx.cluster.owner.as_ref())
                .map(FileOwner::from),
        }
    }

    /// The embedded engine's loopback /config listener, when there is one.
    /// A files-only target at the extension's default providers.d is that
    /// host's own directory (containers, packaged hosts), so its default
    /// listener is asked.
    pub fn embedded_listen(&self) -> Option<String> {
        match self {
            ProviderTarget::Embedded { settings, .. } => Some(settings.embedded_http_listen()),
            ProviderTarget::Path { dir, .. }
                if dir == Path::new(crate::config::DEFAULT_PROVIDERS_PATH) =>
            {
                Some(crate::config::DEFAULT_EMBEDDED_HTTP_LISTEN.to_string())
            }
            ProviderTarget::Path { .. } => None,
        }
    }
}

/// The provider name is a **file stem** that every command joins onto the
/// providers.d path, so it has to be validated before it becomes one —
/// `provider rm ../../etc/something` under `sudo` would otherwise delete a
/// file outside the directory entirely. The charset is the loader's own
/// (`config::validate_model_name`), which also keeps a name from colliding
/// with the `.tmp` sibling `write_secret_file` uses.
pub fn validate_provider_name(name: &str, flag: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(CliError::usage(format!("{flag} must be 1..=64 characters")));
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(CliError::usage(format!(
            "{flag} {name:?} must start with [a-z0-9]"
        )));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(CliError::usage(format!(
            "{flag} {name:?} contains characters outside [a-z0-9._-]"
        )));
    }
    Ok(())
}

/// Regular files in `<providers.d parent>/keys/` — where `provider add
/// univec --api-key-from-login` copies the login key and where operator
/// key files conventionally live. Teardown reports them and retains them.
pub fn sibling_key_files(providers_dir: &Path) -> Vec<PathBuf> {
    let Some(keys) = providers_dir.parent().map(|p| p.join("keys")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(&keys)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()))
        .collect();
    files.sort();
    files
}

/// `<DIR>/providers.d`, or `<DIR>` itself when it already is one.
pub fn providers_dir_from_path(dir: &Path) -> PathBuf {
    if dir.file_name().is_some_and(|name| name == "providers.d") {
        dir.to_path_buf()
    } else {
        dir.join("providers.d")
    }
}

/// Resolve the provider-command target. `mutating` commands need a live
/// database session on a cluster target (the in-use scan and the reload
/// need it); read-only ones fall back to the owned snippet like `model ls`.
pub async fn resolve_target(
    cli: &Cli,
    path: Option<&Path>,
    output: &Output,
    mutating: bool,
) -> Result<ProviderTarget> {
    if let Some(path) = path {
        let path = crate::validate::absolute_path(path, "--path")?;
        return path_target(path, "--path");
    }
    // An explicit URI beats the environment: an image may bake
    // POSTVEC_PROVIDERS_PATH in, and it must not shadow a deliberate
    // cluster choice.
    if cli.database_url.is_some() {
        let context = Context::open(cli, output).await?;
        return target_from_live(context).await;
    }
    // POSTVEC_PROVIDERS_PATH: same semantics as --path (pure file
    // management, best-effort node reload). The postvec-server image sets
    // it, which is what lets `docker exec <ctr> postvec provider …` run
    // flag-free.
    if let Some(dir) = crate::config::env_path_override(crate::config::PROVIDERS_PATH_ENV)? {
        output.note(&format!(
            "using providers path {} (from {})",
            dir.display(),
            crate::config::PROVIDERS_PATH_ENV
        ));
        return path_target(dir, crate::config::PROVIDERS_PATH_ENV);
    }
    let cluster = Context::discover_local(cli, output).await?;
    let cluster_id = cluster.identity.id.clone();
    match Context::connect_to(cli, cluster.clone(), output).await {
        Ok(context) => target_from_live(context).await,
        Err(error) if !mutating => match target_from_snippet(&cluster, cluster_id, output) {
            Some(target) => Ok(target),
            None => Err(error),
        },
        Err(error) => Err(CliError::precondition(format!(
            "{error}; changing provider files for this cluster also needs a database login so \
             affected columns can be checked and the running host reloaded"
        ))
        .with_fix(
            "rerun with sudo (it drops to the cluster owner), as the cluster owner, or pass \
             --database-url. For files only, pass --path DIR",
        )),
    }
}

/// A direct filesystem target from `--path` or the environment override.
fn path_target(path: PathBuf, source: &'static str) -> Result<ProviderTarget> {
    // The root must already be there. It is the only thing that says who
    // the files belong to, and a typo would otherwise build a whole
    // credential tree in a directory nothing reads.
    if !path.is_dir() {
        return Err(CliError::precondition(format!(
            "{source} {} is not an existing directory",
            path.display()
        ))
        .with_fix(
            "name a server root (or a providers.d) that already exists on this host; \
             `postvec provider` creates the providers.d inside it, not the root itself",
        ));
    }
    let dir = providers_dir_from_path(&path);
    let owner = owner_of_nearest_existing(&dir);
    Ok(ProviderTarget::Path { dir, source, owner })
}

async fn target_from_live(mut context: Context) -> Result<ProviderTarget> {
    let snapshot = crate::commands::collect::cluster_snapshot(&mut context).await?;
    target_from_settings(snapshot.settings, Some(context), None)
}

fn target_from_snippet(
    cluster: &crate::cluster::Cluster,
    cluster_id: String,
    output: &Output,
) -> Option<ProviderTarget> {
    let paths = cluster.owned_paths().ok()?;
    let content = match paths.read_config() {
        Ok(Some(content)) => content,
        _ => return None,
    };
    let settings = crate::config::owned::settings_from_snippet(&content);
    let config_path = paths.config.clone();
    match target_from_settings(settings, None, Some(cluster_id)) {
        Ok(target) => {
            output.note(&format!(
                "could not log in as the cluster owner; using {}",
                config_path.display()
            ));
            Some(target)
        }
        Err(_) => None,
    }
}

fn target_from_settings(
    settings: SettingsSnapshot,
    context: Option<Context>,
    cluster_id: Option<String>,
) -> Result<ProviderTarget> {
    let cluster_id = cluster_id
        .or_else(|| context.as_ref().map(|ctx| ctx.cluster.identity.id.clone()))
        .unwrap_or_else(|| "cluster".to_string());
    match settings.mode() {
        Some(Mode::Embedded) => Ok(ProviderTarget::Embedded {
            dir: settings.providers_path(),
            settings,
            context: context.map(Box::new),
            cluster_id,
        }),
        // Not a dead end: name the escape hatch and where the files live.
        Some(Mode::Grpc) => Err(CliError::precondition(
            "the selected cluster uses remote inference; provider files live on the \
             postvec-server nodes (default <server-root>/providers.d, e.g. \
             /var/lib/postvec-server/providers.d), not on this database host",
        )
        .with_fix(
            "run `postvec provider … --path <server-root>` on each node — every node of a \
             fleet must carry the same provider files — and the node's loopback admin port \
             picks the change up (POST /admin/providers/reload); a restart works too",
        )),
        None => Err(CliError::precondition(format!(
            "the cluster's postvec.mode is unparseable ({:?})",
            settings.raw_mode().unwrap_or("unset")
        ))
        .with_fix("fix postvec.mode, or pass --path <DIR> to manage a providers.d directly")),
    }
}

// ---- providers.d file editing (CLI side) --------------------------------
//
// The serve-path parser (0600 refusal, secret resolution, validation) lives
// in the providers crate; this side edits files as TOML documents so
// unknown-but-valid future fields survive a `provider add` to an existing
// file. Comments do not survive a rewrite — the header says the file is
// CLI-managed.

/// A providers.d file as an editable TOML document.
pub struct ProviderFileDoc {
    pub path: PathBuf,
    pub value: toml::Value,
}

impl ProviderFileDoc {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;

        // `O_NOFOLLOW`, because this path is about to be *rewritten* by a
        // root-run command: following a symlink here would make the rename
        // below land somewhere the operator did not name. Bounded for the
        // same reason the loader bounds it — a connector file is a few dozen
        // lines.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return Err(CliError::precondition(format!(
                    "{} is a symlink; provider files must be regular files",
                    path.display()
                ))
                .with_fix("replace the symlink with a regular file, or remove it"))
            }
            Err(e) => {
                return Err(CliError::precondition(format!(
                    "cannot read {}: {e}",
                    path.display()
                )))
            }
        };
        let mut raw = String::new();
        // `take(limit + 1)`, exactly as the loader's `read_private` does. A
        // `take(limit)` followed by `len() > limit` is an unreachable check:
        // the read stops at the limit, so an oversized file arrived here as a
        // silently truncated prefix — which then parsed, and which a
        // subsequent `add` would have *rewritten*, discarding the tail of the
        // operator's file without a word.
        file.take(providers::config::MAX_FILE_BYTES + 1)
            .read_to_string(&mut raw)
            .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
        if raw.len() as u64 > providers::config::MAX_FILE_BYTES {
            return Err(CliError::precondition(format!(
                "{} is larger than {} bytes; the serving host refuses it",
                path.display(),
                providers::config::MAX_FILE_BYTES
            )));
        }
        let value: toml::Value = raw
            .parse()
            .map_err(|e| CliError::precondition(format!("cannot parse {}: {e}", path.display())))?;
        Ok(Some(Self {
            path: path.to_path_buf(),
            value,
        }))
    }

    pub fn provider_type(&self) -> Option<&str> {
        self.value.get("provider").and_then(toml::Value::as_str)
    }

    /// `enabled = false` parses and serves nothing. Absent means enabled,
    /// matching the loader's default. Reported everywhere "is the host
    /// serving this?" is asked, so a deliberately parked file does not read
    /// as a reload that never happened.
    pub fn enabled(&self) -> bool {
        self.value
            .get("enabled")
            .and_then(toml::Value::as_bool)
            .unwrap_or(true)
    }

    /// Every field this file uses as a *file* secret source. The loader
    /// refuses each of them at 0600, so every one of them is worth checking
    /// before the host does — including the AWS SigV4 pair, which is easy to
    /// forget precisely because `provider add` never writes it.
    pub fn secret_file_fields(&self) -> Vec<(&'static str, String)> {
        [
            "api_key_file",
            "bearer_token_file",
            "access_key_id_file",
            "secret_access_key_file",
        ]
        .into_iter()
        .filter_map(|field| {
            self.value
                .get(field)
                .and_then(toml::Value::as_str)
                .map(|path| (field, path.to_string()))
        })
        .collect()
    }

    /// Every field this file uses as an *environment* secret source.
    pub fn secret_env_fields(&self) -> Vec<(&'static str, String)> {
        [
            "api_key_env",
            "bearer_token_env",
            "access_key_id_env",
            "secret_access_key_env",
        ]
        .into_iter()
        .filter_map(|field| {
            self.value
                .get(field)
                .and_then(toml::Value::as_str)
                .map(|var| (field, var.to_string()))
        })
        .collect()
    }

    /// `(public name, provider_model_id)` pairs, in file order.
    pub fn models(&self) -> Vec<(String, String)> {
        self.value
            .get("models")
            .and_then(toml::Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| {
                        Some((
                            m.get("name")?.as_str()?.to_string(),
                            m.get("provider_model_id")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Typed `[[models]]` entries, via the loader's own schema. Entries the
    /// schema refuses (hand-edited files) are skipped — callers that need a
    /// lenient view keep using [`Self::models`].
    pub fn descriptors(&self) -> Vec<providers::config::ModelDescriptor> {
        self.value
            .get("models")
            .and_then(toml::Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.clone().try_into().ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `(public name, ROUTE model id)` pairs, in file order — the identity
    /// [`providers::config::ServedBy`] carries. An embed entry's route id IS
    /// its `provider_model_id`; a converter's folds in its provider-side
    /// pair, dims and postvec-side spaces via the same `route_model_id`
    /// derivation the loader and the serving descriptor use — one
    /// derivation, or `provider rm`'s no-host conservative view would report
    /// a partial removal as a handoff to its own file. Entries the schema
    /// refuses fall back to the raw pair, exactly what [`Self::models`]
    /// reports.
    pub fn route_models(&self) -> Vec<(String, String)> {
        let typed: std::collections::BTreeMap<String, String> = self
            .descriptors()
            .into_iter()
            .map(|d| (d.name.clone(), d.route_model_id()))
            .collect();
        self.models()
            .into_iter()
            .map(|(name, raw_id)| {
                let id = typed.get(&name).cloned().unwrap_or(raw_id);
                (name, id)
            })
            .collect()
    }

    /// The key source, for `ls`/doctor display — never a value.
    pub fn key_source(&self) -> String {
        let field = |name: &str| self.value.get(name).and_then(toml::Value::as_str);
        if let Some(path) = field("api_key_file") {
            return format!("file:{path}");
        }
        if let Some(var) = field("api_key_env") {
            return format!("env:{var}");
        }
        if self.value.get("api_key").is_some() {
            return "inline (redacted)".to_string();
        }
        if let Some(path) = field("bearer_token_file") {
            return format!("bearer file:{path}");
        }
        if let Some(var) = field("bearer_token_env") {
            return format!("bearer env:{var}");
        }
        if self.value.get("bearer_token").is_some() {
            return "bearer inline (redacted)".to_string();
        }
        if self.value.get("access_key_id").is_some()
            || self.value.get("access_key_id_file").is_some()
            || self.value.get("access_key_id_env").is_some()
        {
            return "aws sigv4 (redacted)".to_string();
        }
        "none".to_string()
    }

    /// The file's mutable top-level table. A TOML document always is one, so
    /// this cannot fail on anything `load` accepted.
    fn table(&mut self) -> &mut toml::map::Map<String, toml::Value> {
        self.value
            .as_table_mut()
            .expect("a parsed TOML document is a table")
    }

    /// A hand-edited `models = 3` is an operator mistake, not an invariant
    /// violation — so it is a refusal with the file named, never a panic in a
    /// command an operator ran under `sudo`.
    pub fn push_model(&mut self, entry: toml::Value) -> Result<()> {
        let path = self.path.clone();
        let models = self
            .table()
            .entry("models")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        match models.as_array_mut() {
            Some(array) => {
                array.push(entry);
                Ok(())
            }
            None => Err(CliError::precondition(format!(
                "{} has a `models` field that is not a [[models]] array; the serving host \
                 refuses the file as it stands",
                path.display()
            ))
            .with_fix("fix or remove the `models` field by hand, then rerun")),
        }
    }

    /// Replace one model's declared dimension, by public name.
    ///
    /// `provider add` writes every entry before the probe so that the whole
    /// file can be validated, then comes back and sets the dimensions the
    /// probe measured.
    pub fn set_model_dim(&mut self, public_name: &str, dim: u32) -> Result<()> {
        let path = self.path.clone();
        let entry = self
            .table()
            .get_mut("models")
            .and_then(toml::Value::as_array_mut)
            .and_then(|models| {
                models
                    .iter_mut()
                    .find(|m| m.get("name").and_then(toml::Value::as_str) == Some(public_name))
            })
            .and_then(toml::Value::as_table_mut);
        match entry {
            Some(table) => {
                table.insert("dim".into(), toml::Value::Integer(dim as i64));
                Ok(())
            }
            None => Err(CliError::internal(format!(
                "{}: no [[models]] entry named {public_name:?} to set a dimension on",
                path.display()
            ))),
        }
    }

    /// Remove one model by public name or provider id; the remaining count
    /// tells the caller whether the file itself should go.
    pub fn remove_model(&mut self, id_or_name: &str) -> (bool, usize) {
        let Some(models) = self
            .value
            .get_mut("models")
            .and_then(toml::Value::as_array_mut)
        else {
            return (false, 0);
        };
        let before = models.len();
        models.retain(|m| {
            let matches = |field: &str| {
                m.get(field)
                    .and_then(toml::Value::as_str)
                    .is_some_and(|v| v == id_or_name)
            };
            !(matches("name") || matches("provider_model_id"))
        });
        (models.len() != before, models.len())
    }

    /// Run the serving host's own rules over this document as it currently
    /// stands.
    ///
    /// Called *before* the verification probe as well as at the write. The
    /// write-time check alone was too late: a paid call had already been made
    /// against a file the host would refuse for a reason the probe cannot
    /// see — an unknown field, two sources for one secret, a `dim` outside a
    /// model's range.
    ///
    /// Returns whether the file is enabled, so a caller can say so: a model
    /// added to a parked file is written correctly and serves nothing, and
    /// "succeeded" without that qualification is a lie.
    pub fn validate_prospective(&self) -> Result<bool> {
        let body = self.body()?;
        let label = self.path.display().to_string();
        let refuse = |problem: String| {
            CliError::precondition(format!(
                "the resulting {label} is one the inference host would refuse: {problem}"
            ))
            .with_fix(
                "fix the flag (or the hand-edited field) this reports; nothing has been written \
                 or sent",
            )
        };
        let enabled = providers::config::validate_str(&body, &label).map_err(refuse)?;
        // The file can be fine and the *directory* still refused — over the
        // file, model or concurrency ceilings, or a name another file already
        // owns — and then the whole gateway comes up empty at the next
        // restart. Checked against the directory as it would be after this
        // write.
        if let Some(dir) = self.path.parent() {
            providers::config::validate_prospective_dir(dir, &self.path, &body).map_err(|p| {
                CliError::precondition(format!(
                    "writing {label} would leave {} in a state the inference host refuses as a \
                     whole: {p}",
                    dir.display()
                ))
                .with_fix("nothing has been written or sent; the running host is unaffected")
            })?;
        }
        Ok(enabled)
    }

    /// The file as it would be written.
    pub fn body(&self) -> Result<String> {
        Ok(format!(
            "# Managed by `postvec provider`. Comments do not survive a rewrite.\n{}",
            toml::to_string_pretty(&self.value)
                .map_err(|e| CliError::internal(format!("cannot serialize provider file: {e}")))?
        ))
    }

    /// Serialize and write: 0600 file, 0700 directory, chowned to `owner`
    /// when one is known and we can (root).
    ///
    /// The rendered document is checked against the **loader's own rules**
    /// first. The serving host refuses a connector file as a whole, so one
    /// entry this CLI got wrong — an implausible `dim`, a `base_url` the
    /// connector cannot use, a connector type with no credential — would take
    /// that provider's already-working models down at the next reload. The
    /// command composing the file is the last place that can still stop it,
    /// and running the host's rules rather than restating them is what keeps
    /// there being one rulebook. No secret is resolved to reach the verdict.
    // The error carries residue and sync state a two-file commit must
    // inspect; boxing it would only move the bytes.
    #[allow(clippy::result_large_err)]
    pub fn write(&self, owner: Option<FileOwner>) -> std::result::Result<(), WriteError> {
        let clean = |error: CliError| WriteError {
            committed: false,
            residue: None,
            sync_error: None,
            error,
        };
        let body = self.body().map_err(clean)?;
        let label = self.path.display().to_string();
        providers::config::validate_str(&body, &label).map_err(|problem| {
            clean(
                CliError::precondition(format!(
                    "the resulting {label} is one the inference host would refuse: {problem}"
                ))
                .with_fix(
                    "fix the flag (or the hand-edited field) this reports; the file was not \
                     written, so the host keeps serving whatever it serves now",
                ),
            )
        })?;
        write_secret_file(&self.path, body.as_bytes(), owner)
    }
}

/// Create `dir` 0700, owned by `owner` when we are root acting on their
/// behalf, and report whether it had to be created. Idempotent, and it never
/// touches a directory that already exists — the operator's own mode and
/// ownership are theirs to keep.
///
/// This is where the providers.d directory comes from. The packages
/// deliberately do not ship it: an nfpm-declared owner would have to name
/// `postgres` (postvec serves clusters owned by other accounts too) and would
/// be applied at unpack time, before the PostgreSQL packages have created
/// that account. The CLI, by contrast, knows the cluster owner, so it creates
/// the directory at `postvec setup --embedded` and here.
pub fn ensure_private_dir(dir: &Path, owner: Option<FileOwner>) -> Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let expected_uid = owner.map(|o| o.uid);
    let refuse = |problem: String| {
        CliError::precondition(problem)
            .with_fix("chmod 700 the providers.d directory (and make sure it is not a symlink)")
    };
    if dir.exists() {
        // An existing directory keeps its mode and ownership — but it still
        // has to be a *directory*, reached without following a symlink,
        // through a chain nobody else can rewrite. This command runs under
        // `sudo`; a group-writable providers.d, or one reached through a
        // parent somebody else owns, means another account chooses what a
        // root-run `provider add` creates and where the serving host sends
        // source text. Refuse and name the fix rather than repairing it
        // implicitly: silently chmod-ing someone's directory is its own
        // surprise. The serving host applies the identical rule at load
        // (`providers::config::validate_directory`), which is why this can be
        // that same function rather than a second opinion.
        providers::config::validate_directory(dir, expected_uid).map_err(refuse)?;
        return Ok(false);
    }

    // The **create** path needs the same check, before it creates anything.
    // It had none: `ensure_private_dir` validated only a directory that
    // already existed, so the first `provider add` on a host built a
    // credential tree — and then a `.lock` file and a `chown` — under a
    // parent chain nobody had looked at. The deepest existing ancestor is
    // what the new directory will hang from, so it is what has to be safe.
    let mut anchor = dir.parent();
    while let Some(candidate) = anchor {
        if candidate.exists() {
            providers::config::validate_directory(candidate, expected_uid).map_err(refuse)?;
            break;
        }
        anchor = candidate.parent();
    }
    // Parents (`/etc/postvec`) are 0755, not 0700: only the leaf holds
    // credentials, and a 0700 `/etc/postvec` would hide unrelated files.
    //
    // Explicitly 0755, because `create_dir_all` honours the umask — on a
    // umask-002 host it produces a group-writable parent, and the loader
    // refuses a providers.d whose ancestor can be replaced. postvec would
    // otherwise create a directory its own serving host then rejects.
    let mut missing: Vec<&Path> = Vec::new();
    let mut cursor = dir.parent();
    while let Some(ancestor) = cursor {
        if ancestor.exists() {
            break;
        }
        missing.push(ancestor);
        cursor = ancestor.parent();
    }
    for ancestor in missing.into_iter().rev() {
        std::fs::create_dir(ancestor)
            .map_err(|e| CliError::apply(format!("cannot create {}: {e}", ancestor.display())))?;
        std::fs::set_permissions(ancestor, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| CliError::apply(format!("cannot chmod {}: {e}", ancestor.display())))?;
        chown_if_root(ancestor, owner)?;
    }
    std::fs::create_dir_all(dir)
        .map_err(|e| CliError::apply(format!("cannot create {}: {e}", dir.display())))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| CliError::apply(format!("cannot chmod {}: {e}", dir.display())))?;
    chown_if_root(dir, owner)?;
    // And the result, now that it exists: the same predicate the serving host
    // will apply, before anything privileged is written into it.
    providers::config::validate_directory(dir, expected_uid).map_err(refuse)?;
    Ok(true)
}

/// Create `path`'s parent 0700 and write `path` 0600, atomically (write to a
/// sibling temp file, then rename), chowning both to `owner` when running
/// as root on their behalf.
///
/// Every step here is the way it is because this runs as **root**:
///
/// - the temporary file is `create_new` with `O_NOFOLLOW`. The previous
///   `create(true).truncate(true)` on a predictable `.NAME.tmp` was a
///   file-truncation primitive: in a providers.d another account could write
///   to, that account plants `.openai.toml.tmp` as a symlink to any
///   root-writable file and the next `sudo postvec provider add` truncates
///   it. `ensure_private_dir` now refuses such a directory, and this refuses
///   the symlink even if one appears anyway — two independent barriers,
///   because the cost of getting it wrong is somebody else's file.
/// - the destination is checked the same way, so a symlink or a hard-linked
///   `openai.toml` cannot redirect the rename either.
/// - `sync_all` must **succeed**, and the directory is synced after the
///   rename. `sync_all().ok()` meant "atomic" described only the rename and
///   not the data: a crash could leave a file this command already reported
///   as written.
// Same reason as `ProviderFileDoc::write`: the error is the state record.
#[allow(clippy::result_large_err)]
pub fn write_secret_file(
    path: &Path,
    body: &[u8],
    owner: Option<FileOwner>,
) -> std::result::Result<(), WriteError> {
    let staged = stage_secret_file(path, body, owner).map_err(|e| WriteError {
        committed: false,
        residue: e.residue,
        sync_error: e.sync_error,
        error: e.error,
    })?;
    if let Err(e) = failpoint("write-rename").and_then(|()| std::fs::rename(&staged.tmp, path)) {
        let cleanup = discard_staged(&staged.dir, &staged.tmp);
        return Err(WriteError {
            committed: false,
            error: CliError::apply(format!(
                "cannot move {} into place: {e}{cleanup}",
                staged.tmp.display()
            )),
            residue: cleanup.residue_path(),
            sync_error: cleanup.sync_error,
        });
    }
    sync_after("write-sync", &staged.dir, path).map_err(|e| WriteError {
        committed: true,
        residue: None,
        sync_error: None,
        error: e,
    })
}

/// A failed secret-file write, with everything a caller undoing a
/// multi-file change needs. `committed`: the new content is already in
/// place (a directory sync that fails *after* the rename is a durability
/// doubt, not an unchanged file — rolling a sibling back at that point
/// would leave the two files describing different states). When not
/// committed, `residue` names a staged copy of the content — which may
/// hold an inline API key — that could not be removed, and `sync_error`
/// a cleanup whose removal is visible but not crash-durable. Only
/// `{ committed: false, residue: None, sync_error: None }` means "exactly
/// as it was, durably".
#[derive(Debug)]
pub struct WriteError {
    pub committed: bool,
    pub residue: Option<PathBuf>,
    pub sync_error: Option<CliError>,
    pub error: CliError,
}

impl WriteError {
    /// Nothing changed, durably: an ordinary error is the whole story.
    pub fn is_clean(&self) -> bool {
        !self.committed && self.residue.is_none() && self.sync_error.is_none()
    }
}

impl From<WriteError> for CliError {
    fn from(e: WriteError) -> Self {
        e.error
    }
}

/// How [`install_secret_file`] may take the destination's place.
#[derive(Debug, Clone)]
pub enum InstallGuard {
    /// The destination must still be absent (`RENAME_NOREPLACE`).
    NoReplace,
    /// The destination must still be exactly the approved file: same
    /// identity, security metadata, length and digest. Exchanged
    /// atomically, verified, and exchanged back if it is not.
    Exact(ApprovedFile),
}

/// A file as an inspection approved it: identity, the whole security
/// predicate preflight applied, and content. Nothing weaker than this
/// decides whether a later step may touch the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedFile {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub sha256: [u8; 32],
    /// Permission bits (`& 0o777`).
    pub mode: u32,
    pub uid: u32,
}

impl ApprovedFile {
    /// The predicate over an opened descriptor and its bytes.
    pub fn capture(meta: &std::fs::Metadata, raw: &[u8]) -> Self {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            len: meta.len(),
            sha256: sha256(raw),
            mode: meta.permissions().mode() & 0o777,
            uid: meta.uid(),
        }
    }

    /// Whether `path` (opened without following links) is still this
    /// file: same identity, still a regular singly-linked private file
    /// with the approved mode and owner, same length and digest. Any
    /// change — a `chmod`, a `chown`, an added hard link, an edit — makes
    /// it a file this command did not approve.
    pub fn still_is(&self, path: &Path) -> Result<bool> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(CliError::apply(format!(
                    "cannot reopen {}: {e}",
                    path.display()
                )))
            }
        };
        let meta = file
            .metadata()
            .map_err(|e| CliError::apply(format!("cannot stat {}: {e}", path.display())))?;
        let same = meta.is_file()
            && meta.nlink() == 1
            && (meta.dev(), meta.ino(), meta.len()) == (self.dev, self.ino, self.len)
            && meta.permissions().mode() & 0o777 == self.mode
            && self.mode & 0o077 == 0
            && meta.uid() == self.uid;
        if !same {
            return Ok(false);
        }
        let mut raw = Vec::new();
        file.take(self.len + 1)
            .read_to_end(&mut raw)
            .map_err(|e| CliError::apply(format!("cannot read {}: {e}", path.display())))?;
        Ok(sha256(&raw) == self.sha256)
    }
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// Where an install left the filesystem when it failed — a durability
/// record, not a summary. "Not applied" means the destination is exactly
/// as it was *and* that state is crash-durable unless `sync_error` says
/// otherwise; "applied" means the new secret is at the destination.
/// `residue` / `displaced` name credential material left at a sibling
/// path; `sync_error` is the directory sync that failed, verbatim.
#[derive(Debug)]
pub enum InstallState {
    NotApplied {
        residue: Option<PathBuf>,
        sync_error: Option<CliError>,
    },
    Applied {
        installed: ApprovedFile,
        displaced: Option<PathBuf>,
        sync_error: Option<CliError>,
    },
}

#[derive(Debug)]
pub struct InstallError {
    pub state: InstallState,
    pub error: CliError,
}

impl From<InstallError> for CliError {
    fn from(e: InstallError) -> Self {
        e.error
    }
}

/// A staged secret: the destination's directory, the temp file beside it,
/// and the token the file will carry once renamed (a rename keeps the
/// inode).
struct Staged {
    dir: PathBuf,
    tmp: PathBuf,
    token: ApprovedFile,
}

/// A staging failure with its typed residue: whether a partially written
/// copy of the secret is still on disk, and whether the directory state
/// after cleanup is durable.
struct StageError {
    residue: Option<PathBuf>,
    sync_error: Option<CliError>,
    error: CliError,
}

/// The outcome of removing a credential-bearing temp file: what is left
/// (with the unlink error that left it), and whether the removal reached
/// disk.
struct Cleanup {
    residue: Option<(PathBuf, std::io::Error)>,
    sync_error: Option<CliError>,
}

impl Cleanup {
    fn describe(&self) -> String {
        let mut out = String::new();
        if let Some((residue, cause)) = &self.residue {
            out.push_str(&format!(
                "; the staged copy at {} could not be removed ({cause}) and holds the secret — \
                 remove it by hand",
                residue.display()
            ));
        }
        if let Some(e) = &self.sync_error {
            out.push_str(&format!("; {e}"));
        }
        out
    }

    fn residue_path(&self) -> Option<PathBuf> {
        self.residue.as_ref().map(|(p, _)| p.clone())
    }
}

impl std::fmt::Display for Cleanup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Remove a staged temp file and sync the directory, so a credential
/// never lingers unannounced and a removal the result reports cannot
/// reappear after a crash.
fn discard_staged(dir: &Path, tmp: &Path) -> Cleanup {
    match failpoint("stage-cleanup").and_then(|()| std::fs::remove_file(tmp)) {
        // Gone (by us, or already): the directory state is what must be
        // durable now.
        Ok(()) => Cleanup {
            residue: None,
            sync_error: sync_after("sync", dir, tmp).err(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Cleanup {
            residue: None,
            sync_error: sync_after("sync", dir, tmp).err(),
        },
        Err(e) => Cleanup {
            residue: Some((tmp.to_path_buf(), e)),
            sync_error: None,
        },
    }
}

// Fault injection at the named transitions of the two-file commit, as a
// set of `(point, occurrence)`: the N-th call of `point` fails. Selected
// from a test (`set_failpoints`) or, in debug builds only, from
// `POSTVEC_FAILPOINTS="point:N,point:N"` so the command-level path can be
// driven through the real binary. Release builds compile the whole thing
// to `Ok(())`: there is no way to fail a production install from outside.
#[cfg(debug_assertions)]
mod failpoints {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    thread_local! {
        static ARMED: RefCell<Vec<(String, usize)>> = const { RefCell::new(Vec::new()) };
        static SEEN: RefCell<BTreeMap<String, usize>> = const { RefCell::new(BTreeMap::new()) };
        static LOADED: RefCell<bool> = const { RefCell::new(false) };
    }

    #[cfg(test)]
    pub fn arm(points: Vec<(String, usize)>) {
        ARMED.with(|a| *a.borrow_mut() = points);
        SEEN.with(|s| s.borrow_mut().clear());
        LOADED.with(|l| *l.borrow_mut() = true);
    }

    fn load_env_once() {
        if LOADED.with(|l| std::mem::replace(&mut *l.borrow_mut(), true)) {
            return;
        }
        let armed = std::env::var("POSTVEC_FAILPOINTS")
            .ok()
            .into_iter()
            .flat_map(|v| v.split(',').map(str::to_string).collect::<Vec<_>>())
            .filter_map(|spec| {
                let (name, nth) = spec.split_once(':').unwrap_or((&spec, "1"));
                Some((name.trim().to_string(), nth.trim().parse().ok()?))
            })
            .collect();
        ARMED.with(|a| *a.borrow_mut() = armed);
    }

    pub fn hit(name: &str) -> bool {
        load_env_once();
        let nth = SEEN.with(|s| {
            let mut seen = s.borrow_mut();
            let n = seen.entry(name.to_string()).or_insert(0);
            *n += 1;
            *n
        });
        ARMED.with(|a| a.borrow().iter().any(|(p, n)| p == name && *n == nth))
    }
}

/// Arm a set of `(point, occurrence)` failures for this thread (tests).
#[cfg(test)]
pub(crate) fn set_failpoints(points: &[(&str, usize)]) {
    failpoints::arm(points.iter().map(|(p, n)| (p.to_string(), *n)).collect());
}

/// Arm one point's first occurrence, or clear everything with `None`.
#[cfg(test)]
pub(crate) fn set_failpoint(name: Option<&'static str>) {
    set_failpoints(&name.map(|n| vec![(n, 1)]).unwrap_or_default());
}

fn failpoint(name: &str) -> std::io::Result<()> {
    #[cfg(debug_assertions)]
    {
        if failpoints::hit(name) {
            return Err(std::io::Error::other(format!("injected failure at {name}")));
        }
    }
    let _ = name;
    Ok(())
}

/// [`sync_directory`] with a failpoint, for the install's transitions.
fn sync_after(point: &str, dir: &Path, changed: &Path) -> Result<()> {
    failpoint(point).map_err(|e| {
        CliError::apply(format!(
            "applied {} but could not flush {}: {e}; the change may not survive a crash",
            changed.display(),
            dir.display()
        ))
    })?;
    sync_directory(dir, changed)
}

/// [`write_secret_file`] bound to the state a preflight approved: the
/// destination is taken over only if it is still absent, or still exactly
/// the approved file. Nothing between preflight and this call — an
/// operator, the inference account, another tool — can make this command
/// replace a file it never inspected. Returns the approval token of the
/// installed file, which is what any later restore must be bound to.
//
// The error carries the installed file's token on purpose: a caller must
// be able to bind a rollback to it. Boxing it would only move the bytes.
#[allow(clippy::result_large_err)]
pub fn install_secret_file(
    path: &Path,
    body: &[u8],
    owner: Option<FileOwner>,
    guard: &InstallGuard,
) -> std::result::Result<ApprovedFile, InstallError> {
    use std::ffi::CString;
    let Staged { dir, tmp, token } =
        stage_secret_file(path, body, owner).map_err(|e| InstallError {
            state: InstallState::NotApplied {
                residue: e.residue,
                sync_error: e.sync_error,
            },
            error: e.error,
        })?;
    let c = |p: &Path| CString::new(p.as_os_str().as_encoded_bytes()).expect("no NUL in a path");
    let (c_tmp, c_path) = (c(&tmp), c(path));
    let rename =
        |point: &str, flags: libc::c_uint, from: &CString, to: &CString| -> std::io::Result<()> {
            failpoint(point)?;
            let rc = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    flags,
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        };
    // A refusal: the destination is untouched (or exchanged back); the
    // candidate is removed and that removal is synced.
    let refuse = |message: String| {
        let cleanup = discard_staged(&dir, &tmp);
        InstallError {
            error: CliError::precondition(format!("{}: {message}{cleanup}", path.display())),
            state: InstallState::NotApplied {
                residue: cleanup.residue_path(),
                sync_error: cleanup.sync_error,
            },
        }
    };
    let mut displaced: Option<(PathBuf, std::io::Error)> = None;
    match guard {
        InstallGuard::NoReplace => {
            if let Err(e) = rename("rename", libc::RENAME_NOREPLACE, &c_tmp, &c_path) {
                let why = if e.kind() == std::io::ErrorKind::AlreadyExists {
                    "appeared since it was checked; refusing to replace a file this command never inspected".to_string()
                } else {
                    format!("cannot move into place: {e}")
                };
                return Err(refuse(why));
            }
        }
        InstallGuard::Exact(approved) => {
            // Exchange atomically, then look at what we got. If it is not
            // the approved file, exchange back: the destination is exactly
            // as it was, and nothing of ours is in place.
            if let Err(e) = rename("exchange", libc::RENAME_EXCHANGE, &c_tmp, &c_path) {
                return Err(refuse(format!(
                    "cannot exchange into place ({e}); it may have been removed since it was checked"
                )));
            }
            match approved.still_is(&tmp) {
                Ok(true) => {}
                verdict => {
                    let problem = match verdict {
                        Ok(_) => "changed since it was checked; refusing to replace a file this command never inspected".to_string(),
                        Err(e) => e.to_string(),
                    };
                    return Err(
                        match rename("exchange-back", libc::RENAME_EXCHANGE, &c_tmp, &c_path) {
                            Ok(()) => refuse(problem),
                            // The candidate is at the destination and the
                            // displaced file beside it: applied, with residue.
                            Err(e) => {
                                let sync_error = sync_after("sync", &dir, path).err();
                                InstallError {
                                error: CliError::apply(format!(
                                    "{}: {problem}; and exchanging it back failed too ({e}): the new key \
                                     is in place and the previous file is at {}{}",
                                    path.display(),
                                    tmp.display(),
                                    sync_error.as_ref().map(|e| format!("; {e}")).unwrap_or_default()
                                )),
                                state: InstallState::Applied {
                                    installed: token,
                                    displaced: Some(tmp.clone()),
                                    sync_error,
                                },
                            }
                            }
                        },
                    );
                }
            }
            // The displaced old key. Failing to remove it is not a
            // success: credential material would stay at a predictable
            // sibling path.
            if let Err(e) = failpoint("displaced-unlink").and_then(|()| std::fs::remove_file(&tmp))
            {
                displaced = Some((tmp.clone(), e));
            }
        }
    }
    let sync_error = sync_after("sync", &dir, path).err();
    match (displaced, sync_error) {
        (None, None) => Ok(token),
        (displaced, sync_error) => {
            let mut message = format!("{} is in place", path.display());
            if let Some((tmp, e)) = &displaced {
                message.push_str(&format!(
                    ", but the previous key could not be removed from {} ({e}) — remove it by hand",
                    tmp.display()
                ));
            }
            if let Some(e) = &sync_error {
                message.push_str(&format!("; {e}"));
            }
            Err(InstallError {
                state: InstallState::Applied {
                    installed: token,
                    displaced: displaced.map(|(tmp, _)| tmp),
                    sync_error,
                },
                error: CliError::apply(message),
            })
        }
    }
}

/// The staged half of [`write_secret_file`]: the destination's shape
/// checks and a private, synced temp file beside it.
#[allow(clippy::result_large_err)]
fn stage_secret_file(
    path: &Path,
    body: &[u8],
    owner: Option<FileOwner>,
) -> std::result::Result<Staged, StageError> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let clean = |error: CliError| StageError {
        residue: None,
        sync_error: None,
        error,
    };

    let dir = path.parent().ok_or_else(|| {
        clean(CliError::internal(format!(
            "{} has no parent",
            path.display()
        )))
    })?;
    ensure_private_dir(dir, owner).map_err(clean)?;

    // An existing destination must be an ordinary, singly-linked file.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(clean(
                CliError::precondition(format!(
                    "{} is a symlink; refusing to write a credential through it",
                    path.display()
                ))
                .with_fix("remove the symlink and rerun"),
            ))
        }
        Ok(meta) if !meta.is_file() => {
            return Err(clean(CliError::precondition(format!(
                "{} exists and is not a regular file",
                path.display()
            ))))
        }
        Ok(meta) if meta.nlink() != 1 => {
            return Err(clean(CliError::precondition(format!(
                "{} has {} hard links; refusing to replace it",
                path.display(),
                meta.nlink()
            ))))
        }
        _ => {}
    }

    // A per-process temp name, not a shared one. A single predictable
    // `.NAME.tmp` meant concurrent writers could unlink each other's file
    // between create and rename; with the pid in the name they cannot
    // collide, and `create_new` refuses anything already sitting there rather
    // than removing a file this process did not create.
    let tmp = dir.join(format!(
        ".{}.{}.postvec.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| {
            clean(CliError::apply(format!(
                "cannot create {}: {e}",
                tmp.display()
            )))
        })?;

    // Everything below acts on the **descriptor**, never on the path, and in
    // this order: content, then metadata, then one sync that covers both.
    // Doing chmod/chown by path after the sync left a window where the file
    // could be replaced, and left the metadata unsynced.
    let prepared = (|| -> std::io::Result<std::fs::Metadata> {
        failpoint("stage-write")?;
        file.write_all(body)?;
        file.flush()?;
        // The `mode` on OpenOptions is masked by the umask, so set it again.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        if let Some(owner) = owner.filter(|_| crate::proc::is_root()) {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        }
        // The rename is only atomic with respect to a crash if the data and
        // the metadata are on disk first.
        file.sync_all()?;
        file.metadata()
    })();
    drop(file);
    let meta = match prepared {
        Ok(meta) => meta,
        Err(e) => {
            let cleanup = discard_staged(dir, &tmp);
            return Err(StageError {
                error: CliError::apply(format!("cannot write {}: {e}{cleanup}", tmp.display())),
                residue: cleanup.residue_path(),
                sync_error: cleanup.sync_error,
            });
        }
    };
    Ok(Staged {
        dir: dir.to_path_buf(),
        tmp,
        token: ApprovedFile::capture(&meta, body),
    })
}

/// Flush the directory after a rename or removal.
///
/// Errors propagate. A crash after "applied" with no dir fsync can lose
/// the change.
pub fn sync_directory(dir: &Path, changed: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(|e| {
            CliError::apply(format!(
                "applied {} but could not flush {}: {e}; the change may not survive a crash",
                changed.display(),
                dir.display()
            ))
        })
}

fn chown_if_root(path: &Path, owner: Option<FileOwner>) -> Result<()> {
    let Some(owner) = owner else { return Ok(()) };
    if !crate::proc::is_root() {
        return Ok(());
    }
    // `lchown`, not `chown`: this runs as root over paths in a directory the
    // command is still in the middle of establishing, and following a symlink
    // here would hand ownership of somebody else's file to the cluster owner.
    std::os::unix::fs::lchown(path, Some(owner.uid), Some(owner.gid))
        .map_err(|e| CliError::apply(format!("cannot chown {}: {e}", path.display())))
}

/// Refuse a referenced secret file that is missing or broader than 0600 —
/// the serve-path loader would refuse it too, so failing here is the same
/// rule applied earlier, with a better message.
pub fn require_private_secret_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        CliError::precondition(format!("cannot stat {}: {e}", path.display()))
            .with_fix("the key file must exist on the inference host before the provider serves")
    })?;
    if !meta.is_file() {
        return Err(CliError::precondition(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CliError::precondition(format!(
            "{} is readable by other users (mode {mode:o}); the host refuses such key files",
            path.display()
        ))
        .with_fix(format!(
            "chmod 600 {} (and rotate the key if others could have read it)",
            path.display()
        )));
    }
    Ok(())
}

/// The loader's ceiling on a referenced secret.
const MAX_SECRET_BYTES: u64 = 16 * 1024;

/// Read a referenced key file for the verification probe.
///
/// One `O_NOFOLLOW` open, then every check on the resulting descriptor, then
/// a bounded read from that same descriptor — the shape the serving host
/// uses, and for the same reason. `require_private_secret_file` followed by a
/// separate `read_to_string(path)` asked about one file and read another, and
/// this runs as root: winning that race means root reads a file the operator
/// did not name.
pub fn read_secret_file(path: &Path) -> Result<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => CliError::precondition(format!(
                "{} is a symlink; a key file must be a regular file (a symlink can point at a \
                 world-readable one)",
                path.display()
            )),
            _ => CliError::precondition(format!("cannot read {}: {e}", path.display())).with_fix(
                "the key file must exist on the inference host before the provider serves",
            ),
        })?;
    let meta = file
        .metadata()
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(CliError::precondition(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CliError::precondition(format!(
            "{} is readable by other users (mode {mode:o}); the host refuses such key files",
            path.display()
        ))
        .with_fix(format!(
            "chmod 600 {} (and rotate the key if others could have read it)",
            path.display()
        )));
    }
    if meta.size() > MAX_SECRET_BYTES {
        return Err(CliError::precondition(format!(
            "{} is {} bytes; the host refuses a secret over {MAX_SECRET_BYTES}",
            path.display(),
            meta.size()
        )));
    }
    let mut raw = String::new();
    // The `fstat` size check above already refuses an oversized file; the
    // `+ 1` keeps this reader the same shape as the loader's, so a file that
    // grows between the two calls is still refused rather than truncated.
    file.take(MAX_SECRET_BYTES + 1)
        .read_to_string(&mut raw)
        .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
    if raw.len() as u64 > MAX_SECRET_BYTES {
        return Err(CliError::precondition(format!(
            "{} exceeds {MAX_SECRET_BYTES} bytes; the host refuses such a secret",
            path.display()
        )));
    }
    let key = raw.trim().to_string();
    if key.is_empty() {
        return Err(CliError::precondition(format!(
            "{} is empty",
            path.display()
        )));
    }
    Ok(key)
}

/// Resolve the secret a provider file records, for the verification probe.
/// An `_env` source resolves from *this* shell — the natural place a
/// verifying operator has the key; the serving host resolves its own copy.
pub fn resolve_doc_secret(doc: &ProviderFileDoc) -> Result<String> {
    let field = |name: &str| {
        doc.value
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    for inline in ["api_key", "bearer_token"] {
        if let Some(value) = field(inline) {
            return Ok(value);
        }
    }
    for file in ["api_key_file", "bearer_token_file"] {
        if let Some(path) = field(file) {
            return read_secret_file(Path::new(&path));
        }
    }
    for env in ["api_key_env", "bearer_token_env"] {
        if let Some(var) = field(env) {
            return std::env::var(&var).map_err(|_| {
                CliError::precondition(format!(
                    "the file's key source ({var}) is not set in this shell, so the key \
                     cannot be verified from here"
                ))
                .with_fix("export it in this shell for the probe")
            });
        }
    }
    Err(CliError::precondition(
        "the provider file has no key source (api_key / api_key_file / api_key_env, or the \
         AWS bearer/SigV4 fields)",
    ))
}

/// One live single-input embed. Costs a paid API call.
///
/// Shared by `provider add` and `provider test` so the two cannot disagree
/// about what a good answer looks like — `test` had its own copy that
/// accepted "the first vector, whatever it is", and would therefore report
/// success for a response the serving gateway rejects.
///
/// The response contract is the serving gateway's, applied here: **exactly
/// one** vector, reported for input `0`, non-empty. Accepting "the first
/// vector, whatever it is" meant the probe passed against a provider whose
/// response was already the shape that would dead-letter every batch later.
pub async fn probe_one(
    config: &providers::ProviderConfig,
    id: &str,
    declared_dim: Option<u32>,
    timeout: std::time::Duration,
) -> Result<u32> {
    // Before spending anything: the loader's own per-model rule. The factory
    // knows nothing about per-model contracts, so without this a descriptor
    // naming a Gemini model postvec cannot serve — or a Cohere width the
    // model does not produce — was probed, billed, and only then refused at
    // the write.
    providers::config::validate_model_for_provider(&config.provider, id, declared_dim).map_err(
        |e| {
            CliError::precondition(format!("{id}: {e}"))
                .with_fix("the serving host would refuse this model, so there is nothing to verify")
        },
    )?;
    let backend = providers::new_embedding_backend(
        config,
        id,
        declared_dim.unwrap_or(0) as i32,
        "search_document",
        None,
    )
    .map_err(|e| CliError::precondition(format!("{id}: {e}")))?;
    let deadline = std::time::Instant::now() + timeout;
    let embeddings = backend
        .embed(&["postvec verification probe"], Some(deadline))
        .await
        .map_err(|e| {
            CliError::precondition(format!("verification embed for {id} failed: {e}")).with_fix(
                "check the key, model id and network; pass --no-verify to write the file \
                 anyway (the probe costs one paid API call per model)",
            )
        })?;
    if embeddings.len() != 1 {
        return Err(CliError::precondition(format!(
            "verification embed for {id} returned {} vectors for one input; the serving host \
             refuses a response that is not row-parallel to the request",
            embeddings.len()
        )));
    }
    let embedding = &embeddings[0];
    if embedding.text_index != 0 {
        return Err(CliError::precondition(format!(
            "verification embed for {id} reported its vector as input {} of one; the serving \
             host refuses a response that is not row-parallel to the request",
            embedding.text_index
        )));
    }
    if embedding.vector.is_empty() {
        return Err(CliError::precondition(format!(
            "verification embed for {id} returned an empty vector"
        )));
    }
    Ok(embedding.vector.len() as u32)
}

/// One live single-vector conversion. Costs a paid API call.
///
/// The convert sibling of [`probe_one`], shared by `provider add` and
/// `provider test` for the same reason those share the embed probe. The
/// response contract is the serving gateway's, applied here: exactly one
/// vector, non-empty; its width is the measured target dimension the caller
/// compares (or patches) against the declared one. The probe input is a unit
/// basis vector — finite and non-zero, because a zero vector cannot survive
/// the re-normalisation some conversion pipelines apply.
pub async fn probe_convert_one(
    config: &providers::ProviderConfig,
    model: &providers::config::ModelDescriptor,
    timeout: std::time::Duration,
) -> Result<u32> {
    // Before spending anything: the loader's own converter rule, so a paid
    // call is never made for an entry the serving host would refuse.
    providers::config::validate_converter_for_provider(&config.provider, model).map_err(|e| {
        CliError::precondition(format!("{}: {e}", model.name))
            .with_fix("the serving host would refuse this entry, so there is nothing to verify")
    })?;
    let backend = providers::new_conversion_backend(
        config,
        model.provider_source_id.as_deref().unwrap_or(""),
        &model.provider_model_id,
        model.dim,
        None,
    )
    .map_err(|e| CliError::precondition(format!("{}: {e}", model.name)))?;
    // `validate_converter_for_provider` guarantees source_dim >= 1.
    let mut probe = vec![0.0_f32; model.source_dim.unwrap_or(1) as usize];
    probe[0] = 1.0;
    let deadline = std::time::Instant::now() + timeout;
    let vectors = backend
        .convert(&[probe], Some(deadline))
        .await
        .map_err(|e| {
            CliError::precondition(format!(
                "verification convert for {} failed: {e}",
                model.name
            ))
            .with_fix(
                "check the key, the provider-side model ids and --source-dim; pass --no-verify \
                 to write the file anyway (the probe costs one paid API call)",
            )
        })?;
    if vectors.len() != 1 {
        return Err(CliError::precondition(format!(
            "verification convert for {} returned {} vectors for one input; the serving host \
             refuses a response that is not row-parallel to the request",
            model.name,
            vectors.len()
        )));
    }
    if vectors[0].is_empty() {
        return Err(CliError::precondition(format!(
            "verification convert for {} returned an empty vector",
            model.name
        )));
    }
    Ok(vectors[0].len() as u32)
}

// ---- Reload client -------------------------------------------------------

/// What `POST /admin/providers/reload` reported.
#[derive(Debug, Deserialize)]
pub struct ReloadOutcome {
    /// The providers.d directory that host actually reads. `None` from a
    /// host older than this field; the caller then falls back to trusting
    /// whichever listener answered, which is what it did before.
    #[serde(default)]
    pub path: Option<String>,
    pub providers: usize,
    pub models: usize,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReloadEnvelope {
    success: bool,
    #[serde(default)]
    data: Option<ReloadOutcome>,
    #[serde(default)]
    error: Option<String>,
}

pub async fn post_reload(listen: &str, timeout: std::time::Duration) -> Result<ReloadOutcome> {
    let url = format!("http://{listen}/admin/providers/reload");
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(std::time::Duration::from_secs(5)))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CliError::internal(format!("cannot build HTTP client: {e}")))?;
    let response = client.post(&url).send().await.map_err(|e| {
        CliError::precondition(format!("cannot reach the inference host at {listen}: {e}"))
    })?;
    let status = response.status();
    let body: ReloadEnvelope = response
        .json()
        .await
        .map_err(|e| CliError::precondition(format!("{listen} answered malformed JSON: {e}")))?;
    if !status.is_success() || !body.success {
        return Err(CliError::precondition(format!(
            "the host at {listen} refused the provider reload: {}",
            body.error.unwrap_or_else(|| format!("HTTP {status}"))
        )));
    }
    body.data
        .ok_or_else(|| CliError::precondition(format!("{listen} answered without a report")))
}

/// Nudge the serving host to reload after files changed. Best effort by
/// design — a restart also picks changes up — but the outcome is always in
/// the journal, never silently assumed.
pub async fn reload_host(
    target: &ProviderTarget,
    timeout: std::time::Duration,
    journal: &mut ApplyJournal,
) {
    let candidates: Vec<String> = match target {
        ProviderTarget::Embedded { settings, .. } => vec![settings.embedded_http_listen()],
        // A --path root belongs to some host this CLI was not told about.
        // Try the two loopback defaults — the postvec-server admin port
        // first (--path is the remote-node administration story), then the
        // embedded listener — rather than demanding a new flag for the
        // common case of running on the node itself.
        ProviderTarget::Path { .. } => vec![
            POSTVEC_SERVER_ADMIN.to_string(),
            crate::config::DEFAULT_EMBEDDED_HTTP_LISTEN.to_string(),
        ],
    };
    let wanted = target.dir();
    for listen in &candidates {
        match post_reload(listen, timeout).await {
            Ok(outcome) => {
                // A host that reads a *different* providers.d did not apply
                // this change. Saying "reloaded" there would be the worst
                // kind of wrong: the operator would believe the files are
                // live. This is reachable in the ordinary way — `--path
                // <server-root>` on a machine that also runs an embedded
                // cluster, where the node is down and the embedded host
                // answers instead.
                if let Some(served) = &outcome.path {
                    if Path::new(served) != wanted {
                        journal.record(format!(
                            "the host at {listen} reads {served}, not {}; it was not asked \
                             again",
                            wanted.display()
                        ));
                        continue;
                    }
                }
                journal.record(format!(
                    "reloaded the host at {listen}: {} provider(s), {} model(s) now served",
                    outcome.providers, outcome.models
                ));
                for error in &outcome.errors {
                    journal.incomplete(format!(
                        "provider file problem reported by the host: {error}"
                    ));
                }
                return;
            }
            Err(_) if candidates.len() > 1 => continue,
            Err(e) => {
                unreachable_host(target, journal, &format!("{e}"));
                return;
            }
        }
    }
    unreachable_host(
        target,
        journal,
        &format!(
            "no inference host reading {} answered a provider reload on {}",
            wanted.display(),
            candidates.join(" or ")
        ),
    );
}

/// A host that did not answer the reload. On a **cluster** target the CLI
/// just connected to that cluster, so a live engine was expected and its
/// absence leaves the job half-done (partial). On a `--path` root the CLI
/// was never told which host owns it — provisioning files for a node that
/// is not running here is the ordinary case — so this is a note, not a
/// partial result. Either way the files are written and a restart applies
/// them.
fn unreachable_host(target: &ProviderTarget, journal: &mut ApplyJournal, detail: &str) {
    let tail = "the files are written — the serving host picks them up at its next restart, or \
                POST /admin/providers/reload on its loopback admin listener";
    match target {
        ProviderTarget::Embedded { .. } => journal.incomplete(format!("{detail}; {tail}")),
        ProviderTarget::Path { .. } => journal.record(format!("{detail}; {tail}")),
    }
}

/// Serialize provider-file mutations against other `postvec` commands on this
/// host.
///
/// `provider add` and `provider rm` are read-modify-write over a TOML
/// document: two concurrent runs both load the file, both append, and the
/// second rename silently discards the first one's model. The lock is the
/// same advisory `flock` the `model` family already uses — held only for the
/// life of the process, so a killed command releases it and there is no stale
/// lock to clean up, which is why the earlier decision to skip it was wrong.
///
/// The lock file sits beside the connector files rather than inside them:
/// `.lock` is not `*.toml`, so the loader never sees it, and a `--path` root
/// on a server node gets the same protection as a cluster's providers.d.
pub fn lock_provider_dir(
    dir: &Path,
    owner: Option<FileOwner>,
) -> Result<crate::config::owned::HostLock> {
    ensure_private_dir(dir, owner)?;
    let lock = crate::config::owned::HostLock::acquire_labeled(
        &dir.join(".lock"),
        "this providers.d directory",
    )?;
    chown_if_root(&dir.join(".lock"), owner)?;
    Ok(lock)
}

/// Refresh `postvec.models` in every configured database after a successful
/// provider change.
///
/// Without this, `provider add` reported success and `enable()` on the new
/// name failed until the worker's next discovery cycle — up to
/// `postvec.model_refresh_interval_ms` (60 s by default) plus jitter — because
/// `resolve_embed_route` refuses a name the cache does not carry. The
/// documented quick start is `provider add` then `enable`, back to back.
/// Removal has the mirror problem: a stale route stays selectable and
/// produces avoidable model-not-found retries.
///
/// This is deliberately the same shape as the `model` family's
/// `refresh_databases` — the same journal vocabulary, the same
/// per-database isolation — because it is the same operation. A `--path`
/// target has no cluster and is left alone; the caller says so.
pub async fn refresh_databases(target: &mut ProviderTarget, journal: &mut ApplyJournal) {
    let ProviderTarget::Embedded {
        context, settings, ..
    } = target
    else {
        return;
    };
    let databases = settings.configured_databases();
    if databases.is_empty() {
        return;
    }
    let Some(context) = context.as_mut() else {
        journal.incomplete(
            "could not refresh postvec.models: no database connection (the worker picks the \
             change up on its next discovery cycle)"
                .to_string(),
        );
        return;
    };
    for database in databases {
        match context.db.refresh_models(&database).await {
            Ok(count) => journal.record(format!("refreshed {database}: {count} models in cache")),
            Err(e) => journal.incomplete(format!(
                "could not refresh postvec.models in {database}: {e} (the worker refreshes \
                 automatically on its next cycle, so this is a delay rather than a failure)"
            )),
        }
    }
}

// ---- The privacy / in-use scan -------------------------------------------

/// Every managed column bound to one of `names`, across every configured
/// database, excluding names the running host already serves as a **local**
/// embed model (local wins the collision, so nothing changes for those).
/// A database that cannot be inspected is reported as unknown, never as
/// clean; an unreachable host is treated as "no local model wins" — the
/// conservative direction for a privacy gate.
pub async fn columns_bound_to(
    target: &mut ProviderTarget,
    names: &[String],
    timeout: std::time::Duration,
) -> (Vec<crate::plan::InUseColumn>, Vec<String>) {
    let ProviderTarget::Embedded {
        settings, context, ..
    } = target
    else {
        return (Vec::new(), Vec::new());
    };
    let databases = settings.configured_databases();
    let listen = settings.embedded_http_listen();
    let Some(context) = context.as_mut() else {
        return (Vec::new(), databases);
    };

    // What the running host serves from its own engine right now: an enabled
    // LOCAL embed model under a colliding name keeps that name local,
    // so its columns are not affected either way.
    //
    // `m.provider.is_none()` is load-bearing. `/config` carries provider
    // descriptors in the same list, so without it a name already served by
    // some *other* provider file would count as "local wins" and its columns
    // would be dropped from the privacy warning — for a change that does
    // move their text, from one provider to another.
    let locally_served: std::collections::BTreeSet<String> =
        match crate::commands::model::admin::loaded_inventory(&listen, timeout).await {
            Some(inventory) => inventory
                .models
                .iter()
                .filter(|m| {
                    m.enabled && m.model_type.as_deref() == Some("embed") && m.provider.is_none()
                })
                .map(|m| m.name.clone())
                .collect(),
            None => Default::default(),
        };

    let mut columns = Vec::new();
    let mut unknown = Vec::new();
    for database in databases {
        let facts = match context.db.inspect_database(&database).await {
            Ok(facts) => facts,
            Err(_) => {
                unknown.push(database);
                continue;
            }
        };
        if facts.unreachable.is_some() {
            unknown.push(database);
            continue;
        }
        if !facts.exists || facts.postvec.is_none() {
            continue;
        }
        for entry in &facts.registry {
            let Some(name) = names.iter().find(|name| **name == entry.model) else {
                continue;
            };
            if locally_served.contains(name) {
                continue;
            }
            columns.push(crate::plan::InUseColumn {
                model: name.clone(),
                declared_model: entry.model.clone(),
                database: database.clone(),
                relation: entry.relation.clone(),
                column: entry.source_column.clone(),
                state: entry.state.clone(),
            });
        }
    }
    (columns, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guarded install takes the destination only in the state preflight
    /// approved: a file that appeared is never replaced; a file that was
    /// swapped, edited in place, re-permissioned or hard-linked is never
    /// replaced and is left exactly where it was; the approved file itself
    /// is. The returned token binds a later rollback the same way.
    #[test]
    fn a_guarded_install_never_replaces_what_preflight_did_not_see() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("k.key");
        let entries = || std::fs::read_dir(dir.path()).unwrap().count();
        let refused = |guard: &InstallGuard, expect: &str| {
            let err = install_secret_file(&path, b"two", None, guard).unwrap_err();
            assert!(
                matches!(
                    err.state,
                    InstallState::NotApplied {
                        residue: None,
                        sync_error: None
                    }
                ),
                "{err:?}"
            );
            assert!(err.error.to_string().contains(expect), "{err:?}");
            assert_eq!(entries(), 1, "no temp file left");
        };

        // NoReplace: absent → installed; present → refused, intact.
        let token = install_secret_file(&path, b"one", None, &InstallGuard::NoReplace).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one");
        assert!(
            token.still_is(&path).unwrap(),
            "the token describes the installed file"
        );
        refused(&InstallGuard::NoReplace, "appeared");
        assert_eq!(std::fs::read(&path).unwrap(), b"one");

        // Exact: not after a swap, an edit, a chmod or an extra link…
        // (A swap for a file with the same bytes, mode and owner that
        // happens to reuse the inode is indistinguishable, and replacing it
        // is equivalent; the swap here carries different bytes.)
        let exact = InstallGuard::Exact(token.clone());
        std::fs::remove_file(&path).unwrap();
        write_secret_file(&path, b"uno", None).unwrap();
        refused(&exact, "changed since");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"uno",
            "the substitute is untouched"
        );
        std::fs::write(&path, b"one").unwrap();
        let token = install_secret_file(
            &path,
            b"one",
            None,
            &InstallGuard::Exact(ApprovedFile::capture(
                &std::fs::metadata(&path).unwrap(),
                b"one",
            )),
        )
        .unwrap();
        let exact = InstallGuard::Exact(token.clone());
        std::fs::write(&path, b"one-edited").unwrap();
        refused(&exact, "changed since");
        std::fs::write(&path, b"one").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        refused(&exact, "changed since");
        assert!(
            !token.still_is(&path).unwrap(),
            "a chmod fails the predicate"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("k.link");
        std::fs::hard_link(&path, &link).unwrap();
        assert!(
            !token.still_is(&path).unwrap(),
            "an added link fails the predicate"
        );
        let err = install_secret_file(&path, b"two", None, &exact).unwrap_err();
        assert!(
            matches!(err.state, InstallState::NotApplied { .. }),
            "{err:?}"
        );
        std::fs::remove_file(&link).unwrap();
        // …but the approved file itself is replaced, leaving no residue.
        assert!(token.still_is(&path).unwrap());
        let rotated = install_secret_file(&path, b"two", None, &exact).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(rotated.still_is(&path).unwrap());
        assert_eq!(entries(), 1);
    }

    /// Every failing transition of the install, driven through the
    /// failpoints, leaves a truthful `InstallState`: what is at the
    /// destination, what credential material is beside it, and whether the
    /// directory sync that would make it durable succeeded.
    #[test]
    #[allow(clippy::result_large_err)]
    fn every_install_transition_reports_its_state_truthfully() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("k.key");
        let tmp_of = || {
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .find(|p| p.to_string_lossy().contains(".postvec.tmp"))
        };
        let install =
            |guard: &InstallGuard, body: &[u8]| install_secret_file(&path, body, None, guard);
        let reset = |bytes: &[u8]| {
            let _ = std::fs::remove_file(&path);
            for p in std::fs::read_dir(dir.path()).unwrap().flatten() {
                let _ = std::fs::remove_file(p.path());
            }
            set_failpoint(None);
            install(&InstallGuard::NoReplace, bytes).unwrap()
        };

        // Staging write fails: nothing applied, the partial copy removed and
        // the removal synced.
        set_failpoint(Some("stage-write"));
        let err = install(&InstallGuard::NoReplace, b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: None
                }
            ),
            "{err:?}"
        );
        assert!(tmp_of().is_none() && !path.exists());
        // Staging cleanup fails: the residue is named in the state and the message.
        set_failpoint(Some("stage-cleanup"));
        let _ = install(&InstallGuard::NoReplace, b"new"); // stage ok, rename ok → no cleanup
        let _ = std::fs::remove_file(&path);
        set_failpoint(Some("stage-cleanup"));
        std::fs::write(&path, b"blocker").unwrap();
        let err = install(&InstallGuard::NoReplace, b"new").unwrap_err();
        match &err.state {
            InstallState::NotApplied {
                residue: Some(r), ..
            } => assert!(
                r.exists() && err.error.to_string().contains("could not be removed"),
                "{err:?}"
            ),
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_file(tmp_of().unwrap());

        // Post-rename sync fails: applied, not durable, the error kept.
        let _ = std::fs::remove_file(&path);
        set_failpoint(Some("sync"));
        let err = install(&InstallGuard::NoReplace, b"new").unwrap_err();
        match &err.state {
            InstallState::Applied {
                installed,
                displaced: None,
                sync_error: Some(e),
            } => {
                assert!(installed.still_is(&path).unwrap());
                assert!(e.to_string().contains("injected failure at sync"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        // Rotation: displaced-key unlink fails → applied, displaced named.
        let token = reset(b"old");
        set_failpoint(Some("displaced-unlink"));
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        match &err.state {
            InstallState::Applied {
                displaced: Some(d),
                sync_error: None,
                ..
            } => {
                assert_eq!(
                    std::fs::read(d).unwrap(),
                    b"old",
                    "the old key is where the state says"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        // Rotation refused (destination changed) with a failing exchange-back:
        // applied, the displaced file retained, sync attempted and recorded.
        let token = reset(b"old");
        std::fs::write(&path, b"old-edited").unwrap();
        set_failpoint(Some("exchange-back"));
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        match &err.state {
            InstallState::Applied {
                displaced: Some(d),
                sync_error: None,
                ..
            } => {
                assert_eq!(std::fs::read(d).unwrap(), b"old-edited");
                assert!(
                    err.error.to_string().contains("exchanging it back failed"),
                    "{err:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"new");

        // The rename and exchange transitions themselves: nothing applied,
        // the candidate removed and that removal synced.
        if let Some(t) = tmp_of() {
            std::fs::remove_file(t).unwrap();
        }
        let _ = std::fs::remove_file(&path);
        set_failpoint(Some("rename"));
        let err = install(&InstallGuard::NoReplace, b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: None
                }
            ),
            "{err:?}"
        );
        assert!(!path.exists() && tmp_of().is_none());
        let token = reset(b"old");
        set_failpoint(Some("exchange"));
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: None
                }
            ),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert!(tmp_of().is_none());

        // Compound: exchange-back fails AND the directory sync fails —
        // applied, displaced named, the sync error kept alongside.
        let token = reset(b"old");
        std::fs::write(&path, b"old-edited").unwrap();
        set_failpoints(&[("exchange-back", 1), ("sync", 1)]);
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        match &err.state {
            InstallState::Applied {
                displaced: Some(d),
                sync_error: Some(e),
                ..
            } => {
                assert_eq!(std::fs::read(d).unwrap(), b"old-edited");
                assert!(e.to_string().contains("injected failure at sync"), "{e}");
                assert!(
                    err.error.to_string().contains("exchanging it back failed")
                        && err.error.to_string().contains("injected failure at sync"),
                    "{err:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        // Compound: refusal cleanup fails — the unlink cause is in the
        // message, and no sync is claimed for a removal that did not happen.
        let _ = std::fs::remove_file(tmp_of().unwrap());
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"blocker").unwrap();
        set_failpoints(&[("stage-cleanup", 1)]);
        let err = install(&InstallGuard::NoReplace, b"new").unwrap_err();
        match &err.state {
            InstallState::NotApplied {
                residue: Some(r),
                sync_error: None,
            } => {
                assert!(r.exists());
                assert!(
                    err.error
                        .to_string()
                        .contains("injected failure at stage-cleanup"),
                    "{err:?}"
                );
            }
            other => panic!("{other:?}"),
        }
        let _ = std::fs::remove_file(tmp_of().unwrap());
        // The second occurrence of a point, not the first.
        let token = reset(b"old");
        std::fs::write(&path, b"old-edited").unwrap();
        set_failpoints(&[("sync", 2)]);
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: None
                }
            ),
            "first sync passes: {err:?}"
        );
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: Some(_)
                }
            ),
            "second sync fails: {err:?}"
        );
        set_failpoint(None);

        // The plain connector write has its own two points, and carries
        // its cleanup state exactly like the key install.
        let connector = dir.path().join("c.toml");
        set_failpoint(Some("write-rename"));
        let err = write_secret_file(&connector, b"x", None).unwrap_err();
        assert!(err.is_clean() && !connector.exists(), "{err:?}");
        set_failpoints(&[("write-rename", 1), ("stage-cleanup", 1)]);
        let err = write_secret_file(&connector, b"x", None).unwrap_err();
        assert!(
            !err.committed && err.residue.as_ref().is_some_and(|r| r.exists()),
            "{err:?}"
        );
        assert!(
            err.error
                .to_string()
                .contains("injected failure at stage-cleanup"),
            "{err:?}"
        );
        std::fs::remove_file(err.residue.unwrap()).unwrap();
        set_failpoints(&[("stage-write", 1), ("sync", 1)]);
        let err = write_secret_file(&connector, b"x", None).unwrap_err();
        assert!(
            !err.committed && err.residue.is_none() && err.sync_error.is_some(),
            "{err:?}"
        );
        assert!(!connector.exists());
        set_failpoint(Some("write-sync"));
        let err = write_secret_file(&connector, b"x", None).unwrap_err();
        assert!(err.committed && connector.exists(), "{err:?}");
        set_failpoint(None);

        // Rotation refused cleanly: not applied, destination as it was,
        // candidate removed, removal synced — or the sync error recorded.
        let token = reset(b"old");
        std::fs::write(&path, b"old-edited").unwrap();
        set_failpoint(None);
        let err = install(&InstallGuard::Exact(token.clone()), b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: None
                }
            ),
            "{err:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old-edited");
        assert!(tmp_of().is_none());
        set_failpoint(Some("sync"));
        let err = install(&InstallGuard::Exact(token), b"new").unwrap_err();
        assert!(
            matches!(
                err.state,
                InstallState::NotApplied {
                    residue: None,
                    sync_error: Some(_)
                }
            ),
            "{err:?}"
        );
        set_failpoint(None);
    }
    use std::os::unix::fs::PermissionsExt;

    /// The name becomes `<providers.d>/<name>.toml` in every command, and
    /// `rm` deletes that path. A traversing name must never get that far.
    #[test]
    fn provider_names_cannot_escape_the_directory() {
        assert!(validate_provider_name("openai", "NAME").is_ok());
        assert!(validate_provider_name("openai-eu", "NAME").is_ok());
        for bad in [
            "",
            "../../etc/shadow",
            "..",
            "/etc/passwd",
            ".hidden",
            "Open AI",
            "a/b",
        ] {
            assert!(validate_provider_name(bad, "NAME").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_path_that_already_is_a_providers_d_is_used_as_is() {
        assert_eq!(
            providers_dir_from_path(Path::new("/var/lib/postvec-server")),
            PathBuf::from("/var/lib/postvec-server/providers.d")
        );
        assert_eq!(
            providers_dir_from_path(Path::new("/etc/postvec/providers.d")),
            PathBuf::from("/etc/postvec/providers.d")
        );
    }

    /// The packages ship no providers.d; the CLI is what brings it into
    /// existence, 0700, and it never re-permissions one an operator already
    /// has.
    #[test]
    fn ensure_private_dir_creates_0700_once_and_leaves_an_existing_one_alone() {
        let root = tempfile::tempdir().unwrap();
        // `tempfile` honours the umask; the loader refuses a providers.d whose
        // ancestor is group-writable, and a real `/etc/postvec` is not.
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = root.path().join("postvec/providers.d");

        assert!(ensure_private_dir(&dir, None).unwrap(), "created");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // The parent is an ordinary directory: only the leaf holds secrets.
        assert_ne!(
            std::fs::metadata(dir.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(!ensure_private_dir(&dir, None).unwrap(), "already there");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o750,
            "an existing directory is the operator's to permission"
        );
    }

    #[test]
    fn provider_files_round_trip_and_report_the_key_source_never_the_key() {
        let dir = tempfile::tempdir().unwrap();
        // A providers.d is 0700; `write` refuses a group/world-writable one,
        // and `tempfile` honours the umask.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("openai.toml");
        let doc = ProviderFileDoc {
            path: path.clone(),
            value: toml::toml! {
                provider = "openai"
                api_key = "sk-super-secret"

                [[models]]
                name = "openai-text-embedding-3-small"
                provider_model_id = "text-embedding-3-small"
                dim = 1536
            }
            .into(),
        };
        doc.write(None).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let mut loaded = ProviderFileDoc::load(&path).unwrap().unwrap();
        assert_eq!(loaded.provider_type(), Some("openai"));
        assert_eq!(
            loaded.key_source(),
            "inline (redacted)",
            "the source, never the key"
        );
        assert!(!loaded.key_source().contains("sk-super-secret"));
        assert_eq!(loaded.models().len(), 1);

        // Adding a model preserves the rest of the document.
        loaded
            .push_model(
                toml::toml! {
                    name = "openai-text-embedding-3-large"
                    provider_model_id = "text-embedding-3-large"
                    dim = 3072
                }
                .into(),
            )
            .unwrap();
        loaded.write(None).unwrap();
        let reloaded = ProviderFileDoc::load(&path).unwrap().unwrap();
        assert_eq!(reloaded.models().len(), 2);
        assert_eq!(
            reloaded.value.get("api_key").and_then(toml::Value::as_str),
            Some("sk-super-secret"),
            "the existing key survives an append"
        );

        // Removing by provider id or public name both work.
        let mut doc = reloaded;
        let (removed, remaining) = doc.remove_model("text-embedding-3-large");
        assert!(removed);
        assert_eq!(remaining, 1);
        let (removed, remaining) = doc.remove_model("openai-text-embedding-3-small");
        assert!(removed);
        assert_eq!(remaining, 0);
        let (removed, _) = doc.remove_model("never-there");
        assert!(!removed);
    }

    #[test]
    fn key_sources_cover_the_aws_variants() {
        let doc = |body: toml::Value| ProviderFileDoc {
            path: PathBuf::from("/x/aws.toml"),
            value: body,
        };
        assert_eq!(
            doc(
                toml::toml! { provider = "aws" bearer_token_env = "AWS_BEARER_TOKEN_BEDROCK" }
                    .into()
            )
            .key_source(),
            "bearer env:AWS_BEARER_TOKEN_BEDROCK"
        );
        assert_eq!(
            doc(
                toml::toml! { provider = "aws" access_key_id = "AKIA" secret_access_key = "s" }
                    .into()
            )
            .key_source(),
            "aws sigv4 (redacted)"
        );
        assert_eq!(
            doc(
                toml::toml! { provider = "openai" api_key_file = "/etc/postvec/keys/o.key" }.into()
            )
            .key_source(),
            "file:/etc/postvec/keys/o.key"
        );
        assert_eq!(
            doc(toml::toml! { provider = "openai" }.into()).key_source(),
            "none"
        );
    }

    #[test]
    fn referenced_secret_files_must_be_private() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("k.key");
        std::fs::write(&key, "sk").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o640)).unwrap();
        let err = require_private_secret_file(&key).unwrap_err();
        assert!(err.to_string().contains("readable by other users"), "{err}");

        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(require_private_secret_file(&key).is_ok());

        let missing = dir.path().join("gone.key");
        assert!(require_private_secret_file(&missing).is_err());
    }

    #[test]
    fn secret_writes_create_a_private_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let nested = dir.path().join("providers.d").join("openai.toml");
        write_secret_file(&nested, b"provider = \"openai\"\n", None).unwrap();
        assert_eq!(
            std::fs::metadata(nested.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// One rulebook for route identity: the ids `route_models()` reads from
    /// the document must be byte-equal to the `ServedBy.model_id` the
    /// loader's `served_names_if` computes for the same body. `provider rm`
    /// compares one against the other whenever no host answers — a converter
    /// whose two derivations disagreed would report a partial removal as a
    /// handoff to its own file.
    #[test]
    fn route_models_matches_the_loaders_served_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("univec.toml");
        let body = "provider = \"univec\"\napi_key = \"uv\"\n\n\
             [[models]]\nname = \"univec-arctic\"\n\
             provider_model_id = \"snowflake-arctic-embed-l-v2.0\"\ndim = 1024\n\n\
             [[models]]\nname = \"univec-convert-a-to-b\"\nkind = \"convert\"\n\
             provider_model_id = \"target-space\"\nprovider_source_id = \"source-space\"\n\
             source_model = \"model-a\"\ntarget_model = \"model-b\"\n\
             source_dim = 1536\ndim = 768\n";
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let doc = ProviderFileDoc::load(&path).unwrap().unwrap();
        let served = providers::config::served_names_if(dir.path(), &path, Some(body)).unwrap();
        let from_doc: std::collections::BTreeMap<String, String> =
            doc.route_models().into_iter().collect();
        assert_eq!(from_doc.len(), 2);
        for (name, by) in &served {
            assert_eq!(
                from_doc.get(name),
                Some(&by.model_id),
                "route id for {name} diverged between the document reader and the loader"
            );
        }
        // And the converter's id is the folded route, not the raw target id.
        assert_eq!(
            from_doc.get("univec-convert-a-to-b").unwrap(),
            "source-space->target-space for model-a[1536]->model-b"
        );
    }

    /// The convert probe applies the loader's rules before spending a call,
    /// measures the target dimension from the response, and holds it to the
    /// gateway's response contract.
    #[tokio::test]
    async fn probe_convert_one_measures_and_validates() {
        let mock = providers::testing::always(
            200,
            r#"{"success":true,"data":{"embeddings":[[0.1,0.2,0.3]]}}"#,
        )
        .await;
        let config = providers::ProviderConfig {
            provider: "univec".to_string(),
            api_key: Some("uv-test".to_string()),
            base_url: Some(mock.url.clone()),
            ..Default::default()
        };
        let descriptor: providers::config::ModelDescriptor = toml::toml! {
            name = "univec-convert-a-to-b"
            kind = "convert"
            provider_model_id = "target-space"
            provider_source_id = "source-space"
            source_model = "model-a"
            target_model = "model-b"
            source_dim = 4
            dim = 3
        }
        .try_into()
        .unwrap();

        let measured = probe_convert_one(&config, &descriptor, std::time::Duration::from_secs(5))
            .await
            .expect("probe ok");
        assert_eq!(measured, 3);
        // The probe input is a unit basis vector of source_dim components.
        let body = mock.last_request();
        assert!(body.contains("[[1.0,0.0,0.0,0.0]]"), "{body}");
        assert!(body.contains("\"source_model\":\"source-space\""), "{body}");

        // A connector with no conversion endpoint is refused BEFORE any call.
        let count_before = mock.request_count();
        let mut bad = config.clone();
        bad.provider = "openai".to_string();
        let err = probe_convert_one(&bad, &descriptor, std::time::Duration::from_secs(5))
            .await
            .expect_err("openai cannot convert");
        assert!(err.to_string().contains("univec"), "{err}");
        assert_eq!(mock.request_count(), count_before, "no paid call was spent");
    }
}
