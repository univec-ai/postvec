//! `postvec uninstall --all --purge` — the host-side sweep. **Experimental**:
//! destructive cleanup for disposable release-test hosts, not yet a
//! production promise (see `docs/postvec-uninstall.md`).
//!
//! The contract is narrow on purpose. The sweep deletes only paths for which
//! it holds **positive evidence** that postvec put them there, and only under
//! roots it has proven safe to operate in:
//!
//! - a model directory is one that carries the engine descriptor
//!   (`ninference.hub.json`); a directory without one is retained;
//! - the CLI's model-store state is matched by exact name (`.staging`,
//!   `.trash`, `.swap`);
//! - `libs/` is swept only when it actually holds ONNX Runtime;
//! - a provider file is one `provider ls` would list (`<name>.toml`); every
//!   other file in `providers.d` is retained and reported — the CLI writes
//!   inline keys, so a separate key file is the operator's;
//! - the cluster state file and the extension files are exact names under
//!   trusted roots (`/var/lib/postvec`, `pg_config`'s directories).
//!
//! A root is used only if it passes [`safe_root`]: absolute, canonical, not a
//! system directory, at least two components deep, and — for the leaf **and
//! every ancestor** — owned by root, the effective user or the cluster's own
//! account, with no group/world write (`setup` creates `/etc/postvec` and
//! `providers.d` owned by the cluster account). The caller additionally
//! proves the GUC naming it comes from the CLI-owned snippet or is the
//! built-in default (`uninstall::purge_roots`).
//!
//! **Every leaf is classified, and only classified leaves are deleted.** A
//! candidate is enumerated down to its leaves at planning time, each leaf with
//! its device/inode and a package-ownership verdict (from *every* installed
//! package database, fail-closed). At apply time the candidate is enumerated
//! again; the sweep proceeds only if the leaf set is identical, deletes files
//! one by one against their recorded identity, and removes directories with
//! `rmdir` — never recursively — so a child that appeared in between retains
//! its directory and the result is partial. A traversal that cannot read an
//! entry makes the whole candidate *uncertain*: retained, reported, exit 3.
//!
//! Concurrency: the caller holds the host-wide postvec lock exclusively and
//! has **stopped the cluster** before calling [`apply`]; `apply` then takes the
//! engine root's model-store lock and the providers directory's lock, and
//! only then re-checks processes and other clusters, rebuilds the plan and
//! deletes. Lock files are unlinked while their locks are still held.

use crate::commands::provider::FileOwner;
use crate::config::owned;
use crate::error::{CliError, Result};
use crate::plan::{ApplyJournal, PlanStep};
use crate::proc::{self, Cmd, OsAccount};
use crate::registry::receipt::Receipt;
use crate::registry::root::ModelRoot;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The CLI's transient state beside the models, in the engine root.
const MODELS_STATE: &[&str] = &[".staging", ".trash", ".swap"];
const DESCRIPTOR_FILE: &str = "ninference.hub.json";
/// Root login state; swept with the purge (`postvec login` restores it in
/// seconds; per-user XDG stores are never touched).
const AUTH_FILE: &str = "auth.json";
/// The providers directory's lock file (`provider::lock_provider_dir`).
const PROVIDER_LOCK_FILE: &str = ".lock";
/// The engine-root serving lease: a **stable inode** under
/// `/run/lock/postvec`, named by the **SHA-256 of the canonical root path
/// bytes** (fixed 64-hex — a raw-path encoding hit `NAME_MAX` at 121 root
/// bytes), and **never unlinked** — a lock file inside a tree the sweep is
/// about to `rmdir`, or one that gets unlinked, silently forks the lock
/// domain the moment someone else creates a new inode at the pathname.
/// `postvec-server` holds the shared side for its whole serving lifetime
/// (fail-closed at its startup); the sweep holds the exclusive side, so a
/// server starting after the process scan blocks the sweep, or is blocked
/// by it, never races it — **provided both see the same inode**: across
/// container mount namespaces the participants must share
/// `/run/lock/postvec` as a bind mount, or the container must be stopped
/// first. The derivation is a cross-crate contract with
/// `postvec-server/src/lib.rs::serving_lease_path` — both carry a literal
/// test pinning the same example.
pub fn serving_lease_path(canonical_root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canonical_root.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    PathBuf::from(format!("/run/lock/postvec/engine-{hex}.lease"))
}

/// The exclusive side of the serving lease. Refused while any
/// `postvec-server` (new enough to take the lease) is serving this root.
/// The root must already be canonical (`safe_root` proved it).
pub fn acquire_serving_lease(canonical_root: &Path) -> Result<owned::HostLock> {
    owned::HostLock::acquire_labeled(
        &serving_lease_path(canonical_root),
        "this engine root (a postvec-server serving lease)",
    )
}

/// Directories a purge root may never be. Anything directly under `/` is a
/// system directory; a postvec root lives at least one level deeper
/// (`/opt/postvec`, `/etc/postvec/providers.d`, `/srv/postvec/…`).
const SYSTEM_DIRS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib32",
    "/lib64",
    "/media",
    "/mnt",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/var",
    "/usr/bin",
    "/usr/lib",
    "/usr/lib64",
    "/usr/local",
    "/usr/sbin",
    "/usr/share",
    "/var/lib",
    "/var/log",
    "/var/run",
    "/var/tmp",
];

/// Where the sweep looks. Built by `uninstall` from the cluster's live
/// settings and paths; a root is `None` when the caller could not prove it
/// safe, with the reason in `excluded`.
#[derive(Debug, Clone)]
pub struct PurgeRoots {
    /// The cluster being torn down (`18/main`), so every *other* cluster on
    /// the host can be checked.
    pub cluster_id: String,
    pub engine_root: Option<PathBuf>,
    /// A `postvec-server` process runs on this host: leave the engine root
    /// alone, it may be serving from it.
    pub engine_root_in_use: bool,
    pub providers_dir: Option<PathBuf>,
    /// Who owns the providers directory (the cluster account): the provider
    /// lock is taken on their behalf.
    pub provider_owner: Option<FileOwner>,
    /// `/var/lib/postvec` — holds `clusters/<key>.json` and `auth.json`.
    pub state_dir: PathBuf,
    pub cluster_key: String,
    pub pkglibdir: PathBuf,
    pub sharedir: PathBuf,
    /// False when `shared_preload_libraries` still names postvec from
    /// configuration the CLI does not own: deleting the library would then
    /// stop the next postmaster start.
    pub extension_files_removable: bool,
    /// The running CLI binary, reported but never deleted.
    pub cli_binary: Option<PathBuf>,
    /// Why a root was excluded — reported in the plan, never silent.
    pub excluded: Vec<String>,
}

/// Which package manager claimed a file, and therefore which command removes
/// what the sweep left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PackageManager {
    Dpkg,
    Rpm,
}

impl PackageManager {
    fn remove_command(self, packages: &[String]) -> String {
        match self {
            PackageManager::Dpkg => format!("sudo apt purge {}", packages.join(" ")),
            PackageManager::Rpm => format!("sudo dnf remove {}", packages.join(" ")),
        }
    }

    /// Fixed executable paths: the sweep runs as root and must not let a
    /// `PATH` entry substitute the tool that decides what is deletable.
    fn binary(self) -> &'static [&'static str] {
        match self {
            PackageManager::Dpkg => &["/usr/bin/dpkg", "/bin/dpkg"],
            PackageManager::Rpm => &["/usr/bin/rpm", "/bin/rpm"],
        }
    }

    fn installed(self) -> Option<PathBuf> {
        self.binary()
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }

    fn all_installed() -> Vec<(PackageManager, PathBuf)> {
        [PackageManager::Dpkg, PackageManager::Rpm]
            .into_iter()
            .filter_map(|manager| manager.installed().map(|binary| (manager, binary)))
            .collect()
    }
}

/// The answer to "does a package own this path?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Package(PackageManager, String),
    /// Every package database was consulted and none lists the path.
    Unowned,
    /// No answer could be established; the path must be retained.
    Unknown(String),
}

/// One filesystem object below (or at) a candidate, with the identity it
/// had when it was classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leaf {
    pub path: PathBuf,
    pub identity: (u64, u64),
    pub is_dir: bool,
}

/// One path the sweep considers, enumerated down to its leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub what: String,
    /// `(device, inode)` at planning time; a deletion happens only against
    /// the same object.
    pub identity: (u64, u64),
    /// Every object below `path`, and `path` itself, in enumeration order.
    pub leaves: Vec<Leaf>,
    /// False when some entry below `path` could not be read or stat'ed; such
    /// a candidate is never deleted.
    pub complete: bool,
}

/// The resolved sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    /// Paths to delete, in order.
    pub remove: Vec<Candidate>,
    /// Package-owned paths, grouped by `(manager, package)` — a hybrid host
    /// gets one removal command per manager, and an identically named dpkg
    /// and RPM package cannot collapse into one entry.
    pub packaged: BTreeMap<(PackageManager, String), Vec<PathBuf>>,
    /// Paths whose ownership could not be established or whose contents
    /// could not be fully inspected: retained, and the result is partial.
    pub unresolved: Vec<(PathBuf, String)>,
    /// Locations that could not be inspected at all (an unreadable
    /// directory): retained, and the result is partial.
    pub uncertain: Vec<String>,
    /// Directories to remove afterwards if they are empty, deepest first.
    pub prune_dirs: Vec<PathBuf>,
    /// Lock files, unlinked while their locks are still held. The serving
    /// lease is NOT here: it is a stable inode under /run/lock/postvec and
    /// is never unlinked.
    pub model_lock: Option<PathBuf>,
    pub provider_lock: Option<PathBuf>,
    /// Informational: what was left alone by design and why.
    pub notes: Vec<String>,
}

impl PurgePlan {
    pub fn steps(&self) -> Vec<PlanStep> {
        self.remove
            .iter()
            .map(|candidate| PlanStep::RemovePath {
                path: candidate.path.clone(),
                what: candidate.what.clone(),
            })
            .collect()
    }

    /// The one-line instruction for the packaged remainder, if any.
    pub fn package_note(&self) -> Option<String> {
        if self.packaged.is_empty() {
            return None;
        }
        let names: Vec<&str> = self
            .packaged
            .keys()
            .map(|(_, name)| name.as_str())
            .collect();
        let mut by_manager: BTreeMap<PackageManager, Vec<String>> = BTreeMap::new();
        for (manager, name) in self.packaged.keys() {
            by_manager.entry(*manager).or_default().push(name.clone());
        }
        let command = by_manager
            .into_iter()
            .map(|(manager, names)| manager.remove_command(&names))
            .collect::<Vec<_>>()
            .join(" && ");
        Some(format!(
            "package-owned files were left in place ({}); remove the packages with: {command}",
            names.join(", ")
        ))
    }

    /// Everything that makes the outcome *partial* — safety uncertainty, as
    /// opposed to designed retention. The caller journals each as incomplete
    /// (exit 3), in a dry run as much as in an apply.
    pub fn incomplete(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .unresolved
            .iter()
            .map(|(path, reason)| format!("{} was retained: {reason}", path.display()))
            .collect();
        out.extend(self.uncertain.iter().cloned());
        out
    }

    /// Everything the operator should read beside the steps.
    pub fn all_notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        notes.extend(self.incomplete());
        notes.extend(self.package_note());
        notes
    }
}

/// One owner policy for a root and every ancestor: root, the effective user,
/// or the cluster account; no group/world write, except a sticky directory
/// whose chain entry below it is protected (the `/tmp` shape).
fn trusted_uid(uid: u32, euid: u32, trusted_owner: Option<u32>) -> bool {
    uid == 0 || uid == euid || Some(uid) == trusted_owner
}

/// Is `root` a directory the sweep may operate in?
pub fn safe_root(
    root: &Path,
    what: &str,
    trusted_owner: Option<u32>,
) -> std::result::Result<(), String> {
    if !root.is_absolute() {
        return Err(format!("{what} {} is not absolute", root.display()));
    }
    let text = root.to_string_lossy();
    let normalized = text.trim_end_matches('/');
    let normalized = if normalized.is_empty() {
        "/"
    } else {
        normalized
    };
    if SYSTEM_DIRS.contains(&normalized) {
        return Err(format!(
            "{what} {} is a system directory; refusing to sweep it",
            root.display()
        ));
    }
    if root.components().count() < 3 {
        // `/` + two named components, e.g. /opt/postvec.
        return Err(format!(
            "{what} {} is directly below /; refusing to sweep it",
            root.display()
        ));
    }
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {what} {}: {e}", root.display()))?;
    if canonical != root {
        return Err(format!(
            "{what} {} traverses a symlink (canonical path is {}); refusing to sweep it",
            root.display(),
            canonical.display()
        ));
    }
    let euid = unsafe { libc::geteuid() };
    let meta = fs::symlink_metadata(root).map_err(|e| format!("cannot stat {what}: {e}"))?;
    if !meta.is_dir() {
        return Err(format!("{what} {} is not a directory", root.display()));
    }
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "{what} {} is group- or world-writable (mode {:o}); refusing to sweep it",
            root.display(),
            mode & 0o7777
        ));
    }
    if !trusted_uid(meta.uid(), euid, trusted_owner) {
        return Err(format!(
            "{what} {} is owned by uid {}, not root, the effective user or the cluster \
             account; refusing to sweep it",
            root.display(),
            meta.uid()
        ));
    }
    // Ancestors: the same owner rule (the cluster account included — `setup`
    // creates `/etc/postvec` for it), and a writable ancestor is tolerated
    // only when it is sticky and the entry below it is protected.
    let mut child_uid = meta.uid();
    let mut current = root.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() {
            break;
        }
        let meta = fs::symlink_metadata(dir)
            .map_err(|e| format!("cannot stat ancestor {}: {e}", dir.display()))?;
        let mode = meta.permissions().mode();
        if !trusted_uid(meta.uid(), euid, trusted_owner) {
            return Err(format!(
                "ancestor {} of {what} is owned by uid {}, not root, the effective user or \
                 the cluster account — its owner can replace the tree",
                dir.display(),
                meta.uid()
            ));
        }
        if mode & 0o022 != 0 {
            let sticky = mode & 0o1000 != 0;
            if !(sticky && trusted_uid(child_uid, euid, trusted_owner)) {
                return Err(format!(
                    "ancestor {} of {what} is writable by other accounts (mode {:o}); a writer \
                     there could replace the tree between check and use",
                    dir.display(),
                    mode & 0o7777
                ));
            }
        }
        child_uid = meta.uid();
        current = dir.parent();
    }
    Ok(())
}

/// Refuse when another cluster on this host is still set up by this CLI: the
/// engine root and the package files are shared, and that cluster would lose
/// them.
pub fn other_clusters_configured(
    state_dir: &Path,
    cluster_key: &str,
) -> std::result::Result<Vec<String>, String> {
    let clusters = state_dir.join("clusters");
    let entries = match fs::read_dir(&clusters) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        // Fail closed: an unreadable state directory is not an empty one.
        Err(e) => return Err(format!("cannot read {}: {e}", clusters.display())),
    };
    let own = format!("{cluster_key}.json");
    let mut others = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| format!("cannot read an entry of {}: {e}", clusters.display()))?;
        let Ok(name) = entry.file_name().into_string() else {
            return Err(format!(
                "{} holds an entry with a non-UTF-8 name; cannot prove what it belongs to",
                clusters.display()
            ));
        };
        if name.ends_with(".json") && name != own {
            others.push(name.trim_end_matches(".json").to_string());
        }
    }
    others.sort();
    Ok(others)
}

/// What the other clusters on this host look like, from `pg_lsclusters`.
pub struct OtherClusters {
    /// Every other cluster's id, whether or not it uses postvec.
    pub all: Vec<String>,
    /// Those whose effective `shared_preload_libraries` names postvec — or
    /// whose configuration could not be read, which counts the same way.
    pub preloading: Vec<String>,
}

/// Every *other* cluster `pg_lsclusters` knows. A malformed listing is an
/// error: the purge cannot prove it examined every cluster.
pub async fn other_clusters(own_id: &str, timeout: Duration) -> Result<OtherClusters> {
    let listings = crate::cluster::debian::list_clusters(timeout).await?;
    let mut all = Vec::new();
    let mut preloading = Vec::new();
    for listing in listings.iter().filter(|l| l.id() != own_id) {
        all.push(listing.id());
        let binary = PathBuf::from(format!(
            "/usr/lib/postgresql/{}/bin/postgres",
            listing.major
        ));
        let config = format!(
            "/etc/postgresql/{}/{}/postgresql.conf",
            listing.major, listing.name
        );
        let account = OsAccount::lookup(&listing.owner).ok();
        let cmd = Cmd::new(binary)
            .arg("-D")
            .arg(listing.data_dir.display().to_string())
            .arg("-C")
            .arg("shared_preload_libraries")
            .arg("-c")
            .arg(format!("config_file={config}"))
            .run_as(account.as_ref());
        match proc::run(&cmd, timeout).await {
            Ok(output) if output.ok() => {
                let value = output.first_line().to_string();
                if crate::config::guc::list_contains_postvec(
                    &crate::config::guc::parse_library_list(&value),
                ) {
                    preloading.push(format!(
                        "{} (shared_preload_libraries = {value})",
                        listing.id()
                    ));
                }
            }
            Ok(output) => preloading.push(format!(
                "{} (its configuration could not be read: {})",
                listing.id(),
                output.failure_detail()
            )),
            Err(error) => preloading.push(format!(
                "{} (its configuration could not be read: {error})",
                listing.id()
            )),
        }
    }
    Ok(OtherClusters { all, preloading })
}

/// Is a `postvec-server` running on this host? It serves from the same
/// engine-root layout, so its models must not be swept from under it. A
/// container's process is visible here too, which is the conservative
/// direction.
pub fn server_process_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        // Fail closed: if the process table cannot be read, assume a server
        // may be running and leave the engine root alone.
        return true;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| name.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or(false)
        })
        .any(|entry| {
            fs::read_to_string(entry.path().join("comm"))
                .map(|comm| comm.trim() == "postvec-server")
                .unwrap_or(false)
        })
}

fn identity_of(meta: &fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

/// Enumerate `path` and everything below it, each with its identity. Symlinks
/// are leaves (never followed). `complete` is false when any entry could not
/// be read or stat'ed — the candidate is then never deleted.
fn enumerate_leaves(path: &Path) -> (Vec<Leaf>, bool) {
    let mut leaves = Vec::new();
    let mut complete = true;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (leaves, false);
    };
    leaves.push(Leaf {
        path: path.to_path_buf(),
        identity: identity_of(&meta),
        is_dir: meta.is_dir(),
    });
    let mut stack = if meta.is_dir() {
        vec![path.to_path_buf()]
    } else {
        Vec::new()
    };
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            let child = entry.path();
            let Ok(meta) = fs::symlink_metadata(&child) else {
                complete = false;
                continue;
            };
            leaves.push(Leaf {
                path: child.clone(),
                identity: identity_of(&meta),
                is_dir: meta.is_dir(),
            });
            if meta.is_dir() {
                stack.push(child);
            }
        }
    }
    (leaves, complete)
}

fn candidate(path: PathBuf, what: String) -> Option<Candidate> {
    let (mut leaves, complete) = enumerate_leaves(&path);
    let identity = leaves.first()?.identity;
    // Enumeration order is not a stable filesystem API; the plan comparison
    // at apply time must not read reordering as change.
    leaves.sort_by(|a, b| a.path.cmp(&b.path));
    Some(Candidate {
        path,
        what,
        identity,
        leaves,
        complete,
    })
}

/// Does this directory hold ONNX Runtime? Any regular file whose name starts
/// with `libonnxruntime`, at any depth (bounded). Tri-state: an entry that
/// cannot be read makes the answer `Err` — the caller retains the directory
/// as uncertain rather than guessing.
fn onnxruntime_evidence(libs: &Path) -> std::result::Result<bool, String> {
    fn walk(dir: &Path, depth: usize) -> std::result::Result<bool, String> {
        if depth > 6 {
            return Ok(false);
        }
        let entries =
            fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
            if metadata.is_dir() {
                if walk(&path, depth + 1)? {
                    return Ok(true);
                }
            } else if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("libonnxruntime"))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    walk(libs, 0)
}

/// What `gather` found.
pub struct Gathered {
    pub candidates: Vec<Candidate>,
    pub prune: Vec<PathBuf>,
    pub model_lock: Option<PathBuf>,
    pub provider_lock: Option<PathBuf>,
    /// Designed retention: informational.
    pub notes: Vec<String>,
    /// Safety uncertainty (an unreadable directory): the result is partial.
    pub uncertain: Vec<String>,
}

/// Everything the sweep would touch, before asking the package manager.
/// Pure filesystem reading. An unreadable location is *uncertain*: retained,
/// reported, and the whole result partial.
pub fn gather(roots: &PurgeRoots) -> Gathered {
    let mut candidates = Vec::new();
    let mut prune = Vec::new();
    let mut model_lock = None;
    let mut provider_lock = None;
    let mut notes = roots.excluded.clone();
    let mut uncertain = Vec::new();

    // --- engine root -------------------------------------------------------
    match (&roots.engine_root, roots.engine_root_in_use) {
        (Some(root), true) => notes.push(format!(
            "a postvec-server process is running on this host, so the engine root {} was \
             left alone (it may be serving from it)",
            root.display()
        )),
        (Some(root), false) if root.is_dir() => {
            let models = root.join("models");
            if models.is_dir() {
                match fs::read_dir(&models) {
                    Ok(entries) => {
                        for entry in entries {
                            let Ok(entry) = entry else {
                                uncertain.push(format!(
                                    "an entry of {} could not be read; the directory was left \
                                     alone",
                                    models.display()
                                ));
                                continue;
                            };
                            let path = entry.path();
                            let name = entry.file_name().to_string_lossy().to_string();
                            let Ok(metadata) = fs::symlink_metadata(&path) else {
                                uncertain.push(format!(
                                    "{} could not be inspected and was left alone",
                                    path.display()
                                ));
                                continue;
                            };
                            if name == crate::registry::root::LOCK_FILE {
                                model_lock = Some(path);
                            } else if MODELS_STATE.contains(&name.as_str()) {
                                candidates.extend(candidate(
                                    path,
                                    "postvec-cli model-store state".to_string(),
                                ));
                            } else if metadata.is_dir() {
                                gather_backend(
                                    &path,
                                    &name,
                                    &mut candidates,
                                    &mut prune,
                                    &mut notes,
                                    &mut uncertain,
                                );
                            } else {
                                notes.push(format!(
                                    "{} is not part of the engine-root layout and was left \
                                     alone",
                                    path.display()
                                ));
                            }
                        }
                    }
                    Err(error) => uncertain.push(format!(
                        "cannot read {}: {error}; the engine root was left alone",
                        models.display()
                    )),
                }
                prune.push(models);
            }
            let libs = root.join("libs");
            if let Ok(metadata) = fs::symlink_metadata(&libs) {
                if !metadata.is_dir() {
                    notes.push(format!(
                        "{} is not a directory and was left alone",
                        libs.display()
                    ));
                } else {
                    match onnxruntime_evidence(&libs) {
                        Ok(true) => {
                            candidates.extend(candidate(libs, "ONNX Runtime libraries".to_string()))
                        }
                        Ok(false) => notes.push(format!(
                            "{} does not hold ONNX Runtime and was left alone",
                            libs.display()
                        )),
                        Err(problem) => uncertain.push(format!(
                            "{} could not be fully inspected ({problem}) and was left alone",
                            libs.display()
                        )),
                    }
                }
            }
            match fs::read_dir(root) {
                Ok(entries) => {
                    for entry in entries {
                        let Ok(entry) = entry else {
                            uncertain
                                .push(format!("an entry of {} could not be read", root.display()));
                            continue;
                        };
                        let name = entry.file_name().to_string_lossy().to_string();
                        if name == "models" || name == "libs" {
                            continue;
                        }
                        notes.push(format!(
                            "{} is not part of the engine-root layout and was left alone{}",
                            entry.path().display(),
                            if name == "providers.d" {
                                " (a postvec-server providers directory holds that node's \
                                 credentials)"
                            } else {
                                ""
                            }
                        ));
                    }
                }
                Err(error) => uncertain.push(format!("cannot read {}: {error}", root.display())),
            }
            prune.push(root.clone());
        }
        _ => {}
    }

    // --- provider connector files ------------------------------------------
    if let Some(dir) = &roots.providers_dir {
        if dir.is_dir() {
            match crate::commands::provider::ls::provider_files(dir) {
                Ok(files) => {
                    let connectors: BTreeSet<PathBuf> = files.iter().cloned().collect();
                    for path in files {
                        // A regular file only: `provider_files` filters by
                        // name, and a symlink named like a connector is not
                        // something the CLI wrote.
                        match fs::symlink_metadata(&path) {
                            Ok(metadata) if metadata.is_file() => candidates.extend(candidate(
                                path,
                                "external-provider connector file (holds API credentials)"
                                    .to_string(),
                            )),
                            _ => notes.push(format!(
                                "{} is not a regular file and was left alone",
                                path.display()
                            )),
                        }
                    }
                    match fs::read_dir(dir) {
                        Ok(entries) => {
                            for entry in entries {
                                let Ok(entry) = entry else {
                                    uncertain.push(format!(
                                        "an entry of {} could not be read",
                                        dir.display()
                                    ));
                                    continue;
                                };
                                let path = entry.path();
                                if path.file_name().is_some_and(|n| n == PROVIDER_LOCK_FILE) {
                                    provider_lock = Some(path);
                                } else if !connectors.contains(&path) {
                                    notes.push(format!(
                                        "{} is not a connector file this CLI writes and was \
                                         left alone",
                                        path.display()
                                    ));
                                }
                            }
                        }
                        Err(error) => {
                            uncertain.push(format!("cannot read {}: {error}", dir.display()))
                        }
                    }
                }
                Err(problem) => {
                    uncertain.push(format!("{problem}; the providers directory was left alone"))
                }
            }
            prune.push(dir.clone());
            if let Some(parent) = dir.parent() {
                // `/etc/postvec` exists only for providers.d.
                if parent.file_name().is_some_and(|n| n == "postvec") {
                    prune.push(parent.to_path_buf());
                }
            }
        }
    }

    // --- CLI state ---------------------------------------------------------
    let state = roots
        .state_dir
        .join("clusters")
        .join(format!("{}.json", roots.cluster_key));
    if state.exists() {
        candidates.extend(candidate(state, "postvec-cli cluster state".to_string()));
    }
    if roots.state_dir.exists() {
        prune.push(roots.state_dir.join("clusters"));
        prune.push(roots.state_dir.clone());
        // The root registry login goes with the sweep — a purge is the "this
        // host is done with postvec" verb, and a stale credential is worse
        // than re-running the cheap `postvec login`. Per-user XDG stores are
        // not the system sweep's to touch.
        let auth = roots.state_dir.join(AUTH_FILE);
        if auth.exists() {
            candidates.extend(candidate(
                auth,
                "registry login; `postvec login` restores it".to_string(),
            ));
        }
    }

    // --- extension files ---------------------------------------------------
    let mut extension_files = vec![
        (
            roots.pkglibdir.join("postvec.so"),
            "extension library".to_string(),
        ),
        (
            roots.sharedir.join("extension").join("postvec.control"),
            "extension control file".to_string(),
        ),
    ];
    let extension_dir = roots.sharedir.join("extension");
    match fs::read_dir(&extension_dir) {
        Ok(entries) => {
            let mut scripts: Vec<PathBuf> = Vec::new();
            for entry in entries {
                let Ok(entry) = entry else {
                    uncertain.push(format!(
                        "an entry of {} could not be read; extension SQL scripts may be missed",
                        extension_dir.display()
                    ));
                    continue;
                };
                let p = entry.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("postvec--") && n.ends_with(".sql"))
                {
                    scripts.push(p);
                }
            }
            scripts.sort();
            extension_files.extend(
                scripts
                    .into_iter()
                    .map(|p| (p, "extension SQL script".to_string())),
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => uncertain.push(format!(
            "cannot read {}: {e}; extension SQL scripts may be missed",
            extension_dir.display()
        )),
    }
    let present: Vec<(PathBuf, String)> = extension_files
        .into_iter()
        .filter(|(path, _)| fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
        .collect();
    if !present.is_empty() && !roots.extension_files_removable {
        notes.push(format!(
            "the extension files were left in place (postvec is still preloaded from \
             configuration this CLI does not own, or another PostgreSQL cluster exists on this \
             host and may use them): {}",
            present
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        for (path, what) in present {
            candidates.extend(candidate(path, what));
        }
    }

    Gathered {
        candidates,
        prune,
        model_lock,
        provider_lock,
        notes,
        uncertain,
    }
}

fn gather_backend(
    backend_dir: &Path,
    backend: &str,
    candidates: &mut Vec<Candidate>,
    prune: &mut Vec<PathBuf>,
    notes: &mut Vec<String>,
    uncertain: &mut Vec<String>,
) {
    match fs::read_dir(backend_dir) {
        Ok(entries) => {
            let mut models = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => models.push(entry.path()),
                    Err(_) => uncertain.push(format!(
                        "an entry of {} could not be read; it was left alone",
                        backend_dir.display()
                    )),
                }
            }
            models.sort();
            for path in models {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let is_model_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
                    && path.join(DESCRIPTOR_FILE).is_file();
                if !is_model_dir {
                    // No descriptor: not something the engine or the CLI
                    // put here, whatever its name says.
                    notes.push(format!(
                        "{} is not a model directory (no {DESCRIPTOR_FILE}) and was left alone",
                        path.display()
                    ));
                    continue;
                }
                let receipt = matches!(Receipt::read(&path), Ok(Some(_)));
                candidates.extend(candidate(
                    path,
                    if receipt {
                        format!("model {backend}/{name}, pulled by postvec-cli")
                    } else {
                        format!("model {backend}/{name}")
                    },
                ));
            }
        }
        Err(error) => uncertain.push(format!(
            "cannot read {}: {error}; it was left alone",
            backend_dir.display()
        )),
    }
    prune.push(backend_dir.to_path_buf());
}

/// Split the gathered candidates by package ownership. `owners` answers
/// "which package claims this path?" for every leaf of every candidate — the
/// real one shells out to dpkg/rpm, tests use a table. A leaf with no answer
/// is unknown, never unowned; an incompletely enumerated candidate is
/// unresolved whatever the answers.
pub fn resolve(
    gathered: Gathered,
    cli_binary: Option<&Path>,
    owners: &BTreeMap<PathBuf, Owner>,
) -> PurgePlan {
    let mut plan = PurgePlan {
        prune_dirs: gathered.prune,
        model_lock: gathered.model_lock,
        provider_lock: gathered.provider_lock,
        notes: gathered.notes,
        uncertain: gathered.uncertain,
        ..PurgePlan::default()
    };
    let lookup = |path: &Path| -> Owner {
        owners
            .get(path)
            .cloned()
            .unwrap_or_else(|| Owner::Unknown("no ownership answer was recorded".to_string()))
    };
    for candidate in gathered.candidates {
        if !candidate.complete {
            plan.unresolved.push((
                candidate.path,
                "its contents could not be fully inspected".to_string(),
            ));
            continue;
        }
        // The verdict for a tree: any packaged leaf makes it packaged (every
        // owning package is named); otherwise any unknown makes it retained;
        // only an all-unowned tree is deleted.
        let mut packages: BTreeSet<(PackageManager, String)> = BTreeSet::new();
        let mut unknown: Option<String> = None;
        for leaf in &candidate.leaves {
            match lookup(&leaf.path) {
                Owner::Package(manager, package) => {
                    packages.insert((manager, package));
                }
                Owner::Unknown(reason) => {
                    unknown.get_or_insert(reason);
                }
                Owner::Unowned => {}
            }
        }
        if !packages.is_empty() {
            for (manager, package) in packages {
                plan.packaged
                    .entry((manager, package))
                    .or_default()
                    .push(candidate.path.clone());
            }
        } else if let Some(reason) = unknown {
            plan.unresolved.push((
                candidate.path,
                format!("package ownership could not be established ({reason})"),
            ));
        } else {
            plan.remove.push(candidate);
        }
    }
    if let Some(binary) = cli_binary {
        match lookup(binary) {
            Owner::Package(manager, package) => {
                plan.packaged
                    .entry((manager, package))
                    .or_default()
                    .push(binary.to_path_buf());
            }
            Owner::Unowned => plan.notes.push(format!(
                "the postvec CLI itself, {}, is not package-owned and is the running program; \
                 remove it yourself when you are done",
                binary.display()
            )),
            Owner::Unknown(_) => plan.notes.push(format!(
                "the postvec CLI itself, {}, is the running program and was not removed",
                binary.display()
            )),
        }
    }
    // Deepest directories first, so parents empty out before their turn.
    plan.prune_dirs
        .sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    plan.prune_dirs.dedup();
    plan
}

/// Ask **every** installed package database who owns each path. One answer
/// per input path: a package if any database claims it; unknown if any
/// database could not answer and none claimed it; unowned only when every
/// database answered "no". Without dpkg or rpm at their fixed locations the
/// host has no package database and every path is unowned.
pub async fn package_owners(paths: &[PathBuf], timeout: Duration) -> BTreeMap<PathBuf, Owner> {
    let managers = PackageManager::all_installed();
    let mut answers: BTreeMap<PathBuf, Owner> = BTreeMap::new();
    if managers.is_empty() {
        for path in paths {
            answers.insert(path.clone(), Owner::Unowned);
        }
        return answers;
    }
    for (manager, binary) in managers {
        let mut this: BTreeMap<PathBuf, Owner> = BTreeMap::new();
        for chunk in paths.chunks(200) {
            let flag = match manager {
                PackageManager::Dpkg => "-S",
                PackageManager::Rpm => "-qf",
            };
            let mut cmd = Cmd::new(binary.clone()).arg(flag);
            for path in chunk {
                cmd = cmd.arg(path.display().to_string());
            }
            let outcome = match proc::run(&cmd, timeout).await {
                Ok(output) => parse_owners(manager, chunk, &output.stdout, &output.stderr),
                Err(error) => chunk
                    .iter()
                    .map(|path| {
                        (
                            path.clone(),
                            Owner::Unknown(format!("{} did not answer: {error}", binary.display())),
                        )
                    })
                    .collect(),
            };
            this.extend(outcome);
        }
        for path in paths {
            let verdict = this
                .remove(path)
                .unwrap_or_else(|| Owner::Unknown("no verdict".to_string()));
            let merged = match (answers.remove(path), verdict) {
                (Some(Owner::Package(m, p)), _) | (_, Owner::Package(m, p)) => Owner::Package(m, p),
                (Some(Owner::Unknown(r)), _) | (_, Owner::Unknown(r)) => Owner::Unknown(r),
                _ => Owner::Unowned,
            };
            answers.insert(path.clone(), merged);
        }
    }
    answers
}

/// One `Owner` per queried path out of a dpkg/rpm invocation.
///
/// dpkg: stdout carries `pkg[:arch][, pkg2]: /path` per owned path, stderr
/// `dpkg-query: no path found matching pattern /path` per unowned one; a path
/// in neither is unknown. rpm: one stdout line per argument, in order —
/// either the package or `file /path is not owned by any package`; a line
/// count that does not match the arguments makes the whole batch unknown.
pub fn parse_owners(
    manager: PackageManager,
    queried: &[PathBuf],
    stdout: &str,
    stderr: &str,
) -> BTreeMap<PathBuf, Owner> {
    let mut answers: BTreeMap<PathBuf, Owner> = BTreeMap::new();
    match manager {
        PackageManager::Dpkg => {
            for line in stdout.lines().map(str::trim) {
                if line.is_empty() || line.starts_with("diversion") {
                    continue;
                }
                let Some((packages, path)) = line.rsplit_once(": ") else {
                    continue;
                };
                let Some(package) = packages
                    .split(", ")
                    .next()
                    .map(|p| p.split(':').next().unwrap_or(p).trim().to_string())
                    .filter(|p| !p.is_empty())
                else {
                    continue;
                };
                answers.insert(PathBuf::from(path.trim()), Owner::Package(manager, package));
            }
            for line in stderr.lines() {
                if let Some(rest) = line.split("no path found matching pattern ").nth(1) {
                    let path = PathBuf::from(rest.trim().trim_end_matches('.'));
                    answers.entry(path).or_insert(Owner::Unowned);
                }
            }
            for path in queried {
                answers.entry(path.clone()).or_insert_with(|| {
                    Owner::Unknown("dpkg -S gave no verdict for this path".to_string())
                });
            }
        }
        PackageManager::Rpm => {
            let lines: Vec<&str> = stdout
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if lines.len() != queried.len() {
                for path in queried {
                    answers.insert(
                        path.clone(),
                        Owner::Unknown(format!(
                            "rpm -qf answered {} line(s) for {} path(s)",
                            lines.len(),
                            queried.len()
                        )),
                    );
                }
                return answers;
            }
            for (path, line) in queried.iter().zip(lines) {
                let owner = if line.contains("is not owned by any package") {
                    Owner::Unowned
                } else if line.starts_with("error:") {
                    Owner::Unknown(line.to_string())
                } else {
                    Owner::Package(manager, line.to_string())
                };
                answers.insert(path.clone(), owner);
            }
        }
    }
    answers
}

/// Build the sweep for a cluster: refuse for a shared host, gather, resolve.
pub async fn plan(roots: &PurgeRoots, timeout: Duration) -> Result<PurgePlan> {
    let mut roots = roots.clone();
    let mut blockers = other_clusters_configured(&roots.state_dir, &roots.cluster_key)
        .map_err(|problem| {
            CliError::precondition(format!(
                "--purge refused: the CLI state directory could not be examined ({problem})"
            ))
        })?
        .into_iter()
        .map(|id| format!("{id} (set up by this CLI)"))
        .collect::<Vec<_>>();
    // Unit tests build fake hosts under a tempdir and have no cluster of
    // their own; a release build always asks.
    if !cfg!(test) {
        let others = other_clusters(&roots.cluster_id, timeout).await?;
        blockers.extend(others.preloading);
        if !others.all.is_empty() && roots.extension_files_removable {
            // Another cluster may hold the extension in a database without
            // preloading it (a manual install): the shared extension files
            // are not this cluster's alone to delete.
            roots.extension_files_removable = false;
            roots.excluded.push(format!(
                "other PostgreSQL clusters exist on this host ({}) and may use the extension \
                 files; those were left in place",
                others.all.join(", ")
            ));
        }
    }
    if !blockers.is_empty() {
        return Err(CliError::precondition(format!(
            "--purge refused: another cluster on this host still uses postvec, or could not be \
             proven not to — {} — and the engine root and extension files are shared",
            blockers.join("; ")
        ))
        .with_fix(
            "remove postvec from that cluster first (`postvec --cluster <id> uninstall --all`, \
             or drop it from its shared_preload_libraries and restart), then purge from the \
             last one",
        ));
    }
    let gathered = gather(&roots);
    let mut to_query: Vec<PathBuf> = gathered
        .candidates
        .iter()
        .flat_map(|c| c.leaves.iter().map(|leaf| leaf.path.clone()))
        .collect();
    to_query.extend(roots.cli_binary.iter().cloned());
    to_query.sort();
    to_query.dedup();
    let owners = package_owners(&to_query, timeout).await;
    Ok(resolve(gathered, roots.cli_binary.as_deref(), &owners))
}

/// Descriptor-relative filesystem primitives: after one absolute
/// `O_NOFOLLOW` open of the candidate's parent, every descent, stat and
/// unlink is `*at()` against a held directory descriptor — a symlink swapped
/// in anywhere along the path cannot redirect the operation. The residual
/// window is a same-directory rename between `fstatat` and `unlinkat`, an
/// act only a writer of that (ownership-verified) directory can perform.
mod at {
    use std::ffi::{CString, OsStr};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn cstr(name: &OsStr) -> std::io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| std::io::Error::other("NUL in name"))
    }

    pub fn open_dir(path: &Path) -> std::io::Result<OwnedFd> {
        let c = cstr(path.as_os_str())?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn open_dir_at(parent: &OwnedFd, name: &OsStr) -> std::io::Result<OwnedFd> {
        let c = cstr(name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// `(dev, ino, is_dir)` of `name` below `parent`, never following a
    /// symlink.
    pub fn identity_at(parent: &OwnedFd, name: &OsStr) -> std::io::Result<((u64, u64), bool)> {
        let c = cstr(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                c.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            (st.st_dev, st.st_ino),
            (st.st_mode & libc::S_IFMT) == libc::S_IFDIR,
        ))
    }

    pub fn identity_of_fd(fd: &OwnedFd) -> std::io::Result<(u64, u64)> {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((st.st_dev, st.st_ino))
    }

    pub fn unlink_at(parent: &OwnedFd, name: &OsStr, dir: bool) -> std::io::Result<()> {
        let c = cstr(name)?;
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), c.as_ptr(), flags) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Delete one candidate leaf by leaf, descriptor-relative: one absolute
/// `O_NOFOLLOW` open of the candidate's parent, then `openat`/`fstatat`/
/// `unlinkat` all the way down. Files go only when their recorded identity
/// still matches; directories go with `rmdir` deepest-first — never
/// recursively — so a child that appeared after classification keeps its
/// directory. Anything that does not match is left and reported.
pub fn delete_candidate(candidate: &Candidate) -> Vec<String> {
    let mut problems = Vec::new();
    let (Some(parent_path), Some(_)) = (candidate.path.parent(), candidate.path.file_name()) else {
        return vec![format!(
            "{}: has no parent directory",
            candidate.path.display()
        )];
    };
    let parent_fd = match at::open_dir(parent_path) {
        Ok(fd) => fd,
        Err(error) => return vec![format!("cannot open {}: {error}", parent_path.display())],
    };
    // Open every directory of the tree top-down, verifying each opened
    // descriptor against the identity recorded at classification. `dir_fds`
    // maps a directory's path to its held descriptor; lookups borrow the map
    // only until the child descriptor is obtained, so no entry is borrowed
    // across an insertion or removal.
    let mut dir_fds: BTreeMap<&Path, std::os::fd::OwnedFd> = BTreeMap::new();
    let mut dirs: Vec<&Leaf> = candidate.leaves.iter().filter(|l| l.is_dir).collect();
    dirs.sort_by_key(|l| l.path.components().count());
    for leaf in &dirs {
        let opened = if leaf.path == candidate.path {
            at::open_dir_at(&parent_fd, leaf.path.file_name().unwrap_or_default())
        } else {
            match leaf.path.parent().and_then(|p| dir_fds.get(p)) {
                Some(base) => at::open_dir_at(base, leaf.path.file_name().unwrap_or_default()),
                None => {
                    problems.push(format!(
                        "{}: its parent directory could not be opened; not deleted",
                        leaf.path.display()
                    ));
                    continue;
                }
            }
        };
        match opened {
            Ok(fd) => match at::identity_of_fd(&fd) {
                Ok(identity) if identity == leaf.identity => {
                    dir_fds.insert(leaf.path.as_path(), fd);
                }
                Ok(_) => problems.push(format!(
                    "{}: changed since it was classified; not deleted",
                    leaf.path.display()
                )),
                Err(error) => problems.push(format!("{}: {error}", leaf.path.display())),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => problems.push(format!("{}: {error}", leaf.path.display())),
        }
    }
    // Files: verify identity below the held parent descriptor, then unlink
    // through the same descriptor.
    for leaf in candidate.leaves.iter().filter(|l| !l.is_dir) {
        let base = if leaf.path == candidate.path {
            Some(&parent_fd)
        } else {
            leaf.path.parent().and_then(|p| dir_fds.get(p))
        };
        let Some(base) = base else {
            problems.push(format!(
                "{}: its parent directory was not opened; not deleted",
                leaf.path.display()
            ));
            continue;
        };
        let name = leaf.path.file_name().unwrap_or_default();
        match at::identity_at(base, name) {
            Ok((identity, false)) if identity == leaf.identity => {
                if let Err(error) = at::unlink_at(base, name, false) {
                    problems.push(format!("{}: {error}", leaf.path.display()));
                }
            }
            Ok(_) => problems.push(format!(
                "{}: changed since it was classified; not deleted",
                leaf.path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => problems.push(format!("{}: {error}", leaf.path.display())),
        }
    }
    // Directories deepest-first, through their parent's descriptor. `rmdir`
    // refuses a directory holding anything unclassified. The directory's own
    // descriptor is dropped before its parent is borrowed, so the map never
    // hands out two entries at once.
    dirs.sort_by_key(|l| std::cmp::Reverse(l.path.components().count()));
    for leaf in dirs {
        drop(dir_fds.remove(leaf.path.as_path()));
        let base = if leaf.path == candidate.path {
            Some(&parent_fd)
        } else {
            leaf.path.parent().and_then(|p| dir_fds.get(p))
        };
        let Some(base) = base else {
            continue; // already reported on the way down
        };
        let name = leaf.path.file_name().unwrap_or_default();
        match at::identity_at(base, name) {
            Ok((identity, true)) if identity == leaf.identity => {
                if let Err(error) = at::unlink_at(base, name, true) {
                    problems.push(format!(
                        "{}: {error} (a child appeared after it was classified, or it could \
                         not be removed)",
                        leaf.path.display()
                    ));
                }
            }
            Ok(_) => problems.push(format!(
                "{}: changed since it was classified; not deleted",
                leaf.path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => problems.push(format!("{}: {error}", leaf.path.display())),
        }
    }
    problems
}

/// Apply a confirmed plan under the engine-root and providers locks: rebuild
/// it now, and for each confirmed candidate still present with an identical
/// leaf set, delete leaf by leaf; then prune the directories that emptied
/// out. New candidates are never deleted.
pub async fn apply(
    confirmed: &PurgePlan,
    roots: &mut PurgeRoots,
    timeout: Duration,
    journal: &mut ApplyJournal,
) {
    // A plan with nothing to delete performs no filesystem mutation at all:
    // no lock unlink, no directory prune. The documented stop/sweep/start
    // sequence applies only when something is actually swept.
    if confirmed.remove.is_empty() {
        return;
    }
    // Locks first, then every check: nothing observed here can change under
    // the deletion. Without a lock, the corresponding root is not touched.
    // The serving lease outranks the model-store lock: a postvec-server
    // holds its shared side for its whole serving lifetime, so a server
    // starting after the /proc scan blocks here instead of racing the sweep.
    let serving_lease = match &roots.engine_root {
        Some(root) if root.is_dir() => {
            // safe_root proved the root canonical; re-canonicalize anyway so
            // the lease name can never be derived from a symlinked spelling.
            match root
                .canonicalize()
                .map_err(|e| CliError::precondition(format!("cannot canonicalize: {e}")))
                .and_then(|canonical| acquire_serving_lease(&canonical))
            {
                Ok(lease) => Some(lease),
                Err(error) => {
                    journal.incomplete(format!(
                        "the engine root {} was left alone: {error}",
                        root.display()
                    ));
                    roots.engine_root = None;
                    None
                }
            }
        }
        _ => None,
    };
    let model_lock = match &roots.engine_root {
        Some(root) if root.join("models").is_dir() => {
            match ModelRoot::new(root.clone()).lock_exclusive() {
                Ok(lock) => Some(lock),
                Err(error) => {
                    journal.incomplete(format!(
                        "the engine root {} was left alone: {error}",
                        root.display()
                    ));
                    roots.engine_root = None;
                    None
                }
            }
        }
        _ => None,
    };
    let provider_lock = match &roots.providers_dir {
        Some(dir) if dir.is_dir() => {
            match crate::commands::provider::lock_provider_dir(dir, roots.provider_owner) {
                Ok(lock) => Some(lock),
                Err(error) => {
                    journal.incomplete(format!(
                        "the providers directory {} was left alone: {error}",
                        dir.display()
                    ));
                    roots.providers_dir = None;
                    None
                }
            }
        }
        _ => None,
    };

    // The guards that ran at planning time run again now, under the locks.
    roots.engine_root_in_use = server_process_running();
    let fresh = match plan(roots, timeout).await {
        Ok(fresh) => fresh,
        Err(error) => {
            journal.incomplete(format!("--purge stopped before deleting anything: {error}"));
            return;
        }
    };
    let still_planned: BTreeMap<&Path, &Candidate> =
        fresh.remove.iter().map(|c| (c.path.as_path(), c)).collect();

    for candidate in &confirmed.remove {
        match still_planned.get(candidate.path.as_path()) {
            Some(current)
                if current.identity == candidate.identity && current.leaves == candidate.leaves => {
            }
            Some(_) => {
                journal.incomplete(format!(
                    "{} changed since the plan was confirmed (a different object, or different \
                     contents); not deleted",
                    candidate.path.display()
                ));
                continue;
            }
            None => {
                if fs::symlink_metadata(&candidate.path).is_ok() {
                    journal.incomplete(format!(
                        "{} is no longer eligible for deletion (the plan changed); not deleted",
                        candidate.path.display()
                    ));
                }
                continue;
            }
        }
        let problems = delete_candidate(candidate);
        if problems.is_empty() {
            journal.record(format!(
                "deleted {} ({})",
                candidate.path.display(),
                candidate.what
            ));
        } else {
            for problem in problems {
                journal.incomplete(format!(
                    "could not fully delete {} ({}): {problem}",
                    candidate.path.display(),
                    candidate.what
                ));
            }
        }
    }
    for line in fresh.incomplete() {
        journal.incomplete(line);
    }

    // Lock files go while their locks are still held: a waiter that acquires
    // the old inode after our release finds no path to a root any more,
    // rather than a second, independent lock domain at the same path.
    for (lock, held, what) in [
        (&fresh.model_lock, model_lock.is_some(), "model-store lock"),
        (
            &fresh.provider_lock,
            provider_lock.is_some(),
            "providers.d lock",
        ),
    ] {
        let Some(lock) = lock.as_ref().filter(|_| held) else {
            continue;
        };
        match fs::remove_file(lock) {
            Ok(()) => journal.record(format!("deleted {} ({what})", lock.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                journal.incomplete(format!("could not delete {}: {error}", lock.display()))
            }
        }
    }
    for dir in &fresh.prune_dirs {
        match fs::remove_dir(dir) {
            Ok(()) => journal.record(format!("removed empty directory {}", dir.display())),
            // Not empty, or already gone: both fine.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) || error.raw_os_error() == Some(libc::ENOTEMPTY) => {}
            Err(error) => journal.incomplete(format!(
                "could not remove directory {}: {error}",
                dir.display()
            )),
        }
    }
    drop(model_lock);
    drop(provider_lock);
    drop(serving_lease);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(dir: &Path) -> PurgeRoots {
        PurgeRoots {
            cluster_id: "18/main".to_string(),
            engine_root: Some(dir.join("opt/postvec")),
            engine_root_in_use: false,
            providers_dir: Some(dir.join("etc/postvec/providers.d")),
            provider_owner: None,
            state_dir: dir.join("var/lib/postvec"),
            cluster_key: "18-main".to_string(),
            pkglibdir: dir.join("usr/lib/postgresql/18/lib"),
            sharedir: dir.join("usr/share/postgresql/18"),
            extension_files_removable: true,
            cli_binary: Some(dir.join("usr/local/bin/postvec")),
            excluded: Vec::new(),
        }
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    /// A full host: a package-owned bundled model and libs, a pulled model
    /// with a receipt, a hand-copied model, a directory that only looks like
    /// a model, CLI state, provider files plus an operator's key file, an
    /// unpackaged extension set, and a registry login.
    fn populate(dir: &Path) {
        let r = roots(dir);
        let root = r.engine_root.clone().unwrap();
        touch(&root.join("models/onnx-runtime/bundled/ninference.hub.json"));
        touch(&root.join("models/onnx-runtime/bundled/model.onnx"));
        touch(&root.join("models/onnx-runtime/pulled/ninference.hub.json"));
        touch(&root.join("models/onnx-runtime/pulled/weights/model.onnx"));
        touch(&root.join("models/onnx-runtime/manual/ninference.hub.json"));
        touch(&root.join("models/onnx-runtime/not-a-model/README"));
        touch(&root.join("models/.staging/part"));
        touch(&root.join("models/.postvec.lock"));
        touch(&root.join("libs/onnxruntime/lib/libonnxruntime.so"));
        let providers = r.providers_dir.clone().unwrap();
        touch(&providers.join("openai.toml"));
        touch(&providers.join("bedrock.key"));
        touch(&r.state_dir.join("clusters/18-main.json"));
        touch(&r.state_dir.join("auth.json"));
        touch(&r.pkglibdir.join("postvec.so"));
        touch(&r.sharedir.join("extension/postvec.control"));
        touch(&r.sharedir.join("extension/postvec--0.1.0.sql"));
        touch(&r.sharedir.join("extension/vector.control"));
        touch(&r.cli_binary.clone().unwrap());
        fs::set_permissions(&providers, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("models"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn owners_for(
        gathered: &Gathered,
        binary: &Path,
        rule: impl Fn(&Path) -> Owner,
    ) -> BTreeMap<PathBuf, Owner> {
        let mut map = BTreeMap::new();
        for candidate in &gathered.candidates {
            for leaf in &candidate.leaves {
                map.insert(leaf.path.clone(), rule(&leaf.path));
            }
        }
        map.insert(binary.to_path_buf(), rule(binary));
        map
    }

    /// The ownership a Debian host would answer with.
    fn debian_rule(dir: &Path) -> impl Fn(&Path) -> Owner {
        let r = roots(dir);
        let root = r.engine_root.clone().unwrap();
        move |path: &Path| {
            let owned = [
                (
                    root.join("models/onnx-runtime/bundled"),
                    "postvec-model-minilm-l6-v2",
                ),
                (root.join("libs"), "postvec-onnxruntime"),
                (r.pkglibdir.join("postvec.so"), "postgresql-18-postvec"),
                (
                    r.sharedir.join("extension/postvec.control"),
                    "postgresql-18-postvec",
                ),
                (
                    r.sharedir.join("extension/postvec--0.1.0.sql"),
                    "postgresql-18-postvec",
                ),
                (r.cli_binary.clone().unwrap(), "postvec-cli"),
            ];
            owned
                .iter()
                .find(|(owned, _)| path.starts_with(owned))
                .map(|(_, package)| Owner::Package(PackageManager::Dpkg, (*package).to_string()))
                .unwrap_or(Owner::Unowned)
        }
    }

    fn rel(dir: &Path, path: &Path) -> String {
        path.strip_prefix(dir).unwrap().display().to_string()
    }

    #[test]
    fn a_packaged_host_deletes_only_what_no_package_owns_and_names_the_packages() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let gathered = gather(&r);
        assert!(gathered.uncertain.is_empty(), "{:?}", gathered.uncertain);
        let owners = owners_for(
            &gathered,
            r.cli_binary.as_ref().unwrap(),
            debian_rule(dir.path()),
        );
        let plan = resolve(gathered, r.cli_binary.as_deref(), &owners);

        let removed: Vec<String> = plan
            .remove
            .iter()
            .map(|c| rel(dir.path(), &c.path))
            .collect();
        for expected in [
            "opt/postvec/models/onnx-runtime/pulled",
            "opt/postvec/models/onnx-runtime/manual",
            "opt/postvec/models/.staging",
            "etc/postvec/providers.d/openai.toml",
            "var/lib/postvec/clusters/18-main.json",
            "var/lib/postvec/auth.json",
        ] {
            assert!(
                removed.contains(&expected.to_string()),
                "missing {expected} in {removed:?}"
            );
        }
        for kept in [
            "opt/postvec/models/onnx-runtime/bundled",
            "opt/postvec/models/onnx-runtime/not-a-model",
            "opt/postvec/models/.postvec.lock",
            "opt/postvec/libs",
            "etc/postvec/providers.d/bedrock.key",
            "usr/lib/postgresql/18/lib/postvec.so",
            "usr/share/postgresql/18/extension/postvec.control",
            "usr/share/postgresql/18/extension/postvec--0.1.0.sql",
            "usr/share/postgresql/18/extension/vector.control",
            "usr/local/bin/postvec",
        ] {
            assert!(
                !removed.contains(&kept.to_string()),
                "{kept} must not be deleted"
            );
        }
        let packages: Vec<&str> = plan
            .packaged
            .keys()
            .map(|(_, name)| name.as_str())
            .collect();
        assert_eq!(
            packages,
            [
                "postgresql-18-postvec",
                "postvec-cli",
                "postvec-model-minilm-l6-v2",
                "postvec-onnxruntime"
            ]
        );
        assert!(plan.incomplete().is_empty(), "{:?}", plan.incomplete());
        let note = plan.package_note().unwrap();
        assert!(
            note.ends_with(
                "sudo apt purge postgresql-18-postvec postvec-cli postvec-model-minilm-l6-v2 \
                 postvec-onnxruntime"
            ),
            "{note}"
        );
        let notes = plan.all_notes().join("\n");
        assert!(
            notes.contains("bedrock.key") && notes.contains("left alone"),
            "{notes}"
        );
        assert!(
            notes.contains("not-a-model") && notes.contains("no ninference.hub.json"),
            "{notes}"
        );
        assert!(plan.model_lock.is_some());
        // The pulled model was enumerated to its leaves, nested directory
        // included.
        let pulled = plan
            .remove
            .iter()
            .find(|c| c.path.ends_with("pulled"))
            .unwrap();
        assert_eq!(pulled.leaves.len(), 4, "{:?}", pulled.leaves);
        assert!(pulled.complete);
    }

    /// The root registry login is an ordinary candidate of every purge:
    /// planned, package-checked, deleted — never silently retained.
    #[test]
    fn the_registry_login_is_swept() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let auth = r.state_dir.join("auth.json");
        let gathered = gather(&r);
        let login = gathered
            .candidates
            .iter()
            .find(|c| c.path == auth)
            .expect("the login is a candidate");
        assert!(login.what.contains("registry login"), "{}", login.what);
        assert!(!gathered.notes.iter().any(|n| n.contains("auth.json")));
    }

    /// Origin never exempts a path from the package check: a receipt-bearing
    /// model, a connector file and the CLI's own state are all asked about.
    #[test]
    fn every_candidate_is_asked_about_including_receipts_and_state() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let gathered = gather(&r);
        let state = r.state_dir.join("clusters/18-main.json");
        let toml = r.providers_dir.clone().unwrap().join("openai.toml");
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |path| {
            if path == state || path == toml {
                Owner::Package(PackageManager::Dpkg, "weird-pkg".into())
            } else {
                Owner::Unowned
            }
        });
        let plan = resolve(gathered, None, &owners);
        assert!(!plan
            .remove
            .iter()
            .any(|c| c.path == state || c.path == toml));
        assert_eq!(
            plan.packaged[&(PackageManager::Dpkg, "weird-pkg".to_string())].len(),
            2
        );
        // And with no answers at all, nothing is deleted.
        let plan = resolve(gather(&r), None, &BTreeMap::new());
        assert!(plan.remove.is_empty(), "{:?}", plan.remove);
        assert!(!plan.incomplete().is_empty());
    }

    #[test]
    fn one_packaged_leaf_below_a_tree_retains_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let root = r.engine_root.clone().unwrap();
        let gathered = gather(&r);
        let packaged_file = root.join("models/onnx-runtime/pulled/weights/model.onnx");
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |path| {
            if path == packaged_file {
                Owner::Package(PackageManager::Dpkg, "some-pkg".into())
            } else {
                Owner::Unowned
            }
        });
        let plan = resolve(gathered, None, &owners);
        assert!(!plan.remove.iter().any(|c| c.path.ends_with("pulled")));
        assert_eq!(
            plan.packaged[&(PackageManager::Dpkg, "some-pkg".to_string())],
            [root.join("models/onnx-runtime/pulled")]
        );
    }

    #[test]
    fn an_unknown_ownership_answer_retains_the_path_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let gathered = gather(&r);
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |path| {
            if path.ends_with("postvec.so") {
                Owner::Unknown("dpkg timed out".into())
            } else {
                Owner::Unowned
            }
        });
        let plan = resolve(gathered, None, &owners);
        assert!(!plan.remove.iter().any(|c| c.path.ends_with("postvec.so")));
        assert_eq!(plan.unresolved.len(), 1);
        assert!(plan
            .incomplete()
            .iter()
            .any(|n| n.contains("postvec.so") && n.contains("dpkg timed out")));
    }

    /// Leaf-wise deletion: a child that appears after classification keeps
    /// its directory (rmdir refuses), the classified leaves still go, and
    /// the outcome is reported — never a recursive delete of the unknown.
    #[test]
    fn a_child_added_after_classification_survives_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let pulled = r
            .engine_root
            .clone()
            .unwrap()
            .join("models/onnx-runtime/pulled");
        let planned = candidate(pulled.clone(), "model".into()).unwrap();
        touch(&pulled.join("weights/late-arrival.bin"));
        let problems = delete_candidate(&planned);
        assert!(!problems.is_empty(), "the late child must be reported");
        assert!(pulled.join("weights/late-arrival.bin").exists());
        assert!(pulled.join("weights").exists());
        assert!(!pulled.join("weights/model.onnx").exists());
        assert!(!pulled.join("ninference.hub.json").exists());
        assert!(
            pulled.exists(),
            "the directory holding the unknown child stays"
        );
        assert!(
            problems.iter().any(|p| p.contains("weights")),
            "{problems:?}"
        );
    }

    /// A leaf replaced after classification (new inode) is not deleted.
    /// A directory component swapped for a symlink after classification is
    /// refused by the `O_NOFOLLOW` descent; the link target survives.
    #[test]
    fn a_symlinked_component_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        touch(&root.join("sub/f"));
        let planned = candidate(root.clone(), "model".into()).unwrap();
        let outside = dir.path().join("outside");
        touch(&outside.join("f"));
        fs::remove_file(root.join("sub/f")).unwrap();
        fs::remove_dir(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();
        let problems = delete_candidate(&planned);
        assert!(outside.join("f").exists(), "the link target was followed");
        assert!(!problems.is_empty(), "the swap must be reported");
    }

    #[test]
    fn a_replaced_leaf_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        touch(&root.join("a"));
        let planned = candidate(root.clone(), "model".into()).unwrap();
        let other = dir.path().join("b");
        touch(&other);
        fs::rename(&other, root.join("a")).unwrap();
        let problems = delete_candidate(&planned);
        assert!(root.join("a").exists());
        assert!(
            problems.iter().any(|p| p.contains("changed")),
            "{problems:?}"
        );
    }

    /// An unreadable directory below a candidate makes it uncertain: never
    /// deleted, and the plan is partial.
    #[test]
    fn an_unreadable_subdirectory_retains_the_candidate() {
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let sealed = r
            .engine_root
            .clone()
            .unwrap()
            .join("models/onnx-runtime/pulled/weights");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        let gathered = gather(&r);
        let pulled = gathered
            .candidates
            .iter()
            .find(|c| c.path.ends_with("pulled"))
            .unwrap();
        assert!(!pulled.complete);
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |_| {
            Owner::Unowned
        });
        let plan = resolve(gathered, None, &owners);
        assert!(!plan.remove.iter().any(|c| c.path.ends_with("pulled")));
        assert!(plan
            .incomplete()
            .iter()
            .any(|n| n.contains("pulled") && n.contains("fully inspected")));
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Drives the real `apply` (locks, re-plan, leaf-wise delete, prune) on
    /// a host without a package database.
    #[tokio::test]
    async fn a_manual_install_is_swept_completely_and_the_running_binary_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let mut r = roots(dir.path());
        let gathered = gather(&r);
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |_| {
            Owner::Unowned
        });
        let plan = resolve(gathered, r.cli_binary.as_deref(), &owners);
        assert!(plan.packaged.is_empty());
        let removed: Vec<&Path> = plan.remove.iter().map(|c| c.path.as_path()).collect();
        assert!(removed.contains(&r.pkglibdir.join("postvec.so").as_path()));
        assert!(removed.contains(&r.sharedir.join("extension/postvec--0.1.0.sql").as_path()));
        assert!(!removed.contains(&r.sharedir.join("extension/vector.control").as_path()));
        assert!(removed.contains(&r.engine_root.clone().unwrap().join("libs").as_path()));
        assert!(plan.notes.iter().any(|n| n.contains("running program")));

        if !PackageManager::all_installed().is_empty() {
            // With a real package database the re-plan asks it about the
            // temp files; the lock/permission shape of a tempdir differs per
            // host. The deletion path is covered by the leaf-wise tests.
            return;
        }
        let mut journal = ApplyJournal::default();
        apply(&plan, &mut r, Duration::from_secs(5), &mut journal).await;
        assert!(journal.incomplete.is_empty(), "{:?}", journal.incomplete);
        for gone in [
            r.engine_root.clone().unwrap(),
            r.pkglibdir.join("postvec.so"),
            r.state_dir.join("clusters"),
        ] {
            assert!(!gone.exists(), "{} was left behind", gone.display());
        }
        assert!(
            !r.state_dir.join("auth.json").exists(),
            "the registry login is swept with the purge"
        );
        assert!(r
            .providers_dir
            .clone()
            .unwrap()
            .join("bedrock.key")
            .exists());
        assert!(r.sharedir.join("extension/vector.control").exists());
        assert!(r.cli_binary.clone().unwrap().exists());
    }

    #[test]
    fn a_foreign_preload_keeps_the_extension_files() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let mut r = roots(dir.path());
        r.extension_files_removable = false;
        let gathered = gather(&r);
        assert!(!gathered
            .candidates
            .iter()
            .any(|c| c.path.ends_with("postvec.so") || c.path.ends_with("postvec.control")));
        assert!(gathered
            .notes
            .iter()
            .any(|n| n.contains("extension files were left in place")));
    }

    #[test]
    fn a_running_server_or_an_excluded_root_keeps_the_engine_root() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let mut r = roots(dir.path());
        r.engine_root_in_use = true;
        let gathered = gather(&r);
        let root = r.engine_root.clone().unwrap();
        assert!(!gathered
            .candidates
            .iter()
            .any(|c| c.path.starts_with(&root)));
        assert!(gathered.notes.iter().any(|n| n.contains("postvec-server")));
        assert!(gathered
            .candidates
            .iter()
            .any(|c| c.path.ends_with("18-main.json")));

        let mut r = roots(dir.path());
        r.engine_root = None;
        r.excluded
            .push("postvec.path comes from postgresql.auto.conf".into());
        let gathered = gather(&r);
        assert!(!gathered
            .candidates
            .iter()
            .any(|c| c.path.starts_with(&root)));
        assert!(gathered
            .notes
            .iter()
            .any(|n| n.contains("postgresql.auto.conf")));
    }

    #[test]
    fn a_server_providers_directory_under_the_root_is_reported_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let root = r.engine_root.clone().unwrap();
        touch(&root.join("providers.d/univec.toml"));
        let gathered = gather(&r);
        assert!(!gathered
            .candidates
            .iter()
            .any(|c| c.path.starts_with(root.join("providers.d"))));
        assert!(gathered
            .notes
            .iter()
            .any(|n| n.contains("providers.d") && n.contains("left alone")));
    }

    /// A held serving lease (postvec-server's shared side) refuses the
    /// exclusive acquisition; the file itself is recognized, never a
    /// candidate.
    #[test]
    fn a_served_engine_root_refuses_the_lease() {
        // The stable-inode contract with postvec-server: same derivation on
        // both sides, pinned by the same literal in each crate's tests.
        assert_eq!(
            serving_lease_path(Path::new("/opt/postvec")),
            PathBuf::from(
                "/run/lock/postvec/engine-616ab489616db613212540e426e1245d5dd61ba6bbed138f9cb9ae20c03b6166.lease"
            )
        );
        // Fixed-length whatever the root: a near-PATH_MAX root must not hit
        // NAME_MAX (the raw-path encoding did, at 121 bytes).
        let long = format!("/srv/{}", "x".repeat(3900));
        let name = serving_lease_path(Path::new(&long));
        assert_eq!(
            name.file_name().unwrap().len(),
            "engine-".len() + 64 + ".lease".len()
        );
        // Non-UTF-8 path bytes hash like any others.
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/srv/pv-\xff\xfe");
        let _ = serving_lease_path(Path::new(raw));
        // Distinct roots get distinct keys; equal roots the same one.
        assert_ne!(
            serving_lease_path(Path::new("/opt/postvec")),
            serving_lease_path(Path::new("/opt/postvec2"))
        );
        assert_eq!(
            serving_lease_path(Path::new("/opt/postvec")),
            serving_lease_path(Path::new("/opt/postvec"))
        );
        // A canonical alias (symlink spelling) maps to the same key because
        // every caller canonicalizes before deriving.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            serving_lease_path(&link.canonicalize().unwrap()),
            serving_lease_path(&real.canonicalize().unwrap())
        );

        // /run/lock is world-writable+sticky on Linux, so an unprivileged
        // test can exercise the real path with a tempdir-derived name. The
        // file is deliberately never unlinked (tmpfs; gone at reboot).
        let root = dir.path().canonicalize().unwrap();
        let lease_path = serving_lease_path(&root);
        if fs::create_dir_all(lease_path.parent().unwrap()).is_err() {
            return; // no /run/lock on this host (exotic CI); nothing to prove
        }

        // Hold the shared side the way a serving postvec-server does; the
        // sweep's exclusive side must be refused (flock domains are per open
        // file description, so one process can prove the conflict).
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)
            .unwrap();
        assert_eq!(
            unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&held),
                    libc::LOCK_SH | libc::LOCK_NB,
                )
            },
            0
        );
        let refused = acquire_serving_lease(&root);
        assert!(refused.is_err(), "the exclusive lease must be refused");
        assert!(refused.unwrap_err().to_string().contains("serving lease"));
        drop(held);
        assert!(acquire_serving_lease(&root).is_ok());
        assert!(lease_path.exists(), "the lease inode is never unlinked");

        // The exclusive side needs a writable open (HostLock): a lease this
        // account cannot write — root-owned on a real host, chmod-simulated
        // here — is a refusal, never a bypass.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped (unwritable-lease refusal): root bypasses file modes");
        } else {
            fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o444)).unwrap();
            assert!(acquire_serving_lease(&root).is_err());
            fs::set_permissions(&lease_path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn the_provider_lock_file_is_neither_a_candidate_nor_a_stranger() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let providers = r.providers_dir.clone().unwrap();
        touch(&providers.join(".lock"));
        let gathered = gather(&r);
        assert_eq!(gathered.provider_lock, Some(providers.join(".lock")));
        assert!(!gathered
            .candidates
            .iter()
            .any(|c| c.path.ends_with(".lock")));
        assert!(!gathered.notes.iter().any(|n| n.contains("/.lock")));
    }

    #[test]
    fn another_configured_cluster_refuses_the_purge() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        touch(&r.state_dir.join("clusters/17-main.json"));
        assert_eq!(
            other_clusters_configured(&r.state_dir, "18-main").unwrap(),
            ["17-main".to_string()]
        );
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(plan(&r, Duration::from_secs(1)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("17-main"), "{error}");
    }

    #[test]
    fn symlinks_are_never_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        touch(&outside.join("ninference.hub.json"));
        let r = roots(dir.path());
        let root = r.engine_root.clone().unwrap();
        fs::create_dir_all(root.join("models/onnx-runtime")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("models/onnx-runtime/linked")).unwrap();
        let providers = r.providers_dir.clone().unwrap();
        fs::create_dir_all(&providers).unwrap();
        std::os::unix::fs::symlink(
            outside.join("ninference.hub.json"),
            providers.join("evil.toml"),
        )
        .unwrap();
        let gathered = gather(&r);
        assert!(gathered.candidates.is_empty(), "{:?}", gathered.candidates);
        assert!(gathered.notes.iter().any(|n| n.contains("linked")));
        assert!(gathered.notes.iter().any(|n| n.contains("evil.toml")));
    }

    #[test]
    fn system_and_shallow_and_symlinked_roots_are_refused() {
        for bad in [
            "/",
            "/etc",
            "/opt",
            "/usr/lib",
            "/var/lib",
            "/postvec",
            "relative/x",
        ] {
            assert!(
                safe_root(Path::new(bad), "root", None).is_err(),
                "{bad} accepted"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("a/postvec");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("a/link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = safe_root(&link, "root", None).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        // A group-writable root is refused.
        fs::set_permissions(&real, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(safe_root(&real.canonicalize().unwrap(), "root", None).is_err());
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        // A group-writable *ancestor* is refused too (no sticky bit).
        let parent = dir.path().join("a");
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o775)).unwrap();
        let error = safe_root(&real.canonicalize().unwrap(), "root", None).unwrap_err();
        assert!(error.contains("ancestor"), "{error}");
    }

    /// One owner policy for the leaf and every ancestor: root, the effective
    /// user, or the cluster account.
    #[test]
    fn the_owner_policy_admits_root_the_caller_and_the_cluster_account_only() {
        let me = unsafe { libc::geteuid() };
        assert!(trusted_uid(0, me, None));
        assert!(trusted_uid(me, me, None));
        assert!(!trusted_uid(me + 1, me, None));
        assert!(trusted_uid(me + 1, me, Some(me + 1)));
        assert!(!trusted_uid(me + 2, me, Some(me + 1)));
        // A real chain owned by the effective user passes end to end (the
        // tempdir's own ancestors are root-owned and sticky, or ours).
        let dir = tempfile::tempdir().unwrap();
        // The tempdir follows the host umask (0775 on some boxes) and would
        // rightly be refused as a group-writable ancestor: pin it.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let real = dir.path().join("etc-postvec/providers.d");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(real.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        let verdict = safe_root(&real.canonicalize().unwrap(), "providers directory", None);
        assert!(verdict.is_ok(), "{verdict:?}");
    }

    #[test]
    fn dpkg_and_rpm_batches_parse_fail_closed() {
        let a = PathBuf::from("/opt/postvec/libs/onnxruntime/lib/libonnxruntime.so");
        let b = PathBuf::from("/opt/postvec/models/x/ninference.hub.json");
        let c = PathBuf::from("/usr/bin/postvec");
        let queried = [a.clone(), b.clone(), c.clone()];
        let answers = parse_owners(
            PackageManager::Dpkg,
            &queried,
            "diversion by dash from: /bin/sh\n\
             postvec-onnxruntime: /opt/postvec/libs/onnxruntime/lib/libonnxruntime.so\n\
             postvec-cli:amd64, other: /usr/bin/postvec\n",
            "dpkg-query: no path found matching pattern \
             /opt/postvec/models/x/ninference.hub.json\n",
        );
        assert_eq!(
            answers[&a],
            Owner::Package(PackageManager::Dpkg, "postvec-onnxruntime".into())
        );
        assert_eq!(answers[&b], Owner::Unowned);
        assert_eq!(
            answers[&c],
            Owner::Package(PackageManager::Dpkg, "postvec-cli".into())
        );
        let answers = parse_owners(PackageManager::Dpkg, &queried, "", "");
        assert!(queried
            .iter()
            .all(|p| matches!(answers[p], Owner::Unknown(_))));

        let answers = parse_owners(
            PackageManager::Rpm,
            &queried,
            "postvec-onnxruntime-1.0-1.el9.x86_64\n\
             file /opt/postvec/models/x/ninference.hub.json is not owned by any package\n\
             postvec-cli-0.1.0-1.el9.x86_64\n",
            "",
        );
        assert_eq!(
            answers[&a],
            Owner::Package(
                PackageManager::Rpm,
                "postvec-onnxruntime-1.0-1.el9.x86_64".into()
            )
        );
        assert_eq!(answers[&b], Owner::Unowned);
        let answers = parse_owners(PackageManager::Rpm, &queried, "one\n", "");
        assert!(queried
            .iter()
            .all(|p| matches!(answers[p], Owner::Unknown(_))));
    }
}
