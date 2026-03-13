//! `postvec provider ...`: external embedding providers — the providers.d
//! connector files the inference host reads.
//!
//! Target resolution mirrors the `model` family: an explicit `--path DIR`
//! wins and is pure filesystem management (the directory is `<DIR>/providers.d`,
//! or `<DIR>` itself when it already is one — the postvec-server
//! administration story). Otherwise the selected cluster's effective
//! settings decide: embedded mode manages `postvec.providers_path`
//! (default `/etc/postvec/providers.d`); a grpc-mode cluster is refused
//! with the `--path` escape hatch named, because its provider files live on
//! the postvec-server nodes.
//!
//! Credentials never ride argv and are never printed back. Files are
//! written 0600 in a 0700 directory, owned by the cluster owner where one
//! is known — the same discipline as the registry credential store.

pub mod add;
pub mod ls;
pub mod rm;
pub mod test;

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
    /// `--path DIR`: a providers.d directory, no cluster involved.
    Path {
        dir: PathBuf,
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
    pub fn embedded_listen(&self) -> Option<String> {
        match self {
            ProviderTarget::Embedded { settings, .. } => Some(settings.embedded_http_listen()),
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
        // The root must already be there. It is the only thing that says who
        // the files belong to, and a typo would otherwise build a whole
        // credential tree in a directory nothing reads.
        if !path.is_dir() {
            return Err(CliError::precondition(format!(
                "--path {} is not an existing directory",
                path.display()
            ))
            .with_fix(
                "name a server root (or a providers.d) that already exists on this host; \
                 `postvec provider` creates the providers.d inside it, not the root itself",
            ));
        }
        let dir = providers_dir_from_path(&path);
        let owner = owner_of_nearest_existing(&dir);
        return Ok(ProviderTarget::Path { dir, owner });
    }
    if cli.database_url.is_some() {
        let context = Context::open(cli, output).await?;
        return target_from_live(context).await;
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
        file.take(providers::config::MAX_FILE_BYTES)
            .read_to_string(&mut raw)
            .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
        // `>` — the same boundary the loader's `read_private` applies. A
        // `>=` here refused a file of exactly the ceiling that the host loads.
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
    fn body(&self) -> Result<String> {
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
    pub fn write(&self, owner: Option<FileOwner>) -> Result<()> {
        let body = self.body()?;
        let label = self.path.display().to_string();
        providers::config::validate_str(&body, &label).map_err(|problem| {
            CliError::precondition(format!(
                "the resulting {label} is one the inference host would refuse: {problem}"
            ))
            .with_fix(
                "fix the flag (or the hand-edited field) this reports; the file was not \
                 written, so the host keeps serving whatever it serves now",
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
pub fn write_secret_file(path: &Path, body: &[u8], owner: Option<FileOwner>) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let dir = path
        .parent()
        .ok_or_else(|| CliError::internal(format!("{} has no parent", path.display())))?;
    ensure_private_dir(dir, owner)?;

    // An existing destination must be an ordinary, singly-linked file.
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CliError::precondition(format!(
                "{} is a symlink; refusing to write a credential through it",
                path.display()
            ))
            .with_fix("remove the symlink and rerun"))
        }
        Ok(meta) if !meta.is_file() => {
            return Err(CliError::precondition(format!(
                "{} exists and is not a regular file",
                path.display()
            )))
        }
        Ok(meta) if meta.nlink() != 1 => {
            return Err(CliError::precondition(format!(
                "{} has {} hard links; refusing to replace it",
                path.display(),
                meta.nlink()
            )))
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
        .map_err(|e| CliError::apply(format!("cannot create {}: {e}", tmp.display())))?;

    // Everything below acts on the **descriptor**, never on the path, and in
    // this order: content, then metadata, then one sync that covers both.
    // Doing chmod/chown by path after the sync left a window where the file
    // could be replaced, and left the metadata unsynced.
    let prepared = (|| -> std::io::Result<()> {
        file.write_all(body)?;
        file.flush()?;
        // The `mode` on OpenOptions is masked by the umask, so set it again.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        if let Some(owner) = owner.filter(|_| crate::proc::is_root()) {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        }
        // The rename is only atomic with respect to a crash if the data and
        // the metadata are on disk first.
        file.sync_all()
    })();
    drop(file);
    if let Err(e) = prepared {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot write {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot move {} into place: {e}",
            tmp.display()
        )));
    }
    sync_directory(dir, path)
}

/// Flush the directory entry a rename or removal just changed.
///
/// Checked, not best-effort: without it a crash can lose a change the command
/// already reported as applied, which is the exact outcome an atomic write
/// exists to prevent. A previous pass claimed this and then dropped the
/// error.
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
    file.take(MAX_SECRET_BYTES)
        .read_to_string(&mut raw)
        .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
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
    #[serde(default)]
    pub restart_needed: bool,
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
                if outcome.restart_needed {
                    journal.incomplete(
                        "the provider concurrency budget grew past what the host's ingress \
                         limit was sized with at start; models serve now, but full provider \
                         throughput needs a host restart"
                            .to_string(),
                    );
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
    // LOCAL embed model under a colliding name keeps that name local (§6.1),
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
}
