//! `postvec uninstall --all --purge` — the host-side sweep that follows the
//! SQL and configuration removal.
//!
//! What it deletes is exactly what postvec put on the host outside the
//! package manager: models pulled (or copied) into the engine root and the
//! CLI's staging/trash/lock state beside them, external-provider connector
//! files, the CLI's per-cluster state and lock files, and an extension
//! library/control/SQL set that no package owns (a manual install). Two rules
//! shape it:
//!
//! - **Package-owned files are never deleted.** Deleting under dpkg/rpm's
//!   feet leaves a package the manager believes is installed with half its
//!   files gone. The sweep names the packages instead and prints the exact
//!   `apt purge`/`dnf remove` line.
//! - **Nothing is deleted that another consumer may still be using.** Another
//!   cluster's state on this host refuses the purge outright (engine roots are
//!   shared), and a running `postvec-server` process keeps the engine root
//!   intact — it serves models from the same layout.
//!
//! Everything is planned first, rendered as `RemovePath` steps the operator
//! confirms, and only then applied.

use crate::error::{CliError, Result};
use crate::plan::{ApplyJournal, PlanStep};
use crate::proc::{self, Cmd};
use crate::registry::receipt::Receipt;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The CLI's transient state beside the models, in the engine root.
const MODELS_STATE: &[&str] = &[
    ".staging",
    ".trash",
    ".swap",
    crate::registry::root::LOCK_FILE,
];

/// Where the sweep looks. Built by `uninstall` from the cluster's live
/// settings and paths, so a custom engine root or providers directory is
/// honoured.
#[derive(Debug, Clone)]
pub struct PurgeRoots {
    pub engine_root: PathBuf,
    /// A `postvec-server` process runs on this host: leave the engine root
    /// alone, it may be serving from it.
    pub engine_root_in_use: bool,
    pub providers_dir: PathBuf,
    /// `/var/lib/postvec` — holds `clusters/<key>.json`.
    pub state_dir: PathBuf,
    /// `/run/lock/postvec` — holds `<key>.lock`.
    pub lock_dir: PathBuf,
    pub cluster_key: String,
    pub pkglibdir: PathBuf,
    pub sharedir: PathBuf,
    /// False when `shared_preload_libraries` still names postvec from
    /// configuration the CLI does not own: deleting the library would then
    /// stop the next postmaster start.
    pub extension_files_removable: bool,
    /// The running CLI binary, reported but never deleted.
    pub cli_binary: Option<PathBuf>,
}

/// Which package manager claimed a file, and therefore which command removes
/// what the sweep left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

/// One path the sweep considers, before ownership is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub what: String,
    /// Whether the package manager may own it. Receipt-backed models and
    /// the CLI's own state never do; everything else is asked.
    pub check_package: bool,
}

/// The resolved sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PurgePlan {
    /// Paths to delete, in order.
    pub remove: Vec<Candidate>,
    /// Package-owned paths, grouped by package: reported, never deleted.
    pub packaged: BTreeMap<String, Vec<PathBuf>>,
    pub package_manager: Option<PackageManager>,
    /// Directories to remove afterwards if they are empty, deepest first.
    pub prune_dirs: Vec<PathBuf>,
    /// Things the operator should know: what was skipped and why.
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
/// engine-root layout, so its models must not be swept from under it.
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

/// Everything the sweep would touch, before asking the package manager.
/// Pure filesystem reading; unreadable locations become notes, never errors,
/// because a purge that stops at the first odd directory leaves more behind
/// than one that reports it.
pub fn gather(roots: &PurgeRoots) -> (Vec<Candidate>, Vec<PathBuf>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut prune = Vec::new();
    let mut notes = Vec::new();

    // --- engine root -------------------------------------------------------
    if roots.engine_root_in_use {
        notes.push(format!(
            "a postvec-server process is running on this host, so the engine root {} was \
             left alone (it may be serving from it)",
            roots.engine_root.display()
        ));
    } else if roots.engine_root.is_dir() {
        let models = roots.engine_root.join("models");
        if models.is_dir() {
            match fs::read_dir(&models) {
                Ok(entries) => {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        let name = entry.file_name().to_string_lossy().to_string();
                        if MODELS_STATE.contains(&name.as_str()) {
                            candidates.push(Candidate {
                                path,
                                what: "postvec-cli model-store state".to_string(),
                                check_package: false,
                            });
                        } else if path.is_dir() {
                            gather_backend(&path, &name, &mut candidates, &mut prune, &mut notes);
                        } else {
                            notes.push(format!(
                                "{} is not part of the engine-root layout and was left alone",
                                path.display()
                            ));
                        }
                    }
                }
                Err(error) => notes.push(format!("cannot read {}: {error}", models.display())),
            }
            prune.push(models);
        }
        let libs = roots.engine_root.join("libs");
        if libs.exists() {
            candidates.push(Candidate {
                path: libs,
                what: "ONNX Runtime libraries".to_string(),
                check_package: true,
            });
        }
        if let Ok(entries) = fs::read_dir(&roots.engine_root) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name == "models" || name == "libs" {
                    continue;
                }
                notes.push(format!(
                    "{} is not part of the engine-root layout and was left alone{}",
                    entry.path().display(),
                    if name == "providers.d" {
                        " (a postvec-server providers directory holds that node's credentials)"
                    } else {
                        ""
                    }
                ));
            }
        }
        prune.push(roots.engine_root.clone());
    }

    // --- provider connector files ------------------------------------------
    if roots.providers_dir.is_dir() {
        match fs::read_dir(&roots.providers_dir) {
            Ok(entries) => {
                let mut files: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| !p.is_dir())
                    .collect();
                files.sort();
                for path in files {
                    candidates.push(Candidate {
                        path,
                        what: "external-provider file (holds API credentials)".to_string(),
                        check_package: false,
                    });
                }
            }
            Err(error) => notes.push(format!(
                "cannot read {}: {error}; provider files may remain",
                roots.providers_dir.display()
            )),
        }
        prune.push(roots.providers_dir.clone());
        if let Some(parent) = roots.providers_dir.parent() {
            // `/etc/postvec` exists only for providers.d.
            if parent.file_name().map(|n| n == "postvec").unwrap_or(false) {
                prune.push(parent.to_path_buf());
            }
        }
    }

    // --- CLI state and lock ------------------------------------------------
    let state = roots
        .state_dir
        .join("clusters")
        .join(format!("{}.json", roots.cluster_key));
    if state.exists() {
        candidates.push(Candidate {
            path: state,
            what: "postvec-cli cluster state".to_string(),
            check_package: false,
        });
    }
    if roots.state_dir.exists() {
        prune.push(roots.state_dir.join("clusters"));
        prune.push(roots.state_dir.clone());
    }
    let lock = roots.lock_dir.join(format!("{}.lock", roots.cluster_key));
    if lock.exists() {
        candidates.push(Candidate {
            path: lock,
            what: "postvec-cli mutation lock".to_string(),
            check_package: false,
        });
    }
    if roots.lock_dir.exists() {
        prune.push(roots.lock_dir.clone());
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
                    .map(|n| n.starts_with("postvec--") && n.ends_with(".sql"))
                    .unwrap_or(false)
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
        .filter(|(path, _)| path.exists())
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
            candidates.push(Candidate {
                path,
                what,
                check_package: true,
            });
        }
    }

    (candidates, prune, notes)
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
                let receipt = path.is_dir() && matches!(Receipt::read(&path), Ok(Some(_)));
                candidates.push(Candidate {
                    path,
                    what: if receipt {
                        format!("model {backend}/{name}, pulled by postvec-cli")
                    } else {
                        format!("model {backend}/{name}")
                    },
                    // A receipt proves the CLI installed it; anything else may
                    // be a package's.
                    check_package: !receipt,
                });
            }
        }
        Err(error) => notes.push(format!("cannot read {}: {error}", backend_dir.display())),
    }
    prune.push(backend_dir.to_path_buf());
}

/// Split the gathered candidates by package ownership. `owner` answers
/// "which package claims this path?" — the real one shells out to dpkg/rpm,
/// tests use a table.
pub fn resolve(
    candidates: Vec<Candidate>,
    prune: Vec<PathBuf>,
    notes: Vec<String>,
    cli_binary: Option<&Path>,
    mut owner: impl FnMut(&Path) -> Option<(PackageManager, String)>,
) -> PurgePlan {
    let mut plan = PurgePlan {
        prune_dirs: prune,
        notes,
        ..PurgePlan::default()
    };
    for candidate in candidates {
        match candidate
            .check_package
            .then(|| owner(&candidate.path))
            .flatten()
        {
            Some((manager, package)) => {
                plan.package_manager.get_or_insert(manager);
                plan.packaged
                    .entry(package)
                    .or_default()
                    .push(candidate.path);
            }
            None => plan.remove.push(candidate),
        }
    }
    if let Some(binary) = cli_binary {
        match owner(binary) {
            Some((manager, package)) => {
                plan.package_manager.get_or_insert(manager);
                plan.packaged
                    .entry(package)
                    .or_default()
                    .push(binary.to_path_buf());
            }
            None => plan.notes.push(format!(
                "the postvec CLI itself, {}, is not package-owned and is the running program; \
                 remove it yourself when you are done",
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

/// Which package owns `path`, asking dpkg then rpm. Absent tools, timeouts
/// and non-zero exits all mean "nobody", which is the answer that leads to
/// deletion — so this is only ever called for paths the sweep would delete
/// anyway, and a missing package manager cannot make it delete more than a
/// host without packages would.
pub async fn package_owner(path: &Path, timeout: Duration) -> Option<(PackageManager, String)> {
    if !path.exists() {
        return None;
    }
    for (manager, program, flag) in [
        (PackageManager::Dpkg, "dpkg", "-S"),
        (PackageManager::Rpm, "rpm", "-qf"),
    ] {
        let cmd = Cmd::new(PathBuf::from(program))
            .arg(flag)
            .arg(path.display().to_string());
        let Ok(output) = proc::run(&cmd, timeout).await else {
            continue;
        };
        if !output.ok() {
            continue;
        }
        if let Some(package) = parse_owner(manager, &output.stdout) {
            return Some((manager, package));
        }
    }
    None
}

/// The package name out of `dpkg -S` (`pkg[:arch][, pkg2]: /path`) or
/// `rpm -qf` (`name-version-release.arch`) output.
pub fn parse_owner(manager: PackageManager, stdout: &str) -> Option<String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("diversion"))?;
    match manager {
        PackageManager::Dpkg => {
            let (packages, _path) = line.rsplit_once(": ")?;
            packages
                .split(", ")
                .next()
                .map(|p| p.split(':').next().unwrap_or(p).trim().to_string())
                .filter(|p| !p.is_empty())
        }
        PackageManager::Rpm => {
            if line.contains("not owned") {
                return None;
            }
            Some(line.to_string())
        }
    }
}

/// Delete what the plan says, then prune the directories that emptied out.
/// A path that cannot be removed is recorded as incomplete and the sweep
/// continues: stopping would leave *more* behind, not less.
pub fn apply(plan: &PurgePlan, journal: &mut ApplyJournal) {
    for candidate in &plan.remove {
        match remove_path(&candidate.path) {
            Ok(()) => journal.record(format!(
                "deleted {} ({})",
                candidate.path.display(),
                candidate.what
            )),
            // The configuration step already removed the CLI's own state
            // file; a path that is gone is the outcome wanted.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => journal.incomplete(format!(
                "could not delete {} ({}): {error}",
                candidate.path.display(),
                candidate.what
            )),
        }
    }
    for dir in &plan.prune_dirs {
        match fs::remove_dir(dir) {
            Ok(()) => journal.record(format!("removed empty directory {}", dir.display())),
            // Not empty, or already gone: both fine. Anything else is worth
            // a line, not a failure.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOTEMPTY) => {}
            Err(error) => journal.incomplete(format!(
                "could not remove directory {}: {error}",
                dir.display()
            )),
        }
    }
}

/// Remove a file, a symlink (the link, never its target), or a directory
/// tree. A symlinked directory is unlinked rather than descended: the target
/// is not something postvec created.
fn remove_path(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
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
    let (candidates, prune, notes) = gather(roots);
    let mut owners = BTreeMap::new();
    for candidate in candidates.iter().filter(|c| c.check_package) {
        owners.insert(
            candidate.path.clone(),
            package_owner(&candidate.path, timeout).await,
        );
    }
    if let Some(binary) = &roots.cli_binary {
        owners.insert(binary.clone(), package_owner(binary, timeout).await);
    }
    Ok(resolve(
        candidates,
        prune,
        notes,
        roots.cli_binary.as_deref(),
        |path| owners.get(path).cloned().flatten(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn roots(dir: &Path) -> PurgeRoots {
        PurgeRoots {
            engine_root: dir.join("opt/postvec"),
            engine_root_in_use: false,
            providers_dir: dir.join("etc/postvec/providers.d"),
            state_dir: dir.join("var/lib/postvec"),
            lock_dir: dir.join("run/lock/postvec"),
            cluster_key: "18-main".to_string(),
            pkglibdir: dir.join("usr/lib/postgresql/18/lib"),
            sharedir: dir.join("usr/share/postgresql/18"),
            extension_files_removable: true,
            cli_binary: Some(dir.join("usr/local/bin/postvec")),
        }
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }

    /// A full host: a package-owned bundled model and libs, a pulled model
    /// with a receipt, a hand-copied model, CLI state, provider files, and an
    /// unpackaged extension set.
    fn populate(dir: &Path) {
        let r = roots(dir);
        touch(
            &r.engine_root
                .join("models/onnx-runtime/bundled/ninference.hub.json"),
        );
        touch(
            &r.engine_root
                .join("models/onnx-runtime/pulled/ninference.hub.json"),
        );
        touch(
            &r.engine_root
                .join("models/onnx-runtime/manual/ninference.hub.json"),
        );
        touch(&r.engine_root.join("models/.staging/part"));
        touch(&r.engine_root.join("models/.postvec.lock"));
        touch(&r.engine_root.join("libs/onnxruntime/lib/libonnxruntime.so"));
        touch(&r.providers_dir.join("openai.toml"));
        touch(&r.providers_dir.join("openai.key"));
        touch(&r.state_dir.join("clusters/18-main.json"));
        touch(&r.lock_dir.join("18-main.lock"));
        touch(&r.pkglibdir.join("postvec.so"));
        touch(&r.sharedir.join("extension/postvec.control"));
        touch(&r.sharedir.join("extension/postvec--0.1.0.sql"));
        touch(&r.sharedir.join("extension/vector.control"));
        touch(&r.cli_binary.clone().unwrap());
        fs::set_permissions(&r.providers_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The ownership table a Debian host would answer with.
    fn debian_owner(dir: &Path) -> impl FnMut(&Path) -> Option<(PackageManager, String)> {
        let r = roots(dir);
        move |path: &Path| {
            let owned = [
                (
                    r.engine_root.join("models/onnx-runtime/bundled"),
                    "postvec-model-minilm-l6-v2",
                ),
                (r.engine_root.join("libs"), "postvec-onnxruntime"),
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
                .find(|(owned, _)| owned == path)
                .map(|(_, package)| (PackageManager::Dpkg, (*package).to_string()))
        }
    }

    #[test]
    fn a_packaged_host_deletes_only_what_no_package_owns_and_names_the_packages() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let (candidates, prune, notes) = gather(&r);
        let plan = resolve(
            candidates,
            prune,
            notes,
            r.cli_binary.as_deref(),
            debian_owner(dir.path()),
        );

        let removed: Vec<String> = plan
            .remove
            .iter()
            .map(|c| {
                c.path
                    .strip_prefix(dir.path())
                    .unwrap()
                    .display()
                    .to_string()
            })
            .collect();
        // The pulled model, the manual model, the CLI's state, the provider
        // files and the state/lock files go; nothing packaged does.
        for expected in [
            "opt/postvec/models/onnx-runtime/pulled",
            "opt/postvec/models/onnx-runtime/manual",
            "opt/postvec/models/.staging",
            "opt/postvec/models/.postvec.lock",
            "etc/postvec/providers.d/openai.key",
            "etc/postvec/providers.d/openai.toml",
            "var/lib/postvec/clusters/18-main.json",
            "run/lock/postvec/18-main.lock",
        ] {
            assert!(
                removed.contains(&expected.to_string()),
                "missing {expected} in {removed:?}"
            );
        }
        for kept in [
            "opt/postvec/models/onnx-runtime/bundled",
            "opt/postvec/libs",
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
        let note = plan.package_note().unwrap();
        assert!(
            note.starts_with("package-owned files were left in place"),
            "{note}"
        );
        assert!(
            note.ends_with(
                "sudo apt purge postgresql-18-postvec postvec-cli postvec-model-minilm-l6-v2 \
                 postvec-onnxruntime"
            ),
            "{note}"
        );
        // The provider files say what they are, since deleting them is the
        // one thing the ordinary uninstall refuses to do.
        assert!(plan
            .remove
            .iter()
            .filter(|c| c.path.ends_with("openai.toml"))
            .all(|c| c.what.contains("credentials")));
    }

    #[test]
    fn a_manual_install_is_swept_completely_and_the_running_binary_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        let (candidates, prune, notes) = gather(&r);
        let plan = resolve(candidates, prune, notes, r.cli_binary.as_deref(), |_| None);
        assert!(plan.packaged.is_empty());
        assert!(plan.package_note().is_none());
        let removed: Vec<&Path> = plan.remove.iter().map(|c| c.path.as_path()).collect();
        assert!(removed.contains(&r.pkglibdir.join("postvec.so").as_path()));
        assert!(removed.contains(&r.sharedir.join("extension/postvec--0.1.0.sql").as_path()));
        assert!(!removed.contains(&r.sharedir.join("extension/vector.control").as_path()));
        assert!(removed.contains(&r.engine_root.join("libs").as_path()));
        assert!(!removed.contains(&r.cli_binary.clone().unwrap().as_path()));
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("running program") && n.contains("usr/local/bin/postvec")));

        let mut journal = ApplyJournal::default();
        apply(&plan, &mut journal);
        assert!(journal.incomplete.is_empty(), "{:?}", journal.incomplete);
        for gone in [
            r.engine_root.clone(),
            r.providers_dir.parent().unwrap().to_path_buf(),
            r.state_dir.clone(),
            r.lock_dir.clone(),
            r.pkglibdir.join("postvec.so"),
        ] {
            assert!(!gone.exists(), "{} was left behind", gone.display());
        }
        // Not ours: the sibling extension and the binary.
        assert!(r.sharedir.join("extension/vector.control").exists());
        assert!(r.cli_binary.unwrap().exists());
    }

    #[test]
    fn a_foreign_preload_keeps_the_extension_files() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let mut r = roots(dir.path());
        r.extension_files_removable = false;
        let (candidates, _, notes) = gather(&r);
        assert!(!candidates
            .iter()
            .any(|c| c.path.ends_with("postvec.so") || c.path.ends_with("postvec.control")));
        assert!(notes.iter().any(|n| n.contains("shared_preload_libraries")));
    }

    #[test]
    fn a_running_server_keeps_the_engine_root() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let mut r = roots(dir.path());
        r.engine_root_in_use = true;
        let (candidates, _, notes) = gather(&r);
        assert!(!candidates
            .iter()
            .any(|c| c.path.starts_with(&r.engine_root)));
        assert!(notes.iter().any(|n| n.contains("postvec-server")));
        // Everything outside the root is still swept.
        assert!(candidates.iter().any(|c| c.path.ends_with("18-main.json")));
    }

    #[test]
    fn a_server_providers_directory_under_the_root_is_reported_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        populate(dir.path());
        let r = roots(dir.path());
        touch(&r.engine_root.join("providers.d/univec.toml"));
        let (candidates, _, notes) = gather(&r);
        assert!(!candidates
            .iter()
            .any(|c| c.path.starts_with(r.engine_root.join("providers.d"))));
        assert!(notes
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
        assert!(other_clusters_configured(&r.state_dir, "18-main").len() == 1);
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(plan(&r, Duration::from_secs(1)))
            .unwrap_err()
            .to_string();
        assert!(error.contains("17-main"), "{error}");
    }

    #[test]
    fn symlinked_entries_are_unlinked_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        touch(&outside.join("precious"));
        let r = roots(dir.path());
        fs::create_dir_all(r.engine_root.join("models/onnx-runtime")).unwrap();
        std::os::unix::fs::symlink(&outside, r.engine_root.join("models/onnx-runtime/linked"))
            .unwrap();
        let (candidates, prune, notes) = gather(&r);
        let plan = resolve(candidates, prune, notes, None, |_| None);
        let mut journal = ApplyJournal::default();
        apply(&plan, &mut journal);
        assert!(
            outside.join("precious").exists(),
            "the link target was followed"
        );
        assert!(!r.engine_root.exists());
    }

    #[test]
    fn dpkg_and_rpm_answers_parse() {
        assert_eq!(
            parse_owner(
                PackageManager::Dpkg,
                "postvec-onnxruntime: /opt/postvec/libs/onnxruntime\n"
            )
            .as_deref(),
            Some("postvec-onnxruntime")
        );
        assert_eq!(
            parse_owner(PackageManager::Dpkg, "libfoo:amd64, libbar: /usr/lib/x\n").as_deref(),
            Some("libfoo")
        );
        assert_eq!(
            parse_owner(
                PackageManager::Dpkg,
                "diversion by dash from: /bin/sh\npostvec-cli: /usr/bin/postvec\n"
            )
            .as_deref(),
            Some("postvec-cli")
        );
        assert_eq!(
            parse_owner(PackageManager::Rpm, "postvec-cli-0.1.0-1.el9.x86_64\n").as_deref(),
            Some("postvec-cli-0.1.0-1.el9.x86_64")
        );
        assert!(
            parse_owner(PackageManager::Rpm, "file /x is not owned by any package\n").is_none()
        );
        assert!(parse_owner(PackageManager::Dpkg, "").is_none());
    }
}
