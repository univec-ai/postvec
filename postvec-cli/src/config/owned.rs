//! The owned configuration file, the ownership state file, and the host lock.
//!
//! Paths under `/etc` are treated as hostile: the destination is `lstat`ed and
//! opened with `O_NOFOLLOW`, symlinks and multiply-linked files are refused,
//! and every write is a same-directory temporary file plus `rename`, with
//! `fsync` on both the file and its directory. A half-written
//! `shared_preload_libraries` line is a cluster that will not start.

use crate::error::{CliError, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Where ownership metadata lives. Never contains credentials.
const STATE_DIR: &str = "/var/lib/postvec/clusters";
/// Where the mutation lock lives.
const LOCK_DIR: &str = "/run/lock/postvec";

pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Ownership metadata for one cluster. This is the CLI's memory of what it
/// changed, and the basis for every "may I touch this?" decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClusterState {
    pub schema_version: u32,
    pub cluster: String,
    pub config_path: PathBuf,
    pub config_sha256: String,
    /// Databases this CLI configured and is therefore willing to remove.
    pub managed_databases: Vec<String>,
    /// Databases that were already in `postvec.database` when the CLI took
    /// over. They are rendered so setup does not disrupt them, but the CLI
    /// does not claim the right to remove them.
    #[serde(default)]
    pub preserved_databases: Vec<String>,
    /// True when `postvec` was already in the effective preload list from
    /// configuration the CLI does not own; uninstall then cannot promise to
    /// remove it.
    #[serde(default)]
    pub preload_was_already_present: bool,
    /// The non-postvec preload items the CLI merged from. Lets doctor notice
    /// that another file has since changed the underlying value.
    #[serde(default)]
    pub preload_base: Vec<String>,
    pub mode: String,
    pub updated_by_cli_version: String,
    pub updated_at: String,
}

/// What the CLI found on disk versus what it remembers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "drift", rename_all = "kebab-case")]
pub enum Ownership {
    /// No owned file and no state: a fresh, unmanaged cluster.
    Unmanaged,
    /// File and state agree.
    Managed { state: ClusterState },
    /// A `99-postvec.conf` exists that the CLI did not write (no management
    /// marker and no ownership state). Never overwritten.
    Foreign { content: String },
    /// The file carries the management marker but there is no ownership state
    /// (state directory wiped, or written by a different root filesystem).
    /// Adoptable: the CLI wrote this file, it just no longer remembers which
    /// databases were its own, so every database already configured is
    /// treated as preserved.
    AdoptableMarker { content: String },
    /// State exists but the file was edited by hand. Never overwritten.
    Modified {
        state: ClusterState,
        actual_sha256: String,
    },
    /// State exists but the file is gone. Recreating the CLI's own file is
    /// safe, so this is only worth reporting.
    FileMissing { state: ClusterState },
}

impl Ownership {
    pub fn state(&self) -> Option<&ClusterState> {
        match self {
            Ownership::Managed { state }
            | Ownership::Modified { state, .. }
            | Ownership::FileMissing { state } => Some(state),
            _ => None,
        }
    }

    /// Whether the CLI may rewrite the file without operator intervention.
    pub fn is_writable(&self) -> bool {
        matches!(
            self,
            Ownership::Unmanaged
                | Ownership::Managed { .. }
                | Ownership::AdoptableMarker { .. }
                | Ownership::FileMissing { .. }
        )
    }

    /// The databases already named by an owned-but-unremembered file. They
    /// become `preserved_databases`, never `managed_databases`.
    pub fn configured_databases(&self) -> Vec<String> {
        let content = match self {
            Ownership::AdoptableMarker { content } | Ownership::Foreign { content } => content,
            _ => return Vec::new(),
        };
        content
            .lines()
            .filter_map(|line| parse_config_line(line, "postvec.database"))
            .next_back()
            .map(|value| super::guc::parse_extension_list(&value))
            .unwrap_or_default()
    }

    pub fn drift_description(&self) -> Option<String> {
        match self {
            Ownership::Foreign { .. } => Some(
                "a 99-postvec.conf exists that this CLI did not write (no management marker \
                 and no ownership state)"
                    .to_string(),
            ),
            Ownership::AdoptableMarker { .. } => Some(
                "the owned configuration file exists but its ownership state is missing; \
                 databases already configured will be preserved, not managed"
                    .to_string(),
            ),
            Ownership::Modified { .. } => {
                Some("the owned configuration file was modified by hand".to_string())
            }
            Ownership::FileMissing { .. } => Some(
                "ownership state exists but the configuration file is gone (removed by hand?)"
                    .to_string(),
            ),
            _ => None,
        }
    }
}

/// Extract `name = 'value'` from one configuration line. Deliberately
/// minimal: used only to read back a file the CLI itself rendered, or to name
/// what a foreign file configures. Effective values always come from the
/// server, never from this — except the offline model-command fallback,
/// which has no server to ask.
pub(crate) fn parse_config_line(line: &str, name: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let rest = line.strip_prefix(name)?;
    let rest = rest.trim_start().strip_prefix('=')?.trim();
    let rest = rest.split('#').next().unwrap_or("").trim();
    let inner = rest.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

/// Settings the owned snippet declares. Last assignment of each name wins,
/// matching [`Ownership::configured_databases`]. Used when a model command
/// cannot open a peer-authenticated session (an unprivileged `model ls`)
/// but can still read the 0644 configuration file.
pub(crate) fn settings_from_snippet(content: &str) -> crate::facts::SettingsSnapshot {
    use crate::facts::{SettingRow, SettingsSnapshot};

    let mut rows = Vec::new();
    for name in super::MANAGED_SETTINGS {
        if let Some(setting) = content
            .lines()
            .filter_map(|line| parse_config_line(line, name))
            .next_back()
        {
            rows.push(SettingRow {
                name: (*name).to_string(),
                setting,
                context: "configuration file".to_string(),
                source: "configuration file".to_string(),
                sourcefile: None,
                sourceline: None,
                pending_restart: false,
            });
        }
    }
    SettingsSnapshot {
        rows,
        file_rows: Vec::new(),
    }
}

pub fn sha256_hex(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// Handle to the pair of files the CLI owns for one cluster.
#[derive(Debug, Clone)]
pub struct OwnedPaths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub lock: PathBuf,
}

impl OwnedPaths {
    pub fn new(conf_d: &Path, cluster_key: &str) -> Self {
        Self {
            config: conf_d.join(super::OWNED_FILE_NAME),
            state: PathBuf::from(STATE_DIR).join(format!("{cluster_key}.json")),
            lock: PathBuf::from(LOCK_DIR).join(format!("{cluster_key}.lock")),
        }
    }

    /// Read the on-disk truth. Refuses to read through a symlink or a file
    /// with more than one hard link.
    pub fn inspect(&self) -> Result<Ownership> {
        let state = self.read_state()?;
        let content = read_regular_file(&self.config)?;
        match (state, content) {
            (None, None) => Ok(Ownership::Unmanaged),
            (None, Some(content)) => {
                if content.starts_with(super::MANAGED_MARKER) {
                    Ok(Ownership::AdoptableMarker { content })
                } else {
                    Ok(Ownership::Foreign { content })
                }
            }
            (Some(state), None) => Ok(Ownership::FileMissing { state }),
            (Some(state), Some(content)) => {
                let actual = sha256_hex(&content);
                if actual == state.config_sha256 {
                    Ok(Ownership::Managed { state })
                } else {
                    Ok(Ownership::Modified {
                        state,
                        actual_sha256: actual,
                    })
                }
            }
        }
    }

    pub fn read_config(&self) -> Result<Option<String>> {
        read_regular_file(&self.config)
    }

    fn read_state(&self) -> Result<Option<ClusterState>> {
        let Some(raw) = read_regular_file(&self.state)? else {
            return Ok(None);
        };
        let state: ClusterState = serde_json::from_str(&raw).map_err(|e| {
            CliError::precondition(format!(
                "ownership state {} is not readable: {e}",
                self.state.display()
            ))
            .with_fix(format!(
                "inspect and remove {} if it is corrupt, then rerun setup",
                self.state.display()
            ))
        })?;
        if state.schema_version != STATE_SCHEMA_VERSION {
            return Err(CliError::precondition(format!(
                "ownership state {} has schema_version {} but this CLI understands {}",
                self.state.display(),
                state.schema_version,
                STATE_SCHEMA_VERSION
            ))
            .with_fix("upgrade postvec-cli"));
        }
        Ok(Some(state))
    }

    /// Write configuration then state, atomically, with rollback.
    ///
    /// Ordering is deliberate: the configuration file is what PostgreSQL
    /// reads, so it goes first and is restored if the state write fails. The
    /// inverse (state without config) would make the CLI believe it owns a
    /// file that does not exist.
    pub fn commit(&self, content: &str, state: &ClusterState) -> Result<CommittedConfig> {
        let previous_config = self.read_config()?;
        let previous_state = read_regular_file(&self.state)?;
        write_atomic(&self.config, content.as_bytes(), 0o644)?;
        let serialized = serde_json::to_string_pretty(state)? + "\n";
        if let Err(e) = self.commit_state(&serialized) {
            // Undo the config write so an active snippet never outlives its
            // ownership record.
            match &previous_config {
                Some(old) => {
                    write_atomic(&self.config, old.as_bytes(), 0o644)?;
                }
                None => {
                    let _ = fs::remove_file(&self.config);
                }
            }
            return Err(e);
        }
        Ok(CommittedConfig {
            config_path: self.config.clone(),
            state_path: self.state.clone(),
            previous_config,
            previous_state,
        })
    }

    fn commit_state(&self, serialized: &str) -> Result<()> {
        if let Some(parent) = self.state.parent() {
            create_dir_all_checked(parent, 0o700)?;
        }
        write_atomic(&self.state, serialized.as_bytes(), 0o600)
    }

    /// Remove both files. Used when the last managed database goes away.
    pub fn remove(&self) -> Result<()> {
        if let Err(e) = fs::remove_file(&self.config) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(CliError::apply(format!(
                    "cannot remove {}: {e}",
                    self.config.display()
                )));
            }
        }
        if let Err(e) = fs::remove_file(&self.state) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(CliError::apply(format!(
                    "cannot remove {}: {e}",
                    self.state.display()
                )));
            }
        }
        Ok(())
    }
}

/// A committed configuration *and* its ownership state, remembering what was
/// there before so a failed restart can be rolled back.
///
/// Both files, because they are one logical unit: restoring the configuration
/// while leaving the new state behind would leave the recorded digest
/// describing a candidate that was rejected, so the installation would read as
/// hand-edited (`Ownership::Modified`) and every later `setup` would refuse to
/// repair it.
#[derive(Debug)]
pub struct CommittedConfig {
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub previous_config: Option<String>,
    pub previous_state: Option<String>,
}

impl CommittedConfig {
    /// Put both previous contents back. Used when the candidate configuration
    /// does not parse, or when the cluster refuses to start on it.
    pub fn restore(&self) -> Result<()> {
        restore_file(&self.config_path, self.previous_config.as_deref(), 0o644)?;
        restore_file(&self.state_path, self.previous_state.as_deref(), 0o600)
    }
}

fn restore_file(path: &Path, previous: Option<&str>, mode: u32) -> Result<()> {
    match previous {
        Some(content) => write_atomic(path, content.as_bytes(), mode),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CliError::apply(format!(
                "cannot remove {} during rollback: {e}",
                path.display()
            ))),
        },
    }
}

/// Read a file, refusing anything that is not a plain, singly-linked regular
/// file. Returns `None` when it does not exist.
///
/// `pub(crate)`: the registry credential store and install receipts read
/// their files through the same lstat/`O_NOFOLLOW`/link-count dance — a
/// second implementation of it is exactly the drift this module exists to
/// prevent.
pub(crate) fn read_regular_file(path: &Path) -> Result<Option<String>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CliError::precondition(format!(
                "cannot stat {}: {e}",
                path.display()
            )))
        }
    };
    if meta.file_type().is_symlink() {
        return Err(CliError::precondition(format!(
            "{} is a symlink; postvec refuses to read or replace it",
            path.display()
        ))
        .with_fix("replace the symlink with a regular file, or remove it"));
    }
    if !meta.is_file() {
        return Err(CliError::precondition(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.nlink() != 1 {
        return Err(CliError::precondition(format!(
            "{} has {} hard links; postvec refuses to replace it",
            path.display(),
            meta.nlink()
        )));
    }
    // O_NOFOLLOW closes the window between the lstat above and the open.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
    let mut content = String::new();
    {
        use std::io::Read;
        let mut file = file;
        file.read_to_string(&mut content)
            .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", path.display())))?;
    }
    Ok(Some(content))
}

/// Create a file at `path` atomically with `mode`, refusing an unsafe
/// destination or a world/group-writable parent directory.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CliError::internal(format!("{} has no parent directory", path.display())))?;
    let parent_meta = fs::metadata(parent)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", parent.display())))?;
    if parent_meta.permissions().mode() & 0o022 != 0 {
        return Err(CliError::precondition(format!(
            "{} is group- or world-writable (mode {:o}); refusing to write configuration there",
            parent.display(),
            parent_meta.permissions().mode() & 0o7777
        ))
        .with_fix(format!("chmod go-w {}", parent.display())));
    }
    // Reject an existing unsafe destination before touching anything.
    if let Some(_existing) = read_regular_file(path)? {
        // read_regular_file already rejected symlinks and hard links.
    }

    let tmp = parent.join(format!(
        ".{}.postvec.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
    ));
    // O_EXCL so a leftover attacker-planted temp name cannot be followed.
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| {
            CliError::apply(format!("cannot create {}: {e}", tmp.display())).with_fix(
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!("rerun with sudo (writing {} needs root)", parent.display())
                } else {
                    format!("check {}", parent.display())
                },
            )
        })?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(content)?;
        file.flush()?;
        // The rename below is only atomic with respect to a crash if the data
        // is on disk first.
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot write {}: {e}",
            tmp.display()
        )));
    }
    // Explicit chmod: `mode` on OpenOptions is masked by umask.
    if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)) {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot set mode on {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(CliError::apply(format!(
            "cannot install {}: {e}",
            path.display()
        )));
    }
    // Durability of the rename itself.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

pub(crate) fn create_dir_all_checked(dir: &Path, mode: u32) -> Result<()> {
    // An existing path must actually be a directory, reached without
    // following a symlink at the final component. A planted symlink here
    // would silently redirect everything created below.
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(CliError::precondition(format!(
                "{} is a symlink; refusing to use it as a directory",
                dir.display()
            ))
            .with_fix("replace the symlink with a real directory"));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(CliError::precondition(format!(
                "{} exists and is not a directory",
                dir.display()
            )));
        }
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CliError::precondition(format!(
                "cannot stat {}: {e}",
                dir.display()
            )))
        }
    }
    fs::create_dir_all(dir).map_err(|e| {
        CliError::apply(format!("cannot create {}: {e}", dir.display())).with_fix(
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                "rerun with sudo".to_string()
            } else {
                format!("create {} manually", dir.display())
            },
        )
    })?;
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(mode));
    Ok(())
}

/// Trust check for a directory a privileged command is about to mutate:
/// it must be a real (non-symlink) directory, owned by the effective
/// user and not world-writable. Otherwise a lower-privileged writer
/// could stage symlinks/hard links for the privileged command to
/// follow. Deliberately not applied to arbitrary read paths: reads are
/// already guarded by `read_regular_file`, and the goal is not to make
/// attacker-writable roots safe but to refuse them.
pub(crate) fn check_trusted_dir(dir: &Path, what: &str) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(CliError::precondition(format!(
            "{what} {} is a symlink; refusing to manage it",
            dir.display()
        ))
        .with_fix("point at the real directory instead"));
    }
    if !meta.is_dir() {
        return Err(CliError::precondition(format!(
            "{what} {} is not a directory",
            dir.display()
        )));
    }
    // Group AND world write bits are refused. On a shared-group host a
    // group-writable directory lets another account in that group stage
    // symlink/hard-link races. A deliberately trusted service group is
    // not modelled; the fix message says what to do instead.
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(CliError::precondition(format!(
            "{what} {} is group- or world-writable (mode {:o}); refusing to manage it",
            dir.display(),
            mode & 0o7777
        ))
        .with_fix(format!("chmod go-w {}", dir.display())));
    }
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        let owner = meta.uid();
        let fix = if euid == 0 {
            format!(
                "rerun without sudo as uid {owner} (the account that wrote this root), or \
                 `chown -R root:root {}` if it should be system-managed",
                dir.display()
            )
        } else {
            "rerun as the owning account, or fix the directory's ownership".to_string()
        };
        return Err(CliError::precondition(format!(
            "{what} {} is owned by uid {owner}, not the effective user (uid {euid}); \
             a privileged command must not trust another account's directory",
            dir.display()
        ))
        .with_fix(fix));
    }
    Ok(())
}

/// Ancestor-chain trust: every directory between the managed path and
/// the filesystem root must be one no other account can use to
/// rename-and-replace the subtree. Two rules, factored into
/// [`ancestor_policy`] so the decision is unit-testable without
/// privileged chown:
///
/// 1. **Ownership**: the ancestor must be owned by root or the effective
///    user. Mode bits say nothing about the *owner's* write permission — an
///    attacker-owned `0755` ancestor is fully replaceable by that attacker.
/// 2. **Writability**: group/world write bits are refused, except a
///    **sticky** directory (`/tmp`-style) whose chain entry directly below
///    it is owned by root or the effective user — sticky semantics prevent
///    everyone else from renaming that entry.
///
/// Symlinked ancestors are excluded separately (canonical-path equality in
/// `ModelRoot::lock_exclusive`).
pub(crate) fn check_trusted_ancestry(path: &Path) -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    // Track the uid of the chain entry below the ancestor being judged; the
    // first ancestor's child is the managed path itself.
    let mut child_uid = fs::symlink_metadata(path)
        .map_err(|e| CliError::precondition(format!("cannot stat {}: {e}", path.display())))?
        .uid();
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let meta = fs::symlink_metadata(dir).map_err(|e| {
            CliError::precondition(format!("cannot stat ancestor {}: {e}", dir.display()))
        })?;
        let mode = meta.permissions().mode();
        if let Err(problem) = ancestor_policy(meta.uid(), mode, child_uid, euid) {
            return Err(CliError::precondition(format!(
                "ancestor {}: {problem}; a writer there could replace the managed tree \
                 between check and use",
                dir.display()
            ))
            .with_fix(format!(
                "fix the ownership/mode of {} (or move the engine root under a trusted path)",
                dir.display()
            )));
        }
        child_uid = meta.uid();
        current = dir.parent();
    }
    Ok(())
}

/// The pure ancestor decision: `uid`/`mode` describe the ancestor,
/// `child_uid` the chain entry directly below it, `euid` the effective user.
fn ancestor_policy(
    uid: u32,
    mode: u32,
    child_uid: u32,
    euid: u32,
) -> std::result::Result<(), String> {
    if uid != 0 && uid != euid {
        return Err(format!(
            "owned by uid {uid}, not root or the effective user (uid {euid}) — its owner \
             can replace it regardless of mode {:o}",
            mode & 0o7777
        ));
    }
    if mode & 0o022 != 0 {
        let sticky = mode & 0o1000 != 0;
        let child_protected = child_uid == 0 || child_uid == euid;
        if !(sticky && child_protected) {
            return Err(format!(
                "writable by other accounts (mode {:o}){}",
                mode & 0o7777,
                if sticky {
                    " and the chain entry below it is foreign-owned, so sticky protection \
                     does not apply"
                } else {
                    " with no sticky bit"
                }
            ));
        }
    }
    Ok(())
}

#[derive(Debug)]
/// An exclusive, advisory, whole-host lock serializing mutating commands for
/// one cluster. Released when dropped (or when the process dies).
pub struct HostLock {
    _file: fs::File,
}

impl HostLock {
    /// Acquire the exclusive lock, creating the lock file if needed.
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_labeled(path, "this cluster")
    }

    /// Like [`HostLock::acquire`], naming what the lock protects in the
    /// contention message. The model commands lock an *engine root* that can
    /// be shared by several clusters, so "this cluster" would mislead there.
    pub fn acquire_labeled(path: &Path, what: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            create_dir_all_checked(parent, 0o755)?;
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o644)
            // O_NOFOLLOW: a pre-planted symlink at the lock path must fail
            // the open, not be followed.
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| {
                CliError::apply(format!("cannot open lock {}: {e}", path.display())).with_fix(
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        "rerun with sudo".to_string()
                    } else {
                        format!("check {}", path.display())
                    },
                )
            })?;
        flock(&file, libc::LOCK_EX | libc::LOCK_NB).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                CliError::precondition(format!(
                    "another postvec command is changing {what} (lock {} is held)",
                    path.display()
                ))
                .with_fix("wait for it to finish, then rerun")
            } else {
                CliError::apply(format!("cannot lock {}: {e}", path.display()))
            }
        })?;
        Ok(Self { _file: file })
    }

    /// Take a *shared* lock if the file already exists, so a read-only command
    /// does not observe a half-applied change. Never creates the file:
    /// `doctor` and `--dry-run` must leave no trace.
    ///
    /// Returns `Ok(None)` when there is nothing to lock, in which case a
    /// concurrent change cannot be excluded and the report says so.
    pub fn acquire_shared_if_present(path: &Path) -> Result<Option<Self>> {
        // Same discipline as the exclusive path: a planted symlink or
        // hard-linked file at the lock path is never followed, even for a
        // read-only lock. Unsafe shapes just mean "no lock taken". A
        // read-only command must not fail over them, but it must not open
        // through them either.
        match fs::symlink_metadata(path) {
            Err(_) => return Ok(None),
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() || meta.nlink() != 1 {
                    return Ok(None);
                }
            }
        }
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(f) => f,
            // Not being able to read the lock is not a reason to fail a
            // read-only command.
            Err(_) => return Ok(None),
        };
        match flock(&file, libc::LOCK_SH | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

fn flock(file: &fs::File, operation: libc::c_int) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A temporary directory with realistic `conf.d` permissions. The default
    /// tempdir inherits the umask (often group-writable), which the safety
    /// check correctly refuses.
    fn secure_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn state_for(dir: &Path, digest: &str) -> ClusterState {
        ClusterState {
            schema_version: STATE_SCHEMA_VERSION,
            cluster: "18/main".into(),
            config_path: dir.join(super::super::OWNED_FILE_NAME),
            config_sha256: digest.into(),
            managed_databases: vec!["univec".into()],
            preserved_databases: vec![],
            preload_was_already_present: false,
            preload_base: vec!["pg_stat_statements".into()],
            mode: "grpc".into(),
            updated_by_cli_version: "0.1.0".into(),
            updated_at: "2026-07-30T00:00:00Z".into(),
        }
    }

    fn paths_in(dir: &Path) -> OwnedPaths {
        OwnedPaths {
            config: dir.join(super::super::OWNED_FILE_NAME),
            state: dir.join("state.json"),
            lock: dir.join("cluster.lock"),
        }
    }

    #[test]
    fn atomic_write_sets_mode_and_content() {
        let dir = secure_tempdir();
        let path = dir.path().join("x.conf");
        write_atomic(&path, b"hello\n", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // No temporary files left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn atomic_write_replaces_existing_content_exactly() {
        let dir = secure_tempdir();
        let path = dir.path().join("x.conf");
        write_atomic(&path, b"first-and-longer\n", 0o644).unwrap();
        write_atomic(&path, b"second\n", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
    }

    #[test]
    fn symlink_destinations_are_refused() {
        let dir = secure_tempdir();
        let target = dir.path().join("real.conf");
        fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.conf");
        symlink(&target, &link).unwrap();
        let err = write_atomic(&link, b"y", 0o644).unwrap_err();
        assert!(err.to_string().contains("symlink"));
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "x",
            "target untouched"
        );
    }

    #[test]
    fn hard_linked_destinations_are_refused() {
        let dir = secure_tempdir();
        let a = dir.path().join("a.conf");
        fs::write(&a, "x").unwrap();
        let b = dir.path().join("b.conf");
        fs::hard_link(&a, &b).unwrap();
        let err = write_atomic(&b, b"y", 0o644).unwrap_err();
        assert!(err.to_string().contains("hard link"));
    }

    #[test]
    fn world_writable_parents_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("conf.d");
        fs::create_dir(&sub).unwrap();
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o777)).unwrap();
        let err = write_atomic(&sub.join("x.conf"), b"y", 0o644).unwrap_err();
        assert!(err.to_string().contains("world-writable"));
    }

    #[test]
    fn inspect_reports_each_ownership_state() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        assert_eq!(paths.inspect().unwrap(), Ownership::Unmanaged);

        // Foreign file.
        fs::write(&paths.config, "shared_preload_libraries = 'x'\n").unwrap();
        assert!(matches!(
            paths.inspect().unwrap(),
            Ownership::Foreign { .. }
        ));

        // Managed.
        let content = format!("{} rest\n", super::super::MANAGED_MARKER);
        let state = state_for(dir.path(), &sha256_hex(&content));
        paths.commit(&content, &state).unwrap();
        assert!(matches!(
            paths.inspect().unwrap(),
            Ownership::Managed { .. }
        ));
        assert!(paths.inspect().unwrap().is_writable());

        // Hand-edited.
        fs::write(&paths.config, format!("{content}# edited\n")).unwrap();
        let drifted = paths.inspect().unwrap();
        assert!(matches!(drifted, Ownership::Modified { .. }));
        assert!(!drifted.is_writable());
        assert!(drifted.drift_description().unwrap().contains("by hand"));

        // File removed but state kept: recreating our own file is safe.
        fs::remove_file(&paths.config).unwrap();
        let missing = paths.inspect().unwrap();
        assert!(matches!(missing, Ownership::FileMissing { .. }));
        assert!(missing.is_writable());
    }

    #[test]
    fn a_marked_file_without_state_is_adoptable_and_its_databases_preserved() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        fs::write(
            &paths.config,
            format!(
                "{} Manual edits are refused.\n\
                 shared_preload_libraries = 'pg_stat_statements,postvec'\n\
                 postvec.database = 'analytics,univec'\n",
                super::super::MANAGED_MARKER
            ),
        )
        .unwrap();
        let ownership = paths.inspect().unwrap();
        assert!(matches!(ownership, Ownership::AdoptableMarker { .. }));
        assert!(ownership.is_writable());
        assert_eq!(ownership.configured_databases(), ["analytics", "univec"]);
    }

    #[test]
    fn config_lines_are_parsed_conservatively() {
        assert_eq!(
            parse_config_line("postvec.database = 'a,b'", "postvec.database").as_deref(),
            Some("a,b")
        );
        assert_eq!(
            parse_config_line("postvec.database='x'  # comment", "postvec.database").as_deref(),
            Some("x")
        );
        assert_eq!(
            parse_config_line("# postvec.database = 'x'", "postvec.database"),
            None
        );
        assert_eq!(
            parse_config_line("postvec.databases = 'x'", "postvec.database"),
            None,
            "a longer setting name must not match"
        );
        assert_eq!(
            parse_config_line("postvec.database = 'o''brien'", "postvec.database").as_deref(),
            Some("o'brien")
        );
    }

    #[test]
    fn snippet_settings_read_the_cli_owned_values() {
        let content = "\
# Managed by postvec. Manual edits are refused; use `postvec setup`.
postvec.database = 'app,work'
postvec.mode = 'embedded'
postvec.ninference_path = '/opt/postvec/ninference'
postvec.embedded_http_listen = '127.0.0.1:33434'
postvec.ninference_http_endpoints = 'http://192.0.2.2:22222'
";
        let settings = settings_from_snippet(content);
        assert_eq!(settings.mode(), Some(crate::cli::Mode::Embedded));
        assert_eq!(
            settings.ninference_path().as_deref(),
            Some(std::path::Path::new("/opt/postvec/ninference"))
        );
        assert_eq!(settings.configured_databases(), ["app", "work"]);
        assert_eq!(settings.embedded_http_listen(), "127.0.0.1:33434");
        assert_eq!(
            settings.http_endpoints(),
            ["http://192.0.2.2:22222".to_string()]
        );
    }

    #[test]
    fn snippet_settings_ignore_comments_and_take_the_last_assignment() {
        let content = "\
# postvec.mode = 'grpc'
postvec.mode = 'grpc'
postvec.mode = 'embedded'
";
        let settings = settings_from_snippet(content);
        assert_eq!(settings.mode(), Some(crate::cli::Mode::Embedded));
    }

    #[test]
    fn commit_is_rolled_back_when_state_cannot_be_written() {
        let dir = secure_tempdir();
        let mut paths = paths_in(dir.path());
        // A state path whose parent is a regular file: mkdir must fail.
        let blocker = dir.path().join("blocked");
        fs::write(&blocker, "").unwrap();
        paths.state = blocker.join("nested").join("state.json");

        let content = format!("{} v1\n", super::super::MANAGED_MARKER);
        let state = state_for(dir.path(), &sha256_hex(&content));
        assert!(paths.commit(&content, &state).is_err());
        assert!(
            !paths.config.exists(),
            "a snippet must never be left active without ownership state"
        );

        // Same, but with a pre-existing file: it must be restored verbatim.
        fs::write(&paths.config, "previous\n").unwrap();
        assert!(paths.commit(&content, &state).is_err());
        assert_eq!(fs::read_to_string(&paths.config).unwrap(), "previous\n");
    }

    #[test]
    fn state_files_are_private() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        let content = format!("{} v1\n", super::super::MANAGED_MARKER);
        paths
            .commit(&content, &state_for(dir.path(), &sha256_hex(&content)))
            .unwrap();
        assert_eq!(
            fs::metadata(&paths.state).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn unknown_state_schema_is_refused() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        fs::write(&paths.state, r#"{"schema_version":99,"cluster":"18/main","config_path":"/x","config_sha256":"","managed_databases":[],"mode":"grpc","updated_by_cli_version":"9","updated_at":"now"}"#).unwrap();
        let err = paths.inspect().unwrap_err();
        assert!(err.to_string().contains("schema_version"));
    }

    #[test]
    fn committed_config_restores_previous_content() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        fs::write(&paths.config, "old\n").unwrap();
        let content = format!("{} new\n", super::super::MANAGED_MARKER);
        let committed = paths
            .commit(&content, &state_for(dir.path(), &sha256_hex(&content)))
            .unwrap();
        assert_eq!(fs::read_to_string(&paths.config).unwrap(), content);
        committed.restore().unwrap();
        assert_eq!(fs::read_to_string(&paths.config).unwrap(), "old\n");
    }

    /// Rolling back a rejected candidate must take the ownership state with
    /// it. Otherwise the recorded digest describes configuration that was never
    /// activated, the installation reads as hand-edited, and every later setup
    /// refuses to touch it.
    #[test]
    fn rollback_restores_the_ownership_state_too() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());

        // An existing, healthy installation.
        let first = format!("{} v1\n", super::super::MANAGED_MARKER);
        paths
            .commit(&first, &state_for(dir.path(), &sha256_hex(&first)))
            .unwrap();
        assert!(matches!(
            paths.inspect().unwrap(),
            Ownership::Managed { .. }
        ));

        // A second setup writes a candidate that then has to be rolled back.
        let second = format!("{} v2\n", super::super::MANAGED_MARKER);
        let committed = paths
            .commit(&second, &state_for(dir.path(), &sha256_hex(&second)))
            .unwrap();
        committed.restore().unwrap();

        assert_eq!(fs::read_to_string(&paths.config).unwrap(), first);
        assert!(
            matches!(paths.inspect().unwrap(), Ownership::Managed { .. }),
            "after rollback the installation must still be managed, not look edited"
        );
    }

    /// The same, for a first-ever setup: rollback must leave no state behind
    /// pointing at a file that no longer exists.
    #[test]
    fn rollback_of_a_first_install_removes_both_files() {
        let dir = secure_tempdir();
        let paths = paths_in(dir.path());
        let content = format!("{} v1\n", super::super::MANAGED_MARKER);
        let committed = paths
            .commit(&content, &state_for(dir.path(), &sha256_hex(&content)))
            .unwrap();
        committed.restore().unwrap();
        assert!(!paths.config.exists());
        assert!(!paths.state.exists());
        assert_eq!(paths.inspect().unwrap(), Ownership::Unmanaged);
    }

    #[test]
    fn exclusive_lock_excludes_a_second_holder() {
        let dir = secure_tempdir();
        let path = dir.path().join("c.lock");
        let held = HostLock::acquire(&path).unwrap();
        // flock is per open file description, so a second open in this same
        // process is a faithful stand-in for a second process.
        let err = HostLock::acquire(&path).unwrap_err();
        assert!(err.to_string().contains("another postvec command"));
        drop(held);
        assert!(HostLock::acquire(&path).is_ok());
    }

    /// The pure ancestor policy, exercised over the cases that need
    /// foreign ownership. Impractical to create on an unprivileged CI
    /// filesystem, which is exactly why the decision is factored pure.
    #[test]
    fn ancestor_policy_requires_trusted_ownership() {
        const EUID: u32 = 1000;
        const ATTACKER: u32 = 1001;

        // A foreign-owned 0755 ancestor is refused: its owner can replace it
        // regardless of the mode bits.
        let err = super::ancestor_policy(ATTACKER, 0o755, EUID, EUID).unwrap_err();
        assert!(err.contains("owned by uid 1001"), "{err}");

        // A foreign-owned sticky ancestor is refused for the same reason.
        assert!(super::ancestor_policy(ATTACKER, 0o1777, EUID, EUID).is_err());

        // Root- and self-owned quiet ancestors pass.
        super::ancestor_policy(0, 0o755, EUID, EUID).unwrap();
        super::ancestor_policy(EUID, 0o700, EUID, EUID).unwrap();

        // Root-owned /tmp-style sticky: fine when the chain entry below is
        // ours (or root's), refused when it is foreign-owned.
        super::ancestor_policy(0, 0o1777, EUID, EUID).unwrap();
        super::ancestor_policy(0, 0o1777, 0, EUID).unwrap();
        let err = super::ancestor_policy(0, 0o1777, ATTACKER, EUID).unwrap_err();
        assert!(err.contains("sticky protection does not apply"), "{err}");

        // Group/world-writable without sticky stays refused even root-owned.
        assert!(super::ancestor_policy(0, 0o775, EUID, EUID).is_err());
        assert!(super::ancestor_policy(EUID, 0o777, EUID, EUID).is_err());
    }

    #[test]
    fn shared_lock_never_creates_the_file() {
        let dir = secure_tempdir();
        let path = dir.path().join("absent.lock");
        assert!(HostLock::acquire_shared_if_present(&path)
            .unwrap()
            .is_none());
        assert!(!path.exists(), "read-only commands must leave no trace");
    }
}
