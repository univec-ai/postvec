//! `postvec uninstall --all --purge` — the host-side sweep that follows the
//! SQL and configuration removal and a completed restart.
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
//! A root is used only if it passes [`safe_root`]: absolute, no symlinked
//! ancestor, not a system directory, at least two components deep, owned by a
//! trusted account and not group/world-writable. The caller additionally
//! proves the GUC naming it comes from the CLI-owned snippet or is the
//! built-in default (see `uninstall::purge_roots`), so a later override in
//! another configuration file cannot redirect the sweep.
//!
//! Package-owned files are **never** deleted, and ownership is decided
//! **fail-closed**: on a host with a dpkg/rpm database, a lookup that cannot
//! establish an answer — tool failure, timeout, unparseable output — retains
//! the path and makes the result partial. Every file below a directory is
//! looked up, not just the directory.
//!
//! The plan is built before confirmation and **rebuilt at apply time**: a
//! path is deleted only if it is still in the freshly computed plan with the
//! same identity (device and inode) it had when the operator confirmed it.
//! Deletion happens under the engine root's model-store lock, so a concurrent
//! `model pull`/`rm` cannot interleave.

use crate::config::owned;
use crate::error::{CliError, Result};
use crate::plan::{ApplyJournal, PlanStep};
use crate::proc::{self, Cmd};
use crate::registry::receipt::Receipt;
use crate::registry::root::ModelRoot;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The CLI's transient state beside the models, in the engine root. The
/// model-store lock file is deliberately absent: it is held during apply and
/// unlinked only after release (see [`apply`]).
const MODELS_STATE: &[&str] = &[".staging", ".trash", ".swap"];
const DESCRIPTOR_FILE: &str = "ninference.hub.json";
/// Root login state; never purged (see `PurgePlan::notes`).
const AUTH_FILE: &str = "auth.json";

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
    pub engine_root: Option<PathBuf>,
    /// A `postvec-server` process runs on this host: leave the engine root
    /// alone, it may be serving from it.
    pub engine_root_in_use: bool,
    pub providers_dir: Option<PathBuf>,
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
}

/// The answer to "does a package own this path?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Package(PackageManager, String),
    /// The package database was consulted and does not list the path.
    Unowned,
    /// No answer could be established; the path must be retained.
    Unknown(String),
}

/// One path the sweep considers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub what: String,
    /// Whether the package manager may own it. Receipt-backed models and
    /// the CLI's own state never do; everything else is asked.
    pub check_package: bool,
    /// `(device, inode)` at planning time; a deletion happens only against
    /// the same object.
    pub identity: (u64, u64),
}

/// The resolved sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    /// Paths to delete, in order.
    pub remove: Vec<Candidate>,
    /// Package-owned paths, grouped by package: reported, never deleted.
    pub packaged: BTreeMap<String, Vec<PathBuf>>,
    pub package_manager: Option<PackageManager>,
    /// Paths whose ownership could not be established: retained, and the
    /// result is partial.
    pub unresolved: Vec<(PathBuf, String)>,
    /// Directories to remove afterwards if they are empty, deepest first.
    pub prune_dirs: Vec<PathBuf>,
    /// The model-store lock file, unlinked last, after the lock is released.
    pub model_lock: Option<PathBuf>,
    /// Things the operator should know: what was skipped or retained and why.
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
        let packages: Vec<String> = self.packaged.keys().cloned().collect();
        let command = self
            .package_manager
            .map(|manager| manager.remove_command(&packages))
            .unwrap_or_else(|| "remove them with the package manager".to_string());
        Some(format!(
            "package-owned files were left in place ({}); remove the packages with: {command}",
            packages.join(", ")
        ))
    }

    /// Everything the operator should read beside the steps.
    pub fn all_notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        for (path, reason) in &self.unresolved {
            notes.push(format!(
                "{} was retained: package ownership could not be established ({reason})",
                path.display()
            ));
        }
        notes.extend(self.package_note());
        notes
    }
}

/// Is `root` a directory the sweep may operate in?
///
/// Absolute, canonical (no symlinked ancestor — a replaceable link lets
/// another account swap the whole tree between plan and apply), at least two
/// components below `/`, not one of the system directories, and owned by a
/// trusted account without group/world write (the same rule the model store
/// and providers loader apply to their own roots).
pub fn safe_root(root: &Path, what: &str) -> std::result::Result<(), String> {
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
    owned::check_trusted_dir(root, what).map_err(|e| e.to_string())?;
    owned::check_trusted_ancestry(root).map_err(|e| e.to_string())?;
    Ok(())
}

/// Refuse when another cluster on this host is still set up: the engine root
/// and the package files are shared, and that cluster would lose them.
pub fn other_clusters_configured(state_dir: &Path, cluster_key: &str) -> Vec<String> {
    let clusters = state_dir.join("clusters");
    let Ok(entries) = fs::read_dir(&clusters) else {
        return Vec::new();
    };
    let own = format!("{cluster_key}.json");
    let mut others: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".json") && *name != own)
        .map(|name| name.trim_end_matches(".json").to_string())
        .collect();
    others.sort();
    others
}

/// Is a `postvec-server` running on this host? It serves from the same
/// engine-root layout, so its models must not be swept from under it. A
/// container's process is visible here too, which is the conservative
/// direction.
pub fn server_process_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
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

fn identity_of(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

fn candidate(path: PathBuf, what: String, check_package: bool) -> Option<Candidate> {
    let identity = identity_of(&path)?;
    Some(Candidate {
        path,
        what,
        check_package,
        identity,
    })
}

/// Does this directory hold ONNX Runtime? Any regular file whose name starts
/// with `libonnxruntime`, at any depth (bounded).
fn holds_onnxruntime(libs: &Path) -> bool {
    fn walk(dir: &Path, depth: usize) -> bool {
        if depth > 6 {
            return false;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_dir() {
                if walk(&path, depth + 1) {
                    return true;
                }
            } else if metadata.is_file()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("libonnxruntime"))
            {
                return true;
            }
        }
        false
    }
    walk(libs, 0)
}

/// What `gather` found: deletion candidates, directories to prune when
/// empty, the model-store lock file, and notes.
pub struct Gathered {
    pub candidates: Vec<Candidate>,
    pub prune: Vec<PathBuf>,
    pub model_lock: Option<PathBuf>,
    pub notes: Vec<String>,
}

/// Everything the sweep would touch, before asking the package manager.
/// Pure filesystem reading; unreadable locations become notes, never errors,
/// because a purge that stops at the first odd directory leaves more behind
/// than one that reports it.
pub fn gather(roots: &PurgeRoots) -> Gathered {
    let mut candidates = Vec::new();
    let mut prune = Vec::new();
    let mut model_lock = None;
    let mut notes = roots.excluded.clone();

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
                        for entry in entries.filter_map(|e| e.ok()) {
                            let path = entry.path();
                            let name = entry.file_name().to_string_lossy().to_string();
                            let Ok(metadata) = fs::symlink_metadata(&path) else {
                                continue;
                            };
                            if name == crate::registry::root::LOCK_FILE {
                                model_lock = Some(path);
                            } else if MODELS_STATE.contains(&name.as_str()) {
                                candidates.extend(candidate(
                                    path,
                                    "postvec-cli model-store state".to_string(),
                                    false,
                                ));
                            } else if metadata.is_dir() {
                                gather_backend(
                                    &path,
                                    &name,
                                    &mut candidates,
                                    &mut prune,
                                    &mut notes,
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
                    Err(error) => notes.push(format!("cannot read {}: {error}", models.display())),
                }
                prune.push(models);
            }
            let libs = root.join("libs");
            if let Ok(metadata) = fs::symlink_metadata(&libs) {
                if metadata.is_dir() && holds_onnxruntime(&libs) {
                    candidates.extend(candidate(libs, "ONNX Runtime libraries".to_string(), true));
                } else {
                    notes.push(format!(
                        "{} does not hold ONNX Runtime and was left alone",
                        libs.display()
                    ));
                }
            }
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.filter_map(|e| e.ok()) {
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
                                false,
                            )),
                            _ => notes.push(format!(
                                "{} is not a regular file and was left alone",
                                path.display()
                            )),
                        }
                    }
                    if let Ok(entries) = fs::read_dir(dir) {
                        for entry in entries.filter_map(|e| e.ok()) {
                            let path = entry.path();
                            if !connectors.contains(&path) {
                                notes.push(format!(
                                    "{} is not a connector file this CLI writes and was left \
                                     alone",
                                    path.display()
                                ));
                            }
                        }
                    }
                }
                Err(problem) => notes.push(format!("{problem}; provider files may remain")),
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
        candidates.extend(candidate(
            state,
            "postvec-cli cluster state".to_string(),
            false,
        ));
    }
    if roots.state_dir.exists() {
        prune.push(roots.state_dir.join("clusters"));
        prune.push(roots.state_dir.clone());
        let auth = roots.state_dir.join(AUTH_FILE);
        if auth.exists() {
            notes.push(format!(
                "{} (the registry login) was kept — it may serve other hosts or workflows; \
                 `postvec logout` removes it",
                auth.display()
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
    if let Ok(entries) = fs::read_dir(roots.sharedir.join("extension")) {
        let mut scripts: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("postvec--") && n.ends_with(".sql"))
            })
            .collect();
        scripts.sort();
        extension_files.extend(
            scripts
                .into_iter()
                .map(|p| (p, "extension SQL script".to_string())),
        );
    }
    let present: Vec<(PathBuf, String)> = extension_files
        .into_iter()
        .filter(|(path, _)| fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
        .collect();
    if !present.is_empty() && !roots.extension_files_removable {
        notes.push(format!(
            "shared_preload_libraries still names postvec from configuration this CLI does not \
             own, so the extension files were left in place: {}",
            present
                .iter()
                .map(|(path, _)| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        for (path, what) in present {
            candidates.extend(candidate(path, what, true));
        }
    }

    Gathered {
        candidates,
        prune,
        model_lock,
        notes,
    }
}

fn gather_backend(
    backend_dir: &Path,
    backend: &str,
    candidates: &mut Vec<Candidate>,
    prune: &mut Vec<PathBuf>,
    notes: &mut Vec<String>,
) {
    match fs::read_dir(backend_dir) {
        Ok(entries) => {
            let mut models: Vec<PathBuf> =
                entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
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
                    // A receipt proves the CLI installed it; anything else
                    // may be a package's.
                    !receipt,
                ));
            }
        }
        Err(error) => notes.push(format!("cannot read {}: {error}", backend_dir.display())),
    }
    prune.push(backend_dir.to_path_buf());
}

/// Every entry below `path` (and `path` itself), so package ownership is
/// established for what `remove_dir_all` would actually remove.
fn files_below(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let child = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&child) else {
                continue;
            };
            out.push(child.clone());
            if metadata.is_dir() {
                stack.push(child);
            }
        }
    }
    out
}

/// Split the gathered candidates by package ownership. `owners` answers
/// "which package claims this path?" for every path below a candidate — the
/// real one shells out to dpkg/rpm, tests use a table. A path with no answer
/// is unknown, never unowned.
pub fn resolve(
    gathered: Gathered,
    cli_binary: Option<&Path>,
    owners: &BTreeMap<PathBuf, Owner>,
) -> PurgePlan {
    let mut plan = PurgePlan {
        prune_dirs: gathered.prune,
        model_lock: gathered.model_lock,
        notes: gathered.notes,
        ..PurgePlan::default()
    };
    let lookup = |path: &Path| -> Owner {
        owners
            .get(path)
            .cloned()
            .unwrap_or_else(|| Owner::Unknown("no ownership answer was recorded".to_string()))
    };
    for candidate in gathered.candidates {
        if !candidate.check_package {
            plan.remove.push(candidate);
            continue;
        }
        // The verdict for a tree: any packaged file makes it packaged (every
        // owning package is named); otherwise any unknown makes it retained;
        // only an all-unowned tree is deleted.
        let mut packages: BTreeSet<(PackageManager, String)> = BTreeSet::new();
        let mut unknown: Option<String> = None;
        for path in files_below(&candidate.path) {
            match lookup(&path) {
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
                plan.package_manager.get_or_insert(manager);
                plan.packaged
                    .entry(package)
                    .or_default()
                    .push(candidate.path.clone());
            }
        } else if let Some(reason) = unknown {
            plan.unresolved.push((candidate.path, reason));
        } else {
            plan.remove.push(candidate);
        }
    }
    if let Some(binary) = cli_binary {
        match lookup(binary) {
            Owner::Package(manager, package) => {
                plan.package_manager.get_or_insert(manager);
                plan.packaged
                    .entry(package)
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

/// Ask the package manager who owns each path. One answer per input path.
///
/// Fail-closed: with a dpkg or rpm database on the host, anything short of an
/// authoritative answer is `Unknown`. Without either tool at its fixed
/// location the host has no package database and every path is `Unowned`.
pub async fn package_owners(paths: &[PathBuf], timeout: Duration) -> BTreeMap<PathBuf, Owner> {
    let mut answers = BTreeMap::new();
    let manager = [PackageManager::Dpkg, PackageManager::Rpm]
        .into_iter()
        .find_map(|manager| manager.installed().map(|binary| (manager, binary)));
    let Some((manager, binary)) = manager else {
        for path in paths {
            answers.insert(path.clone(), Owner::Unowned);
        }
        return answers;
    };
    // Bounded argument lists; a model tree is a handful of files, libs a few
    // dozen, so this is one or two invocations in practice.
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
        answers.extend(outcome);
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
    let others = other_clusters_configured(&roots.state_dir, &roots.cluster_key);
    if !others.is_empty() {
        return Err(CliError::precondition(format!(
            "--purge refused: {} on this host still {} set up by this CLI, and the engine root \
             and extension files are shared",
            others.join(", "),
            if others.len() == 1 { "is" } else { "are" }
        ))
        .with_fix(
            "run `postvec --cluster <that cluster> uninstall --all` for each of them first, \
             then purge from the last one",
        ));
    }
    let gathered = gather(roots);
    let mut to_query: Vec<PathBuf> = gathered
        .candidates
        .iter()
        .filter(|c| c.check_package)
        .flat_map(|c| files_below(&c.path))
        .collect();
    to_query.extend(roots.cli_binary.iter().cloned());
    to_query.sort();
    to_query.dedup();
    let owners = package_owners(&to_query, timeout).await;
    Ok(resolve(gathered, roots.cli_binary.as_deref(), &owners))
}

/// Apply a confirmed plan: rebuild it now, delete only what is still in it
/// with the identity the operator confirmed, under the engine root's
/// model-store lock, then prune the directories that emptied out.
///
/// A path that cannot be removed is recorded as incomplete and the sweep
/// continues: stopping would leave *more* behind, not less. A path that is no
/// longer in the fresh plan, or whose device/inode changed, is skipped as
/// changed-since-confirmation. New candidates in the fresh plan are not
/// deleted: nobody confirmed them.
pub async fn apply(
    confirmed: &PurgePlan,
    roots: &mut PurgeRoots,
    timeout: Duration,
    journal: &mut ApplyJournal,
) {
    // The guards that ran at planning time run again now.
    roots.engine_root_in_use = server_process_running();
    let fresh = match plan(roots, timeout).await {
        Ok(fresh) => fresh,
        Err(error) => {
            journal.incomplete(format!("--purge stopped before deleting anything: {error}"));
            return;
        }
    };

    // Serialize against model pull/rm/activate on this root for the whole
    // deletion phase. Without the lock nothing under the root is touched.
    let model_lock = match &roots.engine_root {
        Some(root) if root.join("models").is_dir() && !roots.engine_root_in_use => {
            match ModelRoot::new(root.clone()).lock_exclusive() {
                Ok(lock) => Some(lock),
                Err(error) => {
                    journal.incomplete(format!(
                        "the engine root {} was left alone: {error}",
                        root.display()
                    ));
                    None
                }
            }
        }
        _ => None,
    };
    let engine_root = roots.engine_root.clone();
    let under_engine_root = |path: &Path| {
        engine_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
    };
    let engine_root_locked = model_lock.is_some();
    let still_planned: BTreeMap<&Path, &Candidate> =
        fresh.remove.iter().map(|c| (c.path.as_path(), c)).collect();

    for candidate in &confirmed.remove {
        if under_engine_root(&candidate.path) && !engine_root_locked {
            continue;
        }
        match still_planned.get(candidate.path.as_path()) {
            Some(current) if current.identity == candidate.identity => {}
            Some(_) => {
                journal.incomplete(format!(
                    "{} changed since the plan was confirmed (different file); not deleted",
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
        match remove_path(&candidate.path, candidate.identity) {
            Ok(()) => journal.record(format!(
                "deleted {} ({})",
                candidate.path.display(),
                candidate.what
            )),
            Err(error) => journal.incomplete(format!(
                "could not delete {} ({}): {error}",
                candidate.path.display(),
                candidate.what
            )),
        }
    }
    for (path, reason) in &fresh.unresolved {
        journal.incomplete(format!(
            "{} was retained: package ownership could not be established ({reason})",
            path.display()
        ));
    }

    // The model-store lock is released before its file goes: a held lock is
    // never unlinked from under a concurrent waiter.
    drop(model_lock);
    if let Some(lock) = fresh.model_lock.as_ref().filter(|_| engine_root_locked) {
        match fs::remove_file(lock) {
            Ok(()) => journal.record(format!("deleted {} (model-store lock)", lock.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                journal.incomplete(format!("could not delete {}: {error}", lock.display()))
            }
        }
    }
    for dir in &fresh.prune_dirs {
        if under_engine_root(dir) && !engine_root_locked {
            continue;
        }
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
}

/// Remove a file or a directory tree whose identity still matches. A
/// symlink is never a candidate (`gather` requires regular files and real
/// directories); `remove_dir_all` does not traverse symlinks below the root.
fn remove_path(path: &Path, expected: (u64, u64)) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if (metadata.dev(), metadata.ino()) != expected {
        return Err(std::io::Error::other(
            "the object at this path changed since it was planned",
        ));
    }
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn roots(dir: &Path) -> PurgeRoots {
        PurgeRoots {
            engine_root: Some(dir.join("opt/postvec")),
            engine_root_in_use: false,
            providers_dir: Some(dir.join("etc/postvec/providers.d")),
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
    }

    fn owners_for(
        gathered: &Gathered,
        binary: &Path,
        rule: impl Fn(&Path) -> Owner,
    ) -> BTreeMap<PathBuf, Owner> {
        let mut map = BTreeMap::new();
        for candidate in gathered.candidates.iter().filter(|c| c.check_package) {
            for path in files_below(&candidate.path) {
                map.insert(path.clone(), rule(&path));
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
            "var/lib/postvec/auth.json",
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
        let packages: Vec<&String> = plan.packaged.keys().collect();
        assert_eq!(
            packages,
            [
                "postgresql-18-postvec",
                "postvec-cli",
                "postvec-model-minilm-l6-v2",
                "postvec-onnxruntime"
            ]
        );
        assert!(plan.unresolved.is_empty());
        let note = plan.package_note().unwrap();
        assert!(
            note.ends_with(
                "sudo apt purge postgresql-18-postvec postvec-cli postvec-model-minilm-l6-v2 \
                 postvec-onnxruntime"
            ),
            "{note}"
        );
        // The operator's key file and the lookalike directory are reported,
        // and the login is named as kept.
        let notes = plan.all_notes().join("\n");
        assert!(
            notes.contains("bedrock.key") && notes.contains("left alone"),
            "{notes}"
        );
        assert!(
            notes.contains("not-a-model") && notes.contains("no ninference.hub.json"),
            "{notes}"
        );
        assert!(
            notes.contains("auth.json") && notes.contains("postvec logout"),
            "{notes}"
        );
        assert!(plan.model_lock.is_some());
    }

    #[test]
    fn one_packaged_file_below_a_tree_retains_the_whole_tree() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let root = r.engine_root.clone().unwrap();
        let gathered = gather(&r);
        let packaged_file = root.join("models/onnx-runtime/manual/ninference.hub.json");
        let owners = owners_for(&gathered, r.cli_binary.as_ref().unwrap(), |path| {
            if path == packaged_file {
                Owner::Package(PackageManager::Dpkg, "some-pkg".into())
            } else {
                Owner::Unowned
            }
        });
        let plan = resolve(gathered, None, &owners);
        assert!(!plan.remove.iter().any(|c| c.path.ends_with("manual")));
        assert_eq!(
            plan.packaged["some-pkg"],
            [root.join("models/onnx-runtime/manual")]
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
            .all_notes()
            .iter()
            .any(|n| n.contains("postvec.so") && n.contains("dpkg timed out")));
        // A path with no recorded answer at all is unknown too — never
        // silently unowned.
        let plan = resolve(gather(&r), None, &BTreeMap::new());
        assert!(plan.remove.iter().all(|c| !c.check_package));
        assert!(!plan.unresolved.is_empty());
    }

    /// Drives the real `apply` (re-plan, lock, identity check, prune) on a
    /// host without a package database, which is the one configuration
    /// where the shell-out cannot influence the verdict.
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

        if PackageManager::Dpkg.installed().is_some() || PackageManager::Rpm.installed().is_some() {
            // With a real package database the re-plan asks it about the
            // temp files; the answer is "unowned" but the lock/permission
            // shape of a tempdir differs per host. The apply path is covered
            // by the identity and prune tests instead.
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
        assert!(r.state_dir.join("auth.json").exists());
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
            .any(|n| n.contains("shared_preload_libraries")));
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

    #[test]
    fn another_configured_cluster_refuses_the_purge() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        touch(&r.state_dir.join("clusters/17-main.json"));
        assert_eq!(
            other_clusters_configured(&r.state_dir, "18-main"),
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
    fn a_changed_inode_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        touch(&file);
        let planned = identity_of(&file).unwrap();
        // Two live files cannot share an inode; renaming one over the other
        // is a guaranteed identity change (delete+recreate may reuse it).
        let other = dir.path().join("g");
        touch(&other);
        fs::rename(&other, &file).unwrap();
        assert_ne!(identity_of(&file).unwrap(), planned);
        let error = remove_path(&file, planned).unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
        assert!(file.exists());
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
            assert!(safe_root(Path::new(bad), "root").is_err(), "{bad} accepted");
        }
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("a/postvec");
        fs::create_dir_all(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("a/link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = safe_root(&link, "root").unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        // A group-writable root is refused by the trusted-dir rule.
        fs::set_permissions(&real, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(safe_root(&real.canonicalize().unwrap(), "root").is_err());
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
        // Silence about a path is unknown, not unowned.
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
        // A short answer makes the batch unknown.
        let answers = parse_owners(PackageManager::Rpm, &queried, "one\n", "");
        assert!(queried
            .iter()
            .all(|p| matches!(answers[p], Owner::Unknown(_))));
    }
}
