//! Engine-root filesystem management: locking, staging, atomic install,
//! guarded removal, and the installed-model inventory.
//!
//! Layout contract (mirrors what the engine's loader actually does):
//! models live at exactly `<root>/models/<backend>/<model>/`, the CLI's
//! transient state lives under `<root>/models/.staging/` and
//! `<root>/models/.trash/`, and the mutation lock is
//! `<root>/models/.postvec.lock`: per engine root, not per cluster,
//! because several clusters may share one root.

use crate::config::owned::{self, HostLock};
use crate::error::{CliError, Result};
use crate::proc::{self, Cmd};
use crate::registry::receipt::Receipt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LOCK_FILE: &str = ".postvec.lock";
const STAGING_DIR: &str = ".staging";
const TRASH_DIR: &str = ".trash";
/// Where an in-place replacement parks the copies it is replacing, together
/// with the transaction record that says whether they are still
/// authoritative.
///
/// Distinct from `.trash` on purpose: trash means *removal intent* and is
/// always swept, whereas a parked copy is the last known-good revision of a
/// model whose replacement has not been proven to load. Filesystem presence
/// cannot encode that proof — only the record can — so this directory is
/// never swept blindly.
const SWAP_DIR: &str = ".swap";
const SWAP_RECORD: &str = "txn.json";
// 2: every batch member is recorded with a `role`, not replacements only.
// 3: a member records whether rollback must reload it — a replacement of a
//    deactivated model has no resident predecessor to restore into the engine.
// No registry has been deployed, so an older record is refused rather than
// migrated — see `pending_swap`.
const SWAP_RECORD_SCHEMA: u32 = 3;
const DESCRIPTOR_FILE: &str = "ninference.hub.json";

/// A resolved engine root.
#[derive(Debug, Clone)]
pub struct ModelRoot {
    pub root: PathBuf,
}

/// Who owns an installed model directory — decides what `rm` may touch and
/// what the refusal message says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// A valid CLI receipt is present.
    Cli,
    /// The package manager claims the directory (dpkg or rpm).
    Package,
    /// Neither — an operator-managed directory.
    Manual,
}

impl Ownership {
    pub fn describe(&self) -> &'static str {
        match self {
            Ownership::Cli => "installed by postvec-cli",
            Ownership::Package => "owned by a package",
            Ownership::Manual => "manually managed (no receipt)",
        }
    }
}

/// Whether [`ModelRoot::set_enabled`] had anything to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledChange {
    Changed,
    Unchanged,
}

/// What a batch member is doing, and therefore how recovery undoes it.
///
/// Recording fresh installs alongside replacements is what makes the record a
/// description of the whole batch rather than of its replacements only. Before
/// this existed, a crash mid-load could leave a fresh dependency installed
/// *and resident* while recovery — which knew only the replacement names —
/// neither unloaded nor removed it. Every loaded model participates in the
/// engine's resolver, so that left the recovered runtime unequal to the
/// pre-command runtime, which is precisely what the transaction exists to
/// prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwapRole {
    /// An installed model is being replaced in place; its predecessor is
    /// parked under `.swap/` and rollback restores it.
    Replace,
    /// A model that did not exist before this batch; rollback removes it.
    /// There is nothing to park and nothing to reload.
    Install,
}

/// One model taking part in a batch transaction.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SwapModel {
    pub name: String,
    pub backend: String,
    pub role: SwapRole,
    /// Whether rollback must put this model's predecessor back **into the
    /// engine**, not merely back on disk.
    ///
    /// False for a fresh install (nothing preceded it) and for a replacement
    /// of a **deactivated** model: its predecessor was not resident, and the
    /// engine refuses to load a disabled descriptor — asking would fail the
    /// very proof [`crate::commands::model::restore_and_prove`] exists to
    /// establish, turning a clean rollback into a retained transaction.
    pub reload_on_rollback: bool,
}

/// Whether the parked predecessors are still authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SwapPhase {
    /// Replacements are on disk but unproven: recovery restores.
    Swapping,
    /// Replacements are proven loadable: recovery discards.
    Confirmed,
}

/// A transaction over one batch of models — replacements *and* fresh
/// installs. Both need quiescing on rollback, because either may have been
/// made resident by the load that failed.
#[derive(Debug)]
pub struct SwapTransaction {
    dir: PathBuf,
    models: Vec<SwapModel>,
}

impl SwapTransaction {
    /// Every name in the batch. This is the quiesce set: the engine may hold
    /// new bytes for any of them.
    pub fn names(&self) -> Vec<String> {
        self.models.iter().map(|m| m.name.clone()).collect()
    }

    /// The names whose predecessor must be put back **into the engine** after a
    /// rollback restores it on disk. A fresh install has no predecessor, and a
    /// deactivated model's predecessor was never resident — asking the engine
    /// for either would load nothing that exists, or nothing it will accept.
    pub fn replaced_names(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|m| m.reload_on_rollback)
            .map(|m| m.name.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SwapRecord {
    schema_version: u32,
    phase: SwapPhase,
    models: Vec<SwapModel>,
}

/// Write the transaction record durably. Every step propagates its error
/// the general-purpose `write_atomic` treats the
/// parent-directory fsync as best effort, which is fine for configuration but
/// not for the one fact that decides whether a predecessor may be deleted.
fn write_swap_record(dir: &Path, phase: SwapPhase, models: &[SwapModel]) -> Result<()> {
    let body = serde_json::to_vec_pretty(&SwapRecord {
        schema_version: SWAP_RECORD_SCHEMA,
        phase,
        models: models.to_vec(),
    })
    .map_err(|e| CliError::internal(format!("cannot serialize the swap record: {e}")))?;
    let path = dir.join(SWAP_RECORD);
    let tmp = dir.join(".txn.json.tmp");
    let _ = fs::remove_file(&tmp);
    let write = |path: &Path| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(path)?;
        file.write_all(&body)?;
        file.flush()?;
        file.sync_all()
    };
    write(&tmp).map_err(|e| {
        CliError::apply(format!(
            "cannot write the replacement record {}: {e}",
            tmp.display()
        ))
    })?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        CliError::apply(format!(
            "cannot install the replacement record {}: {e}",
            path.display()
        ))
    })?;
    fsync_dir(dir)
}

/// Directory children of `parent`, enumerated strictly, sorted.
///
/// `read_dir(..).flatten()` and `Path::is_dir()` are both fail-open — the
/// first discards an errored entry, the second turns a metadata failure into
/// "not a directory" — and this enumeration decides which directories the CLI
/// believes exist. The engine scans every backend and serves the **first**
/// directory-name match it finds, so a directory the CLI silently overlooked
/// is how a second copy of one logical model gets installed and the wrong one
/// served.
///
/// Hidden entries are skipped: `.staging`, `.trash` and `.swap` are the CLI's
/// own state and the engine ignores them too. Non-directories are skipped.
/// A **symlink is refused**, not skipped: the engine's own scan follows it, so
/// ignoring one would make the CLI and the engine disagree about the
/// searchable set.
fn strict_dir_children(parent: &Path, what: &str) -> Result<Vec<PathBuf>> {
    let entries = fs::read_dir(parent)
        .map_err(|e| CliError::precondition(format!("cannot read {}: {e}", parent.display())))?;
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            CliError::precondition(format!(
                "cannot enumerate {} in {}: {e}",
                what,
                parent.display()
            ))
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err(CliError::precondition(format!(
                "{} has a non-UTF-8 name; postvec cannot match it against a model name",
                path.display()
            ))
            .with_fix("remove or rename it"));
        };
        if name.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| {
            CliError::precondition(format!("cannot inspect {}: {e}", path.display()))
        })?;
        if file_type.is_symlink() {
            return Err(CliError::precondition(format!(
                "{} is a symlink; the engine's scan follows it but postvec will not, so the two \
                 would disagree about which models exist",
                path.display()
            ))
            .with_fix("replace the symlink with a real directory, or remove it"));
        }
        if !file_type.is_dir() {
            continue;
        }
        children.push(path);
    }
    children.sort();
    Ok(children)
}

/// Does this path exist?
///
/// Only a typed `NotFound` counts as absence. Every other lookup failure —
/// `EACCES`, `EIO`, a broken mount — leaves the answer *unknown*, and the one
/// place that must never be guessed is whether a transaction still has
/// something to restore or remove: reading "unknown" as "already done" would
/// let recovery clear its own evidence without having established the
/// filesystem state it promised.
fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(CliError::apply(format!(
            "cannot determine whether {} exists: {e}",
            path.display()
        ))
        .with_fix("fix the filesystem or permission problem and rerun")),
    }
}

/// fsync a directory entry, propagating failure. A rename is only durable
/// once the directories on **both** sides have been synced.
fn fsync_dir(path: &Path) -> Result<()> {
    let dir = fs::File::open(path)
        .map_err(|e| CliError::apply(format!("cannot open {} to fsync: {e}", path.display())))?;
    dir.sync_all()
        .map_err(|e| CliError::apply(format!("cannot fsync {}: {e}", path.display())))
}

/// One model directory on disk, however it got there.
#[derive(Debug)]
pub struct InstalledModel {
    /// Directory name (what an explicit `postvec.embedded_models` entry and
    /// an engine load request must match).
    pub dir_name: String,
    /// `configuration.name` from the descriptor, when readable.
    pub descriptor_name: Option<String>,
    pub backend: String,
    pub enabled: bool,
    pub dependencies: Vec<String>,
    /// `params.model_type` from the descriptor, when present.
    pub model_type: Option<String>,
    /// `params.source_model` / `params.target_model` from the descriptor —
    /// the vector spaces a converter reads from and writes into. Together with
    /// `model_type` they are what decides which columns a model can still
    /// serve, which is how [`crate::commands::model::in_use_columns`] answers
    /// "does anything break if this goes away".
    pub source_model: Option<String>,
    pub target_model: Option<String>,
    /// `params.target_dim` from the descriptor, when present.
    pub target_dim: Option<u32>,
    pub path: PathBuf,
    pub receipt: Option<Receipt>,
    pub receipt_error: Option<String>,
    pub disk_bytes: u64,
}

impl InstalledModel {
    pub async fn ownership(&self, timeout: Duration) -> Ownership {
        if self.receipt.is_some() {
            return Ownership::Cli;
        }
        if package_owns(&self.path.join(DESCRIPTOR_FILE), timeout).await {
            return Ownership::Package;
        }
        Ownership::Manual
    }
}

impl ModelRoot {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.models_dir().join(LOCK_FILE)
    }

    /// The models directory, proven to exist.
    ///
    /// Every read of the inventory funnels through here so that "there is no
    /// engine root at this path" is reported once, in the operator's
    /// vocabulary, instead of as a bare `ENOENT` from whichever enumeration
    /// happened to touch the directory first. Only a typed `NotFound` is
    /// absence; anything else (a permission problem, a dead mount) keeps its
    /// own error, because "it is not there" and "it could not be read" call
    /// for different fixes.
    pub fn require_models_dir(&self) -> Result<PathBuf> {
        let models = self.models_dir();
        match fs::symlink_metadata(&models) {
            Ok(meta) if meta.is_dir() => Ok(models),
            Ok(meta) if meta.file_type().is_symlink() => Err(CliError::precondition(format!(
                "{} is a symlink; refusing to manage it",
                models.display()
            ))
            .with_fix("point --path at the real engine root instead")),
            Ok(_) => Err(CliError::precondition(format!(
                "{} exists but is not a directory; this is not an engine root",
                models.display()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(CliError::precondition(format!(
                    "{} does not exist; this is not an engine root",
                    models.display()
                ))
                .with_fix(format!(
                    "check --path, or create the root with: mkdir -p {} && chmod go-w {}",
                    models.display(),
                    models.display()
                )))
            }
            Err(e) => Err(CliError::precondition(format!(
                "cannot inspect {}: {e}",
                models.display()
            ))),
        }
    }

    /// Every precondition a mutating command places on the models directory,
    /// with nothing acquired and nothing changed.
    ///
    /// Split out of [`Self::lock_exclusive`] so a `--dry-run` can assert the
    /// same things the real run will: a dry run that reports a clean plan and
    /// is then followed by a refusal has told the operator nothing useful.
    /// The lock itself is deliberately *not* taken here — waiting on a
    /// concurrent command is a cost a preview should not impose.
    pub fn check_mutable(&self) -> Result<()> {
        let models = self.require_models_dir()?;
        // Ancestor discipline: the path must reach the models directory
        // without traversing a symlink anywhere. A replaceable
        // symlinked ancestor lets another account swap the whole tree
        // between check and use. Canonical-path equality is exactly
        // that assertion, and the fix is to manage the canonical path.
        let canonical = models.canonicalize().map_err(|e| {
            CliError::precondition(format!("cannot canonicalize {}: {e}", models.display()))
        })?;
        if canonical != models {
            return Err(CliError::precondition(format!(
                "{} traverses a symlink (canonical path is {}); refusing to manage it",
                models.display(),
                canonical.display()
            ))
            .with_fix(format!(
                "manage the canonical root instead: --path {}",
                canonical.parent().unwrap_or(&canonical).display()
            )));
        }
        owned::check_trusted_dir(&models, "models directory")?;
        owned::check_trusted_ancestry(&models)
    }

    /// Exclusive lock for mutation. Also the moment stale staging from an
    /// interrupted command is swept — only ever under the lock.
    ///
    /// Mutation refuses an untrustworthy models directory outright:
    /// symlinked, world-writable or owned by another account. A
    /// privileged pull into such a root could be steered by a
    /// lower-privileged writer.
    pub fn lock_exclusive(&self) -> Result<HostLock> {
        self.check_mutable()?;
        let lock = HostLock::acquire_labeled(&self.lock_path(), "this engine root")?;
        self.sweep_stale_staging();
        Ok(lock)
    }

    /// Shared lock for read-only commands; `None` when nothing to lock.
    pub fn lock_shared(&self) -> Result<Option<HostLock>> {
        HostLock::acquire_shared_if_present(&self.lock_path())
    }

    fn sweep_stale_staging(&self) {
        let staging = self.models_dir().join(STAGING_DIR);
        if let Ok(entries) = fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let path = entry.path();
                // Keep resumable `.part` downloads; sweep interrupted extract
                // directories (and anything else) — they are re-created whole.
                let is_part = path.extension().is_some_and(|e| e == "part");
                if !is_part {
                    let _ = if path.is_dir() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                }
            }
        }
        let trash = self.models_dir().join(TRASH_DIR);
        if let Ok(entries) = fs::read_dir(&trash) {
            for entry in entries.flatten() {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
        // `.swap` is deliberately NOT swept here: whether a parked copy is
        // the last known-good revision or a superseded one is a fact about
        // the engine, not the filesystem. Mutating model commands resolve it
        // through `pending_swap` before doing anything else.
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.models_dir().join(STAGING_DIR)
    }

    pub fn ensure_staging(&self) -> Result<PathBuf> {
        let staging = self.staging_dir();
        owned::create_dir_all_checked(&staging, 0o755)?;
        owned::check_trusted_dir(&staging, "staging directory")?;
        Ok(staging)
    }

    /// The resumable download path for an archive digest.
    pub fn part_path(&self, digest_hex: &str) -> PathBuf {
        self.staging_dir().join(format!("{digest_hex}.part"))
    }

    /// A fresh extraction directory for `name`.
    pub fn extract_dir(&self, name: &str) -> PathBuf {
        self.staging_dir()
            .join(format!("{name}.{}", unique_suffix()))
    }

    /// Search every backend directory for `name`. The engine resolves a
    /// requested model by the first directory-name match across backends,
    /// so a second copy under another backend is a silent wrong-model
    /// bug, not a coexistence feature.
    pub fn find_installed_anywhere(&self, name: &str) -> Result<Option<PathBuf>> {
        for backend in strict_dir_children(&self.require_models_dir()?, "backend directory")? {
            let candidate = backend.join(name);
            if path_present(&candidate)? {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }

    /// Atomically install a fully staged, validated model directory to
    /// `models/<backend>/<name>/`. The staged tree already contains the
    /// receipt; everything is fsynced before the rename, and the parent after
    /// it, so a crash leaves either nothing visible or a complete install.
    /// **Post-rename failures do not un-land the install.** Once the rename
    /// succeeds the directory is visible to the engine's scanner, so a later
    /// `fsync` error returns `Err` with the model *installed*. Callers must not
    /// treat `Err` as "nothing happened": the batch record (`SwapRole::Install`)
    /// is what makes the destination known to rollback and to recovery,
    /// independently of what this function managed to return.
    pub fn install_staged(&self, staged: &Path, backend: &str, name: &str) -> Result<PathBuf> {
        let models_dir = self.models_dir();
        let backend_dir = models_dir.join(backend);
        let backend_dir_is_new = !path_present(&backend_dir)?;
        owned::create_dir_all_checked(&backend_dir, 0o755)?;
        // A new backend directory is itself a directory entry in `models/`.
        // Syncing only the backend directory would leave the install durable
        // inside a parent that may not survive a crash.
        if backend_dir_is_new {
            fsync_dir(&models_dir)?;
        }
        let final_path = backend_dir.join(name);
        if path_present(&final_path)? {
            return Err(CliError::precondition(format!(
                "{} already exists",
                final_path.display()
            )));
        }
        fsync_tree(staged)?;
        let staging_parent = staged.parent().map(|p| p.to_path_buf());
        fs::rename(staged, &final_path).map_err(|e| {
            CliError::apply(format!("cannot install {}: {e}", final_path.display()))
        })?;
        fsync_dir(&backend_dir)?;
        if let Some(staging_parent) = staging_parent {
            fsync_dir(&staging_parent)?;
        }
        Ok(final_path)
    }

    fn swap_dir(&self) -> PathBuf {
        self.models_dir().join(SWAP_DIR)
    }

    /// Open a replacement transaction over `models`, durably, **before the
    /// first engine call** — not merely before the first rename
    /// An unload that times out may have been
    /// applied in whole or in part, so the record has to exist for the next
    /// command to find even when nothing has moved on disk yet.
    pub fn begin_swap(&self, models: Vec<SwapModel>) -> Result<SwapTransaction> {
        let dir = self.swap_dir();
        if path_present(&dir.join(SWAP_RECORD))? {
            return Err(CliError::internal(
                "an unfinished replacement is already recorded in this root; it must be \
                 recovered before another one starts",
            ));
        }
        owned::create_dir_all_checked(&dir, 0o755)?;
        // The directory entry itself must survive a crash, or the record
        // inside it is unreachable.
        fsync_dir(&self.models_dir())?;
        write_swap_record(&dir, SwapPhase::Swapping, &models)?;
        Ok(SwapTransaction { dir, models })
    }

    /// Replace an installed model with a staged one, in place, under the
    /// exclusive lock. Two `rename(2)` calls inside one directory tree on one
    /// filesystem, with nothing fallible between them:
    ///
    /// 1. `models/<backend>/<name>` → `.swap/<backend>/<name>`
    /// 2. staged → `models/<backend>/<name>`
    ///
    /// Both parents of the cross-directory rename are fsynced before the next
    /// state boundary: syncing only the destination side would not durably
    /// represent the move.
    pub fn swap_in(
        &self,
        txn: &SwapTransaction,
        staged: &Path,
        backend: &str,
        name: &str,
    ) -> Result<()> {
        let backend_dir = self.models_dir().join(backend);
        let installed = backend_dir.join(name);
        if !path_present(&installed)? {
            return Err(CliError::internal(format!(
                "{} vanished before the swap",
                installed.display()
            )));
        }
        let parked_dir = txn.dir.join(backend);
        owned::create_dir_all_checked(&parked_dir, 0o755)?;
        fsync_dir(&txn.dir)?;
        let parked = parked_dir.join(name);
        let staging_parent = staged.parent().map(|p| p.to_path_buf());

        // Everything before this point is undone by deleting staging.
        fsync_tree(staged)?;
        fs::rename(&installed, &parked).map_err(|e| {
            CliError::apply(format!(
                "cannot set {} aside for replacement: {e}",
                installed.display()
            ))
        })?;
        if let Err(e) = fs::rename(staged, &installed) {
            // The only window, and it is closed immediately: put the old copy
            // back before reporting.
            let _ = fs::rename(&parked, &installed);
            return Err(CliError::apply(format!(
                "cannot install the replacement at {}: {e} (the previous copy was restored)",
                installed.display()
            )));
        }
        fsync_dir(&backend_dir)?;
        fsync_dir(&parked_dir)?;
        if let Some(staging_parent) = staging_parent {
            fsync_dir(&staging_parent)?;
        }
        Ok(())
    }

    /// Restore every parked predecessor over whatever now sits at its
    /// destination. **The record is deliberately left in place**: the
    /// filesystem is only half the state, and until the engine has been shown
    /// to hold the predecessors again the transaction is still unsettled
    pub fn restore_swapped(&self, txn: &SwapTransaction) -> Result<()> {
        let mut problems = Vec::new();
        for model in txn.models.iter().filter(|m| m.role == SwapRole::Replace) {
            let parked = txn.dir.join(&model.backend).join(&model.name);
            match path_present(&parked) {
                // Never parked, or already restored: an ordinary no-op that
                // keeps recovery idempotent.
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => {
                    problems.push(e.to_string());
                    continue;
                }
            }
            let destination = self.models_dir().join(&model.backend).join(&model.name);
            let occupied = match path_present(&destination) {
                Ok(occupied) => occupied,
                Err(e) => {
                    problems.push(e.to_string());
                    continue;
                }
            };
            if occupied {
                match self.retire_to_trash(&destination) {
                    Ok(grave) => {
                        let _ = fs::remove_dir_all(&grave);
                    }
                    Err(e) => {
                        problems.push(format!("{}: {e}", model.name));
                        continue;
                    }
                }
            }
            match fs::rename(&parked, &destination) {
                Ok(()) => {
                    let backend_dir = self.models_dir().join(&model.backend);
                    if let Err(e) = fsync_dir(&backend_dir)
                        .and_then(|()| fsync_dir(&txn.dir.join(&model.backend)))
                    {
                        problems.push(format!("{}: {e}", model.name));
                    }
                }
                Err(e) => problems.push(format!(
                    "{}: cannot restore {}: {e}",
                    model.name,
                    destination.display()
                )),
            }
        }
        if !problems.is_empty() {
            return Err(CliError::apply(format!(
                "the previous revision could not be fully restored: {}",
                problems.join("; ")
            ))
            .with_fix(
                "the replacement transaction is still recorded; fix the filesystem problem and \
                 rerun any `postvec model` command to finish recovery",
            ));
        }
        Ok(())
    }

    /// Remove every fresh install the batch recorded, whether or not this
    /// process ever learned that it landed.
    ///
    /// This is the other half of a rollback, and it is driven by the **record**
    /// rather than by in-process bookkeeping on purpose. `install_staged` can
    /// return `Err` from a post-rename `fsync` with the model already in place,
    /// and a crash can land one with no surviving process at all; in both cases
    /// the only durable knowledge that it may exist is this record. Absence is
    /// an ordinary no-op, so recovery is idempotent and a batch that crashed
    /// before anything landed costs nothing.
    ///
    /// Safe because a fresh install's destination was proven free at plan time
    /// and `install_staged` refuses a destination that exists: anything sitting
    /// there now was put there by this batch.
    pub fn remove_recorded_installs(&self, txn: &SwapTransaction) -> Result<()> {
        let mut problems = Vec::new();
        for model in txn.models.iter().filter(|m| m.role == SwapRole::Install) {
            let path = self.models_dir().join(&model.backend).join(&model.name);
            match path_present(&path) {
                // Never landed, or already removed.
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => {
                    problems.push(e.to_string());
                    continue;
                }
            }
            if let Err(e) = self.remove_installed(&path) {
                problems.push(format!("{}: {e}", model.name));
            }
        }
        if !problems.is_empty() {
            return Err(CliError::apply(format!(
                "the fresh installs from this batch could not be removed: {}",
                problems.join("; ")
            ))
            .with_fix(
                "the transaction is still recorded; fix the filesystem problem and rerun any \
                 `postvec model` command to finish recovery",
            ));
        }
        Ok(())
    }

    /// Accept a transaction: the replacements are proven, so the predecessors
    /// may go. The phase is written and fsynced *before* the deletion, so a
    /// crash in between recovers as "confirmed — discard", never as a
    /// rollback over proven-good bytes.
    pub fn confirm_swap(&self, txn: &SwapTransaction) -> Result<()> {
        write_swap_record(&txn.dir, SwapPhase::Confirmed, &txn.models)?;
        self.discard_swap(txn);
        Ok(())
    }

    /// Clear a settled transaction: the record and any parked copies go, and
    /// failure is reported. This is the **last** step of a rollback or a
    /// recovery — until it runs, the next command must still see the
    /// transaction.
    pub fn clear_swap(&self, txn: &SwapTransaction) -> Result<()> {
        fs::remove_dir_all(&txn.dir).map_err(|e| {
            CliError::apply(format!(
                "cannot clear the replacement record at {}: {e}",
                txn.dir.display()
            ))
            .with_fix("remove the directory by hand once the filesystem problem is fixed")
        })?;
        fsync_dir(&self.models_dir())
    }

    /// Drop a confirmed transaction's parked predecessors. Best effort *only*
    /// here: the record already says they are superseded, so a leftover
    /// directory is disk usage, never a correctness question.
    pub fn discard_swap(&self, txn: &SwapTransaction) {
        let _ = fs::remove_dir_all(&txn.dir);
    }

    /// The replacement transaction left behind by an interrupted command, if
    /// any. Read-only; the caller decides how to settle it, because settling
    /// an unconfirmed one may require the engine.
    pub fn pending_swap(&self) -> Result<Option<(SwapTransaction, SwapPhase)>> {
        let dir = self.swap_dir();
        let path = dir.join(SWAP_RECORD);
        let Some(content) = owned::read_regular_file(&path)? else {
            // No record: either nothing happened, or a `begin_swap` that
            // never got to write one. Any stray tree there is pre-transaction
            // debris with no destination to restore to.
            if dir.exists() {
                let _ = fs::remove_dir_all(&dir);
            }
            return Ok(None);
        };
        // Version first, and on its own. Deserializing the whole record before
        // checking the version means a record whose *shape* changed dies with
        // a serde error ("missing field ...") instead of the actionable
        // version refusal — which is exactly the case the version exists for.
        #[derive(serde::Deserialize)]
        struct SchemaProbe {
            schema_version: u32,
        }
        let malformed = |e: String| {
            CliError::precondition(format!("{} is malformed: {e}", path.display())).with_fix(
                "this records an interrupted model batch; the parked copies under \
                 .swap/ are the previous revisions — restore them by hand and remove the file",
            )
        };
        let probe: SchemaProbe =
            serde_json::from_str(&content).map_err(|e| malformed(e.to_string()))?;
        if probe.schema_version != SWAP_RECORD_SCHEMA {
            return Err(CliError::precondition(format!(
                "{} has schema_version {}, this postvec-cli understands {SWAP_RECORD_SCHEMA}",
                path.display(),
                probe.schema_version
            ))
            .with_fix("upgrade postvec-cli to finish the interrupted replacement"));
        }
        let record: SwapRecord =
            serde_json::from_str(&content).map_err(|e| malformed(e.to_string()))?;
        for model in &record.models {
            crate::registry::index::valid_model_name(&model.name)
                .and_then(|()| crate::registry::index::valid_model_name(&model.backend))
                .map_err(|e| {
                    CliError::precondition(format!("{} is malformed: {e}", path.display()))
                })?;
        }
        Ok(Some((
            SwapTransaction {
                dir,
                models: record.models,
            },
            record.phase,
        )))
    }

    /// Phase one of a removal: rename an installed model into root-local
    /// trash (same filesystem, so it cannot fail halfway) and return the
    /// grave path. The model becomes invisible to the engine's scanner but
    /// its bytes are intact — [`ModelRoot::restore_retired`] undoes it.
    pub fn retire_to_trash(&self, path: &Path) -> Result<PathBuf> {
        let trash = self.models_dir().join(TRASH_DIR);
        owned::create_dir_all_checked(&trash, 0o755)?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("model");
        let grave = trash.join(format!("{name}.{}", unique_suffix()));
        fs::rename(path, &grave)
            .map_err(|e| CliError::apply(format!("cannot remove {}: {e}", path.display())))?;
        Ok(grave)
    }

    /// Undo [`ModelRoot::retire_to_trash`]: put the directory back where it
    /// was. Used when a later retire in the same batch fails, so a partial
    /// multi-model removal does not leave a broken closure behind.
    pub fn restore_retired(&self, grave: &Path, original: &Path) -> Result<()> {
        fs::rename(grave, original).map_err(|e| {
            CliError::apply(format!(
                "cannot restore {} from {}: {e}",
                original.display(),
                grave.display()
            ))
        })
    }

    /// Phase two of a removal: delete a retired directory's bytes. A failure
    /// here is reported but the model is already deactivated — the next
    /// lock-holding command's sweep finishes the job.
    pub fn purge_retired(&self, original: &Path, grave: &Path) -> Result<()> {
        if let Err(e) = fs::remove_dir_all(grave) {
            return Err(CliError::apply(format!(
                "{} was deactivated but its bytes linger in {}: {e}",
                original.display(),
                grave.display()
            ))
            .with_fix("remove the trash directory manually, or rerun any model command"));
        }
        Ok(())
    }

    /// Retire-then-purge in one call, for callers with a single directory and
    /// no unload ordering concerns.
    pub fn remove_installed(&self, path: &Path) -> Result<()> {
        let grave = self.retire_to_trash(path)?;
        self.purge_retired(path, &grave)
    }

    /// The installed inventory: every model directory under every backend,
    /// with descriptor summary, receipt (when present), and disk usage.
    pub fn installed(&self) -> Result<Vec<InstalledModel>> {
        let mut result = Vec::new();
        // Same strict enumeration as the collision guard: an unreadable
        // backend is reported, never quietly treated as empty.
        for backend_dir in strict_dir_children(&self.require_models_dir()?, "backend directory")? {
            let backend = backend_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let model_paths = strict_dir_children(&backend_dir, "model directory")?;
            for path in model_paths {
                let dir_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                let descriptor = read_descriptor_summary(&path.join(DESCRIPTOR_FILE));
                let (receipt, receipt_error) = match Receipt::read(&path) {
                    Ok(r) => (r, None),
                    Err(e) => (None, Some(e.to_string())),
                };
                result.push(InstalledModel {
                    dir_name,
                    descriptor_name: descriptor.as_ref().map(|d| d.name.clone()),
                    backend: backend.clone(),
                    enabled: descriptor.as_ref().map(|d| d.enabled).unwrap_or(false),
                    model_type: descriptor
                        .as_ref()
                        .and_then(|d| d.params.model_type.clone()),
                    source_model: descriptor
                        .as_ref()
                        .and_then(|d| d.params.source_model.clone()),
                    target_model: descriptor
                        .as_ref()
                        .and_then(|d| d.params.target_model.clone()),
                    target_dim: descriptor.as_ref().and_then(|d| d.params.target_dim),
                    dependencies: descriptor.map(|d| d.dependencies).unwrap_or_default(),
                    disk_bytes: dir_size(&path),
                    path,
                    receipt,
                    receipt_error,
                });
            }
        }
        Ok(result)
    }

    /// Flip the installed descriptor's `enabled` field, and keep the receipt
    /// truthful about it.
    ///
    /// This is the whole persistence mechanism behind `model activate` and
    /// `model deactivate`: the engine reads `enabled` from the installed
    /// `ninference.hub.json` at every start, so a flip here survives a
    /// PostgreSQL restart with no GUC, sidecar or catalogue involved.
    ///
    /// Order is descriptor first, receipt second. A crash in between leaves
    /// the engine's view correct and the receipt stale, which `--verify` and
    /// `doctor --deep` report as a descriptor hash mismatch and the next
    /// activate/deactivate repairs. The reverse order would leave a receipt
    /// claiming a state the engine does not have.
    ///
    /// `archive_digest`, `revision` and the identity block are never touched:
    /// they name the published bytes, not the operator's power switch.
    ///
    /// The rewrite is a `serde_json::Value` round-trip, so every key survives
    /// but the file is **not byte-stable against the published archive** —
    /// whitespace, and (depending on whether `serde_json/preserve_order` is
    /// unified into the build) key order, may differ. That is fine and is the
    /// point: the installed descriptor is deliberately not the archive's, and
    /// the receipt — updated below — is the integrity anchor for what is on
    /// disk. Nothing may compare this file against the archive byte for byte.
    pub fn set_enabled(&self, model: &InstalledModel, enabled: bool) -> Result<EnabledChange> {
        let descriptor_path = model.path.join(DESCRIPTOR_FILE);
        let Some(content) = owned::read_regular_file(&descriptor_path)? else {
            return Err(CliError::precondition(format!(
                "{} has no {DESCRIPTOR_FILE}",
                model.path.display()
            )));
        };
        let mut value: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
            CliError::precondition(format!("{} is malformed: {e}", descriptor_path.display()))
        })?;
        let object = value.as_object_mut().ok_or_else(|| {
            CliError::precondition(format!(
                "{} is not a JSON object",
                descriptor_path.display()
            ))
        })?;
        let already = match object.get("enabled") {
            // Absent reads as false: `#[serde(default)]` on both the engine's
            // and the CLI's descriptor parsers.
            None => !enabled,
            Some(serde_json::Value::Bool(current)) => *current == enabled,
            Some(other) => {
                return Err(CliError::precondition(format!(
                    "{} has a non-boolean \"enabled\" field ({other}); refusing to rewrite it",
                    descriptor_path.display()
                ))
                .with_fix("repair the descriptor, or remove and re-pull the model"));
            }
        };
        if !already {
            // Every other key survives verbatim: this rewrites one boolean,
            // not the descriptor's shape.
            object.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
            let mut body = serde_json::to_vec_pretty(&value)
                .map_err(|e| CliError::internal(format!("cannot serialize the descriptor: {e}")))?;
            body.push(b'\n');
            owned::write_atomic(&descriptor_path, &body, 0o644)?;
        }

        // The receipt is reconciled whether or not the descriptor moved. That
        // is what makes rerunning the command the repair for a crash between
        // the two writes: an early return on "already in that state" would
        // leave the stale receipt reporting the model as tampered-with
        // forever, with no command able to fix it.
        let receipt_changed = match &model.receipt {
            Some(receipt) => {
                let mut updated = receipt.clone();
                updated.update_file_hash(DESCRIPTOR_FILE, &model.path)?;
                let stale =
                    receipt
                        .files
                        .iter()
                        .zip(updated.files.iter())
                        .any(|(before, after)| {
                            before.path == DESCRIPTOR_FILE
                                && (before.sha256 != after.sha256 || before.size != after.size)
                        });
                if stale {
                    updated.write(&model.path)?;
                }
                stale
            }
            None => false,
        };
        Ok(if already && !receipt_changed {
            EnabledChange::Unchanged
        } else {
            EnabledChange::Changed
        })
    }

    /// Free bytes on the filesystem holding the models directory.
    pub fn free_disk_bytes(&self) -> Option<u64> {
        let models = self.models_dir();
        let c_path = std::ffi::CString::new(models.as_os_str().as_encoded_bytes()).ok()?;
        let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
        if rc != 0 {
            return None;
        }
        Some(stats.f_bavail as u64 * stats.f_frsize as u64)
    }
}

/// Minimal tolerant descriptor summary (same discipline as
/// `engine/embedded.rs`: never depend on the engine crate's schema).
#[derive(Debug, serde::Deserialize)]
struct DescriptorSummary {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    params: DescriptorParams,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DescriptorParams {
    #[serde(default)]
    model_type: Option<String>,
    #[serde(default)]
    source_model: Option<String>,
    #[serde(default)]
    target_model: Option<String>,
    #[serde(default)]
    target_dim: Option<u32>,
}

fn read_descriptor_summary(path: &Path) -> Option<DescriptorSummary> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

/// Unique-enough suffix for staging/trash names without a rand dependency:
/// pid + monotonic-ish nanos.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}.{nanos}", std::process::id())
}

/// fsync every file and directory beneath `path` so the rename that follows
/// publishes a fully durable tree. Failures propagate: claiming a tree is
/// durable when the sync failed is the one thing this function must not do.
fn fsync_tree(path: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry.map_err(|e| CliError::apply(format!("cannot walk staged tree: {e}")))?;
        let file = fs::File::open(entry.path()).map_err(|e| {
            CliError::apply(format!(
                "cannot open {} to fsync: {e}",
                entry.path().display()
            ))
        })?;
        file.sync_all().map_err(|e| {
            CliError::apply(format!("cannot fsync {}: {e}", entry.path().display()))
        })?;
    }
    Ok(())
}

/// Does dpkg or rpm claim this file? Best effort: absent tools or timeouts
/// mean "no claim", which downgrades the refusal message from "package-owned"
/// to "manual" — the refusal itself does not depend on this answer.
async fn package_owns(path: &Path, timeout: Duration) -> bool {
    if !path.exists() {
        return false;
    }
    for (program, args) in [("dpkg", vec!["-S"]), ("rpm", vec!["-qf"])] {
        let mut cmd = Cmd::new(PathBuf::from(program));
        for arg in args {
            cmd = cmd.arg(arg);
        }
        cmd = cmd.arg(path.display().to_string());
        if let Ok(output) = proc::run(&cmd, timeout).await {
            if output.ok() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn root_with_models() -> (tempfile::TempDir, ModelRoot) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        // The umask may make created directories group-writable, which the
        // trust check rightly refuses; pin the realistic 0755.
        fs::set_permissions(dir.path().join("models"), fs::Permissions::from_mode(0o755)).unwrap();
        // Canonicalized so the ancestor-symlink gate never trips over a
        // symlinked TMPDIR on the test host.
        let root = ModelRoot::new(dir.path().canonicalize().unwrap());
        (dir, root)
    }

    #[test]
    fn a_missing_models_directory_is_a_precondition_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = ModelRoot::new(dir.path().to_path_buf());
        let err = root.lock_exclusive().unwrap_err();
        assert!(err.to_string().contains("not an engine root"), "{err}");
    }

    #[test]
    fn cross_backend_name_collisions_are_found() {
        let (_guard, root) = root_with_models();
        fs::create_dir_all(root.models_dir().join("candle/foo")).unwrap();
        assert!(root
            .find_installed_anywhere("foo")
            .unwrap()
            .unwrap()
            .ends_with("candle/foo"));
        assert!(root.find_installed_anywhere("bar").unwrap().is_none());
    }

    #[test]
    fn install_staged_refuses_an_existing_destination() {
        let (_guard, root) = root_with_models();
        fs::create_dir_all(root.models_dir().join("onnx-runtime/m")).unwrap();
        let staged = root.ensure_staging().unwrap().join("m.stage");
        fs::create_dir_all(&staged).unwrap();
        let err = root
            .install_staged(&staged, "onnx-runtime", "m")
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    #[test]
    fn install_then_inventory_then_remove_round_trips() {
        let (_guard, root) = root_with_models();
        let staged = root.ensure_staging().unwrap().join("m.stage");
        fs::create_dir_all(&staged).unwrap();
        fs::write(
            staged.join(DESCRIPTOR_FILE),
            r#"{"name":"m","enabled":true,"dependencies":[]}"#,
        )
        .unwrap();
        let installed = root.install_staged(&staged, "onnx-runtime", "m").unwrap();
        assert!(installed.is_dir());

        let inventory = root.installed().unwrap();
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory[0].dir_name, "m");
        assert_eq!(inventory[0].descriptor_name.as_deref(), Some("m"));
        assert!(inventory[0].enabled);
        assert!(inventory[0].receipt.is_none());

        root.remove_installed(&installed).unwrap();
        assert!(root.installed().unwrap().is_empty());
    }

    #[test]
    fn sweep_keeps_part_files_and_drops_stage_directories() {
        let (_guard, root) = root_with_models();
        let staging = root.ensure_staging().unwrap();
        fs::write(staging.join("abc.part"), b"partial").unwrap();
        fs::create_dir_all(staging.join("m.123.456")).unwrap();
        let _lock = root.lock_exclusive().unwrap();
        assert!(staging.join("abc.part").exists());
        assert!(!staging.join("m.123.456").exists());
    }

    /// A group-writable models directory is refused for mutation.
    #[test]
    fn group_writable_models_directories_are_refused() {
        let (_guard, root) = root_with_models();
        fs::set_permissions(root.models_dir(), fs::Permissions::from_mode(0o775)).unwrap();
        let err = root.lock_exclusive().unwrap_err();
        assert!(
            err.to_string().contains("group- or world-writable"),
            "{err}"
        );
        fs::set_permissions(root.models_dir(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(root.lock_exclusive().is_ok());
    }

    /// A group-writable, non-sticky ancestor is refused. A writer there
    /// could rename-and-replace the whole managed tree.
    #[test]
    fn group_writable_ancestors_are_refused() {
        let outer = tempfile::tempdir().unwrap();
        fs::set_permissions(outer.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let mid = outer.path().join("shared");
        fs::create_dir_all(mid.join("root/models/onnx-runtime")).unwrap();
        for dir in [mid.join("root/models"), mid.join("root")] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::set_permissions(&mid, fs::Permissions::from_mode(0o775)).unwrap();

        let root = ModelRoot::new(mid.join("root").canonicalize().unwrap());
        let err = root.lock_exclusive().unwrap_err();
        assert!(err.to_string().contains("ancestor"), "{err}");

        // The same chain with the ancestor tightened passes; a *sticky*
        // group-writable ancestor (the /tmp shape) also passes.
        fs::set_permissions(&mid, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(root.lock_exclusive().is_ok());
        fs::set_permissions(&mid, fs::Permissions::from_mode(0o1775)).unwrap();
        assert!(root.lock_exclusive().is_ok());
    }

    /// A root reached through a symlinked ancestor is refused. Another
    /// account owning the link could swap the whole tree.
    #[test]
    fn symlinked_ancestors_are_refused() {
        let (_guard, real_root) = root_with_models();
        let link_holder = tempfile::tempdir().unwrap();
        fs::set_permissions(link_holder.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let link = link_holder.path().join("root-link");
        std::os::unix::fs::symlink(&real_root.root, &link).unwrap();

        let via_link = ModelRoot::new(link);
        let err = via_link.lock_exclusive().unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        // The canonical path itself still works.
        let canonical = ModelRoot::new(real_root.root.canonicalize().unwrap());
        assert!(canonical.lock_exclusive().is_ok());
    }

    /// Install a model directory whose single file marks which revision it is.
    fn plant(root: &ModelRoot, name: &str, marker: &str) -> PathBuf {
        let dir = root.models_dir().join("onnx-runtime").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(DESCRIPTOR_FILE), format!(r#"{{"name":"{name}"}}"#)).unwrap();
        fs::write(dir.join("marker"), marker).unwrap();
        dir
    }

    fn stage(root: &ModelRoot, name: &str, marker: &str) -> PathBuf {
        let staged = root.ensure_staging().unwrap().join(format!("{name}.stage"));
        fs::create_dir_all(&staged).unwrap();
        fs::write(
            staged.join(DESCRIPTOR_FILE),
            format!(r#"{{"name":"{name}"}}"#),
        )
        .unwrap();
        fs::write(staged.join("marker"), marker).unwrap();
        staged
    }

    fn marker(path: &Path) -> String {
        fs::read_to_string(path.join("marker")).unwrap()
    }

    fn one(name: &str) -> Vec<SwapModel> {
        vec![SwapModel {
            name: name.into(),
            backend: "onnx-runtime".into(),
            role: SwapRole::Replace,
            reload_on_rollback: true,
        }]
    }

    /// The happy path: the replacement is in place while the transaction is
    /// open, and confirming releases the predecessor.
    #[test]
    fn a_confirmed_swap_replaces_in_place_and_drops_the_predecessor() {
        let (_guard, root) = root_with_models();
        let installed = plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        root.swap_in(&txn, &stage(&root, "m", "new"), "onnx-runtime", "m")
            .unwrap();
        assert_eq!(marker(&installed), "new");

        root.confirm_swap(&txn).unwrap();
        assert_eq!(marker(&installed), "new");
        assert!(root.pending_swap().unwrap().is_none());
        assert!(!root.models_dir().join(SWAP_DIR).exists());
    }

    /// The blocking case from the audit: a crash after the replacement is on
    /// disk but before anything proved it loadable. Directory presence says
    /// "done"; the record says otherwise, and the record wins.
    #[test]
    fn an_unconfirmed_transaction_recovers_to_the_predecessor() {
        let (_guard, root) = root_with_models();
        let installed = plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        root.swap_in(&txn, &stage(&root, "m", "new"), "onnx-runtime", "m")
            .unwrap();
        assert_eq!(marker(&installed), "new");
        drop(txn); // crash: nothing confirmed the replacement

        // A fresh process sees the transaction and its phase.
        let (recovered, phase) = root.pending_swap().unwrap().expect("recorded");
        assert_eq!(phase, SwapPhase::Swapping);
        assert_eq!(recovered.names(), ["m"]);
        // Restoring is only half of settling: the record survives it, so a
        // command that dies before proving the reload still leaves work
        // behind for the next one.
        root.restore_swapped(&recovered).unwrap();
        assert_eq!(marker(&installed), "old");
        assert!(
            root.pending_swap().unwrap().is_some(),
            "the record must outlive the filesystem restore"
        );
        root.clear_swap(&recovered).unwrap();
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// A crash between the durable "confirmed" write and the deletion is the
    /// other side of the same coin: the replacement is proven, so recovery
    /// discards rather than rolls back.
    #[test]
    fn a_confirmed_transaction_recovers_by_discarding() {
        let (_guard, root) = root_with_models();
        let installed = plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        root.swap_in(&txn, &stage(&root, "m", "new"), "onnx-runtime", "m")
            .unwrap();
        // The commit point, without the cleanup that normally follows it.
        write_swap_record(
            &root.models_dir().join(SWAP_DIR),
            SwapPhase::Confirmed,
            &one("m"),
        )
        .unwrap();

        let (recovered, phase) = root.pending_swap().unwrap().expect("recorded");
        assert_eq!(phase, SwapPhase::Confirmed);
        root.discard_swap(&recovered);
        assert_eq!(marker(&installed), "new");
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// A crash *between* the two renames leaves no model at the destination.
    /// Recovery must put the predecessor back rather than leave a hole.
    #[test]
    fn a_crash_between_the_two_renames_restores_the_predecessor() {
        let (_guard, root) = root_with_models();
        let installed = plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        // Hand-roll the first rename only.
        let parked_dir = root.models_dir().join(SWAP_DIR).join("onnx-runtime");
        fs::create_dir_all(&parked_dir).unwrap();
        fs::rename(&installed, parked_dir.join("m")).unwrap();
        assert!(!installed.exists());
        drop(txn);

        let (recovered, phase) = root.pending_swap().unwrap().expect("recorded");
        assert_eq!(phase, SwapPhase::Swapping);
        root.restore_swapped(&recovered).unwrap();
        assert_eq!(marker(&installed), "old");
        root.clear_swap(&recovered).unwrap();
    }

    /// A partially applied batch rolls back whole: the model that did move
    /// goes back too.
    #[test]
    fn rolling_back_a_batch_restores_every_parked_model() {
        let (_guard, root) = root_with_models();
        let a = plant(&root, "a", "old-a");
        let b = plant(&root, "b", "old-b");
        let models = vec![
            SwapModel {
                name: "a".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Replace,
                reload_on_rollback: true,
            },
            SwapModel {
                name: "b".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Replace,
                reload_on_rollback: true,
            },
        ];
        let txn = root.begin_swap(models).unwrap();
        root.swap_in(&txn, &stage(&root, "a", "new-a"), "onnx-runtime", "a")
            .unwrap();
        // `b` never got its turn — the batch failed here.
        assert_eq!(marker(&a), "new-a");

        root.restore_swapped(&txn).unwrap();
        assert_eq!(marker(&a), "old-a");
        assert_eq!(marker(&b), "old-b");
    }

    /// Two transactions cannot overlap: the second would have nowhere
    /// unambiguous to park, and the first's predecessors must be settled by a
    /// deliberate recovery, not overwritten.
    #[test]
    fn a_second_transaction_cannot_open_over_an_unsettled_one() {
        let (_guard, root) = root_with_models();
        plant(&root, "m", "old");
        let _txn = root.begin_swap(one("m")).unwrap();
        let err = root.begin_swap(one("m")).unwrap_err();
        assert!(err.to_string().contains("unfinished replacement"), "{err}");
    }

    /// Absence is only ever a typed `NotFound`. A lookup that fails for any
    /// other reason leaves the answer unknown, so recovery must refuse and
    /// keep its evidence rather than read "unreadable" as "already done".
    ///
    /// The failure is staged with a regular file where a directory belongs,
    /// so the lookup fails with `ENOTDIR` for every user — a permission-based
    /// fixture would silently pass under a root CI runner.
    #[test]
    fn an_unreadable_path_is_not_treated_as_absent() {
        let (_guard, root) = root_with_models();
        let installed = plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        root.swap_in(&txn, &stage(&root, "m", "new"), "onnx-runtime", "m")
            .unwrap();

        // Replace the parked copy's parent directory with a file: looking the
        // parked predecessor up now fails with ENOTDIR, not NotFound.
        let parked_dir = root.models_dir().join(SWAP_DIR).join("onnx-runtime");
        fs::remove_dir_all(&parked_dir).unwrap();
        fs::write(&parked_dir, b"not a directory").unwrap();

        let err = root
            .restore_swapped(&txn)
            .expect_err("an unreadable parked copy must not read as restored");
        assert!(
            err.to_string().contains("cannot determine whether"),
            "{err}"
        );
        // Nothing was restored and, crucially, the transaction survives.
        assert_eq!(marker(&installed), "new");
        assert!(root.pending_swap().unwrap().is_some());
    }

    /// The same rule for the fresh-install half of a rollback.
    #[test]
    fn an_unreadable_install_destination_is_not_treated_as_removed() {
        let (_guard, root) = root_with_models();
        // A backend whose directory is really a file: the destination lookup
        // under it fails with ENOTDIR.
        fs::write(root.models_dir().join("wedged"), b"not a directory").unwrap();
        let txn = root
            .begin_swap(vec![SwapModel {
                name: "fresh".into(),
                backend: "wedged".into(),
                role: SwapRole::Install,
                reload_on_rollback: false,
            }])
            .unwrap();

        let err = root
            .remove_recorded_installs(&txn)
            .expect_err("an unreadable destination must not read as removed");
        assert!(
            err.to_string().contains("cannot determine whether"),
            "{err}"
        );
        assert!(root.pending_swap().unwrap().is_some());
    }

    /// The cross-backend collision guard must never answer "not installed"
    /// because it could not look. The engine serves the first
    /// directory-name match across backends, so an overlooked backend is how
    /// two copies of one logical model come to exist.
    #[test]
    fn the_collision_guard_refuses_rather_than_overlook_a_backend() {
        let (_guard, root) = root_with_models();
        plant(&root, "m", "old");

        // A symlinked backend: the engine's scan would follow it, so the CLI
        // refuses instead of quietly searching a smaller set.
        let elsewhere = root.root.join("other-backend");
        fs::create_dir_all(elsewhere.join("m")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.models_dir().join("linked")).unwrap();
        let err = root.find_installed_anywhere("m").unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(root
            .installed()
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        fs::remove_file(root.models_dir().join("linked")).unwrap();

        // …and an unenumerable models directory is an error, not "nothing
        // installed". A file where the directory belongs is reported as the
        // engine-root problem it is, not as the ENOTDIR of whichever
        // enumeration happened to touch it first.
        let (_guard, wedged) = root_with_models();
        fs::remove_dir_all(wedged.models_dir()).unwrap();
        fs::write(wedged.models_dir(), b"not a directory").unwrap();
        let err = wedged.find_installed_anywhere("m").unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
        assert!(err.to_string().contains("not an engine root"), "{err}");
        assert!(wedged.installed().is_err());

        // A plain file beside the backends is not a backend, and not an error.
        fs::write(root.models_dir().join("README"), b"notes").unwrap();
        assert!(root.find_installed_anywhere("m").unwrap().is_some());
        assert_eq!(root.installed().unwrap().len(), 1);
    }

    /// The staging/trash sweep must never touch a parked predecessor.
    #[test]
    fn the_staging_sweep_leaves_the_swap_transaction_alone() {
        let (_guard, root) = root_with_models();
        plant(&root, "m", "old");
        let txn = root.begin_swap(one("m")).unwrap();
        root.swap_in(&txn, &stage(&root, "m", "new"), "onnx-runtime", "m")
            .unwrap();
        drop(txn);
        drop(root.lock_exclusive().unwrap());
        assert!(root.pending_swap().unwrap().is_some());
    }

    /// Install a CLI-owned model with a real receipt, so the flip has
    /// something to keep truthful.
    fn plant_cli_owned(root: &ModelRoot, name: &str, enabled: bool) -> PathBuf {
        use crate::registry::archive::ExtractedFile;
        use crate::registry::identity::Identity;
        use crate::registry::index::{ArchiveInfo, IndexModel};
        use sha2::{Digest, Sha256};

        let dir = root.models_dir().join("onnx-runtime").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // An unrelated key rides along so the flip can be shown to preserve it.
        let descriptor = serde_json::json!({
            "name": name,
            "backend": "onnx-runtime",
            "enabled": enabled,
            "params": { "model_type": "embed", "target_dim": 384 },
            "vendor_extension": { "keep": "me" }
        });
        let body = format!("{}\n", serde_json::to_string_pretty(&descriptor).unwrap());
        fs::write(dir.join(DESCRIPTOR_FILE), &body).unwrap();

        let model = IndexModel {
            name: name.into(),
            access: "public".into(),
            model_type: "embed".into(),
            backend: "onnx-runtime".into(),
            quantization: None,
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
            sequence_len: None,
            license: Some("mit".into()),
            license_version: None,
            license_url: None,
            license_acceptance: None,
            source: None,
            dependencies: vec![],
            postvec_requires: vec![],
            min_postvec_version: None,
            published_at: None,
            summary: None,
            eval: None,
            withdrawn: false,
            revision: Some(1),
            required_entitlements: vec![],
            archive: ArchiveInfo {
                digest: format!("sha256:{}", "ab".repeat(32)),
                size: 10,
                installed_size: 5,
                sources: vec!["https://example.invalid/a".into()],
            },
        };
        let files = vec![ExtractedFile {
            path: DESCRIPTOR_FILE.to_string(),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body.as_bytes())),
        }];
        Receipt::new(
            &model,
            None,
            &files,
            &Identity {
                model_type: "embed".into(),
                backend: "onnx-runtime".into(),
                source_model: None,
                target_model: None,
                source_dim: None,
                target_dim: Some(384),
            },
            None,
        )
        .write(&dir)
        .unwrap();
        dir
    }

    fn installed_named<'a>(inventory: &'a [InstalledModel], name: &str) -> &'a InstalledModel {
        inventory.iter().find(|m| m.dir_name == name).unwrap()
    }

    /// The whole persistence mechanism: one boolean in the installed
    /// descriptor, every other key untouched, and the receipt kept truthful so
    /// a deactivated model does not read as tampered-with.
    #[test]
    fn setting_enabled_flips_one_field_and_keeps_the_receipt_verifiable() {
        let (_guard, root) = root_with_models();
        let dir = plant_cli_owned(&root, "m", true);

        let inventory = root.installed().unwrap();
        let model = installed_named(&inventory, "m");
        assert!(model.enabled);
        assert!(
            model
                .receipt
                .as_ref()
                .unwrap()
                .verify_files(&dir)
                .is_empty(),
            "the fixture must start verifiable"
        );

        assert_eq!(
            root.set_enabled(model, false).unwrap(),
            EnabledChange::Changed
        );

        // The descriptor says disabled, and nothing else moved.
        let descriptor: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(DESCRIPTOR_FILE)).unwrap()).unwrap();
        assert_eq!(descriptor["enabled"], serde_json::json!(false));
        assert_eq!(descriptor["name"], "m");
        assert_eq!(descriptor["vendor_extension"]["keep"], "me");
        assert_eq!(descriptor["params"]["target_dim"], 384);

        // The receipt still verifies, and still names the published archive.
        let after = root.installed().unwrap();
        let reloaded = installed_named(&after, "m");
        assert!(!reloaded.enabled);
        let receipt = reloaded.receipt.as_ref().unwrap();
        assert!(
            receipt.verify_files(&dir).is_empty(),
            "a deactivated model must not read as corrupt: {:?}",
            receipt.verify_files(&dir)
        );
        assert_eq!(
            receipt.archive_digest,
            format!("sha256:{}", "ab".repeat(32))
        );
        assert_eq!(receipt.revision(), 1);

        // …and back again, idempotently.
        assert_eq!(
            root.set_enabled(reloaded, false).unwrap(),
            EnabledChange::Unchanged
        );
        assert_eq!(
            root.set_enabled(reloaded, true).unwrap(),
            EnabledChange::Changed
        );
        let again = root.installed().unwrap();
        assert!(installed_named(&again, "m").enabled);
    }

    /// The crash window between the two writes is detectable rather than
    /// silent: the descriptor is authoritative for the engine, and the stale
    /// receipt is what `--verify` and `doctor --deep` report.
    #[test]
    fn a_stale_receipt_after_a_flip_is_reported_on_the_descriptor_alone() {
        let (_guard, root) = root_with_models();
        let dir = plant_cli_owned(&root, "m", true);
        let inventory = root.installed().unwrap();
        let receipt = installed_named(&inventory, "m").receipt.clone().unwrap();

        // Exactly the state a crash between step 1 and step 2 leaves.
        let mut value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(DESCRIPTOR_FILE)).unwrap()).unwrap();
        value["enabled"] = serde_json::json!(false);
        fs::write(
            dir.join(DESCRIPTOR_FILE),
            format!("{}\n", serde_json::to_string_pretty(&value).unwrap()),
        )
        .unwrap();

        let problems = receipt.verify_files(&dir);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with(DESCRIPTOR_FILE), "{problems:?}");

        // Rerunning the same command repairs it, which is the documented
        // recovery: the reconciliation is not skipped just because the
        // descriptor already says what was asked for.
        let inventory = root.installed().unwrap();
        assert_eq!(
            root.set_enabled(installed_named(&inventory, "m"), false)
                .unwrap(),
            EnabledChange::Changed,
            "a stale receipt is work, even when the descriptor is already right"
        );
        let repaired = root.installed().unwrap();
        let model = installed_named(&repaired, "m");
        assert!(!model.enabled);
        assert!(model
            .receipt
            .as_ref()
            .unwrap()
            .verify_files(&dir)
            .is_empty());
    }

    /// A descriptor whose `enabled` is not a boolean is a repair job, not
    /// something to overwrite: rewriting it would discard whatever the
    /// operator meant by it.
    #[test]
    fn a_non_boolean_enabled_field_is_refused() {
        let (_guard, root) = root_with_models();
        let dir = plant_cli_owned(&root, "m", true);
        fs::write(
            dir.join(DESCRIPTOR_FILE),
            r#"{"name":"m","backend":"onnx-runtime","enabled":"yes"}"#,
        )
        .unwrap();
        let inventory = root.installed().unwrap();
        let err = root
            .set_enabled(installed_named(&inventory, "m"), false)
            .unwrap_err();
        assert!(err.to_string().contains("non-boolean"), "{err}");
    }

    /// An absent `enabled` field reads as `false` on both parsers, so asking
    /// for `false` is a no-op rather than a rewrite.
    #[test]
    fn an_absent_enabled_field_reads_as_disabled() {
        let (_guard, root) = root_with_models();
        let dir = plant_cli_owned(&root, "m", true);
        fs::write(
            dir.join(DESCRIPTOR_FILE),
            r#"{"name":"m","backend":"onnx-runtime"}"#,
        )
        .unwrap();
        let inventory = root.installed().unwrap();
        let model = installed_named(&inventory, "m");
        assert!(!model.enabled);
        // The hand-edit above left the receipt stale, so the first call still
        // has the reconciliation to do; the second has nothing at all.
        root.set_enabled(model, false).unwrap();
        let settled = root.installed().unwrap();
        assert_eq!(
            root.set_enabled(installed_named(&settled, "m"), false)
                .unwrap(),
            EnabledChange::Unchanged
        );
        assert_eq!(
            root.set_enabled(installed_named(&settled, "m"), true)
                .unwrap(),
            EnabledChange::Changed
        );
    }

    /// A rollback reloads only the predecessors the engine actually held. A
    /// deactivated model's predecessor was never resident, and asking the
    /// engine for it would be refused — failing the very proof the rollback
    /// exists to make.
    #[test]
    fn only_engine_held_predecessors_are_reloaded_on_rollback() {
        let (_guard, root) = root_with_models();
        plant(&root, "live", "old");
        plant(&root, "off", "old");
        let txn = root
            .begin_swap(vec![
                SwapModel {
                    name: "live".into(),
                    backend: "onnx-runtime".into(),
                    role: SwapRole::Replace,
                    reload_on_rollback: true,
                },
                SwapModel {
                    name: "off".into(),
                    backend: "onnx-runtime".into(),
                    role: SwapRole::Replace,
                    reload_on_rollback: false,
                },
            ])
            .unwrap();
        // Both are quiesced — the engine may hold new bytes for either.
        assert_eq!(txn.names(), ["live", "off"]);
        // Only one is reloaded.
        assert_eq!(txn.replaced_names(), ["live"]);
        // …and both are restored on disk, because both parked a predecessor.
        root.swap_in(&txn, &stage(&root, "live", "new"), "onnx-runtime", "live")
            .unwrap();
        root.swap_in(&txn, &stage(&root, "off", "new"), "onnx-runtime", "off")
            .unwrap();
        root.restore_swapped(&txn).unwrap();
        for name in ["live", "off"] {
            assert_eq!(
                marker(&root.models_dir().join("onnx-runtime").join(name)),
                "old"
            );
        }
    }

    #[test]
    fn hidden_directories_are_not_backends() {
        let (_guard, root) = root_with_models();
        root.ensure_staging().unwrap();
        fs::create_dir_all(root.models_dir().join(".trash/x")).unwrap();
        assert!(root.installed().unwrap().is_empty());
        assert!(root.find_installed_anywhere("x").unwrap().is_none());
    }
}
