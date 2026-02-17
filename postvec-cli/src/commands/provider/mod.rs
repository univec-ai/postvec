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

/// What a provider command works against.
pub enum ProviderTarget {
    /// `--path DIR`: a providers.d directory, no cluster involved.
    Path { dir: PathBuf },
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
            ProviderTarget::Path { dir } => dir,
            ProviderTarget::Embedded { dir, .. } => dir,
        }
    }

    /// The label that stands in for `cluster` in result envelopes.
    pub fn label(&self) -> String {
        match self {
            ProviderTarget::Path { dir } => format!("path:{}", dir.display()),
            ProviderTarget::Embedded { cluster_id, .. } => cluster_id.clone(),
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
        return Ok(ProviderTarget::Path {
            dir: providers_dir_from_path(&path),
        });
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
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(CliError::precondition(format!(
                    "cannot read {}: {e}",
                    path.display()
                )))
            }
        };
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

    pub fn push_model(&mut self, entry: toml::Value) {
        let table = self.value.as_table_mut().expect("provider file is a table");
        table
            .entry("models")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .expect("models is an array")
            .push(entry);
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

    /// Serialize and write: 0600 file, 0700 directory, chowned to `owner`
    /// when one is known and we can (root).
    pub fn write(&self, owner: Option<&crate::proc::OsAccount>) -> Result<()> {
        let body = format!(
            "# Managed by `postvec provider`. Comments do not survive a rewrite.\n{}",
            toml::to_string_pretty(&self.value)
                .map_err(|e| CliError::internal(format!("cannot serialize provider file: {e}")))?
        );
        write_secret_file(&self.path, body.as_bytes(), owner)
    }
}

/// Create `path`'s parent 0700 and write `path` 0600, atomically (write to a
/// sibling temp file, then rename), chowning both to `owner` when running
/// as root on their behalf.
pub fn write_secret_file(
    path: &Path,
    body: &[u8],
    owner: Option<&crate::proc::OsAccount>,
) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = path
        .parent()
        .ok_or_else(|| CliError::internal(format!("{} has no parent", path.display())))?;
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::apply(format!("cannot create {}: {e}", dir.display())))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| CliError::apply(format!("cannot chmod {}: {e}", dir.display())))?;
        chown_if_root(dir, owner)?;
    }
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| CliError::apply(format!("cannot create {}: {e}", tmp.display())))?;
        file.write_all(body)
            .map_err(|e| CliError::apply(format!("cannot write {}: {e}", tmp.display())))?;
        file.sync_all().ok();
    }
    // Mode again, in case the file pre-existed the OpenOptions mode.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| CliError::apply(format!("cannot chmod {}: {e}", tmp.display())))?;
    chown_if_root(&tmp, owner)?;
    std::fs::rename(&tmp, path)
        .map_err(|e| CliError::apply(format!("cannot move {} into place: {e}", tmp.display())))?;
    Ok(())
}

fn chown_if_root(path: &Path, owner: Option<&crate::proc::OsAccount>) -> Result<()> {
    let Some(owner) = owner else { return Ok(()) };
    if !crate::proc::is_root() {
        return Ok(());
    }
    std::os::unix::fs::chown(path, Some(owner.uid), Some(owner.gid))
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

/// Read a referenced key file (0600-checked) for the verification probe.
pub fn read_secret_file(path: &Path) -> Result<String> {
    require_private_secret_file(path)?;
    let raw = std::fs::read_to_string(path)
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

// ---- Reload client -------------------------------------------------------

/// What `POST /admin/providers/reload` reported.
#[derive(Debug, Deserialize)]
pub struct ReloadOutcome {
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
    for listen in &candidates {
        match post_reload(listen, timeout).await {
            Ok(outcome) => {
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
            "no local inference host answered a provider reload on {}",
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

    // What the running host serves locally right now: an enabled embed model
    // under a colliding name keeps that name local (§6.1), so its columns
    // are not affected either way.
    let locally_served: std::collections::BTreeSet<String> =
        match crate::commands::model::admin::loaded_inventory(&listen, timeout).await {
            Some(inventory) => inventory
                .models
                .iter()
                .filter(|m| m.enabled && m.model_type.as_deref() == Some("embed"))
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

    #[test]
    fn provider_files_round_trip_and_report_the_key_source_never_the_key() {
        let dir = tempfile::tempdir().unwrap();
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
        loaded.push_model(
            toml::toml! {
                name = "openai-text-embedding-3-large"
                provider_model_id = "text-embedding-3-large"
                dim = 3072
            }
            .into(),
        );
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
