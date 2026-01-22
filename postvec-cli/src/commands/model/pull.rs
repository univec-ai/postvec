//! `postvec model pull` and `postvec model upgrade`.
//!
//! One pipeline, two modes. Order of operations: resolve target → resolve
//! channel index → expand the closure → preflight every entry against the
//! root (cross-backend collision, ownership, revision comparison, the
//! identity contract, `min_postvec_version`, disk space) → plan → confirm →
//! download all archives (≤4 concurrently, resumable, digest-verified) →
//! stage → install or swap in dependency order → activate → refresh the SQL
//! caches. Downloads happen before any install, so an interrupted run leaves
//! either resumable `.part` files or complete models — never half a model.
//!
//! The two modes differ in exactly three places:
//!
//! - **`pull` never replaces bytes.** An installed name whose head revision
//!   moved is a no-op that names the upgrade command. An upgrade that
//!   happened because the registry moved would be the background updater this
//!   design rejects, arriving through the front door.
//! - **`upgrade` replaces in place**, under the atomic swap in
//!   [`crate::registry::root::ModelRoot::swap_in`], and re-checks the
//!   identity contract against the install's own receipt first — so no
//!   registry can move an installed column's vector space.
//! - **`upgrade` refuses a lower or equal revision.** There is no downgrade
//!   flag; a genuine rollback is published forwards as a new revision.
//!
//! ## Installing is not activating
//!
//! A fresh install lands **deactivated**: the staged descriptor's `enabled`
//! field is flipped to `false` before the tree is published, so the model is
//! neither hot-loaded now nor scan-loaded at the next restart.
//! `postvec model activate` is the only command that turns a model on.
//!
//! Leaving the published `enabled: true` in place and merely skipping the hot
//! load would not be "pull does not activate" — it would defer activation to
//! the next PostgreSQL restart, which is activation through another door.
//!
//! An **upgrade** carries the previous copy's bit forward: replacing a model's
//! bytes is not a decision about whether it should serve. A deactivated model
//! therefore upgrades as a pure filesystem swap — nothing is unloaded, nothing
//! is loaded, and the rollback path knows not to try.

use crate::cli::{Cli, ModelPullArgs, ModelUpgradeArgs};
use crate::commands::model::{
    self, admin, fetch_channel_index, require_root, resolve_target_for_mutation, terms, ModelTarget,
};
use crate::commands::Context;
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use crate::proc::{self, InterruptGuard};
use crate::registry::client::{DownloadError, Progress, RegistryClient};
use crate::registry::identity::{self, Identity};
use crate::registry::index::{ClosureEntry, Index, IndexModel, PullReason};
use crate::registry::root::{ModelRoot, Ownership};
use crate::registry::urls;
use futures_util::StreamExt;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

/// Up to four archives in flight per command.
const DOWNLOAD_CONCURRENCY: usize = 4;
/// Free disk must cover archive + installed + 10%. An upgrade needs that
/// on top of the copy it replaces, which is what the sum below computes.
/// The existing copy is never subtracted.
const DISK_MARGIN: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Install,
    Upgrade,
}

impl Mode {
    fn command(self) -> &'static str {
        match self {
            Mode::Install => "model pull",
            Mode::Upgrade => "model upgrade",
        }
    }
}

/// The two commands' arguments, normalized.
struct Request {
    mode: Mode,
    names: Vec<String>,
    /// `upgrade --all`: the name list is derived from the installed set.
    all: bool,
    path: Option<PathBuf>,
    api_key_file: Option<PathBuf>,
    /// Raw `--accept-license <id>@<version>` tokens; parsed strictly before
    /// anything else happens.
    accept_license: Vec<String>,
    yes: bool,
    dry_run: bool,
}

pub async fn run(cli: &Cli, args: ModelPullArgs, output: &Output) -> Result<Exit> {
    let names = args.validated()?;
    execute(
        cli,
        Request {
            mode: Mode::Install,
            names,
            all: false,
            path: args.path,
            api_key_file: args.api_key_file,
            accept_license: args.accept_license,
            yes: args.yes,
            dry_run: args.dry_run,
        },
        output,
    )
    .await
}

pub async fn run_upgrade(cli: &Cli, args: ModelUpgradeArgs, output: &Output) -> Result<Exit> {
    let names = args.validated()?;
    execute(
        cli,
        Request {
            mode: Mode::Upgrade,
            names,
            all: args.all,
            path: args.path,
            api_key_file: args.api_key_file,
            accept_license: args.accept_license,
            yes: args.yes,
            dry_run: args.dry_run,
        },
        output,
    )
    .await
}

async fn execute(cli: &Cli, request: Request, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    // Malformed or duplicate terms flags fail before anything is touched —
    // dry run included.
    let accepted_flags = terms::parse_accept_license(&request.accept_license)?;

    let mut target = resolve_target_for_mutation(cli, request.path.as_deref(), output).await?;
    require_root(&target, request.mode.command())?;

    // Channel and closure before the lock: a typo'd name or a dead network
    // should not block concurrent commands on the root.
    let (index, credential) =
        fetch_channel_index(cli.timeout, request.api_key_file.as_deref(), output).await?;
    output.note(&format!(
        "registry channel: {} ({} models)",
        index.channel,
        index.models.len()
    ));

    let root = require_root(&target, request.mode.command())?.clone();

    // The lock comes first, and pending recovery immediately after it: an
    // interrupted replacement may already have put the head revision on disk,
    // and a pre-lock scan would then see nothing outdated and report success
    // while leaving the transaction unsettled.
    // A dry run asserts every precondition the real run will, without taking
    // the exclusive lock: a preview that reports a clean plan and is then
    // followed by "refusing to manage it" has told the operator nothing.
    let _shared = if request.dry_run {
        root.check_mutable()?;
        root.lock_shared()?
    } else {
        None
    };
    let lock = if request.dry_run {
        None
    } else {
        let lock = root.lock_exclusive()?;
        model::recover_pending_swap(&root, &target, cli.timeout, output).await?;
        Some(lock)
    };
    if request.dry_run && root.pending_swap()?.is_some() {
        output.note(
            "an interrupted model replacement is pending in this root; a real run recovers it \
             first (a dry run changes nothing)",
        );
    }

    let names = match request.all {
        true => outdated_installed(&root, &index)?,
        false => request.names.clone(),
    };
    if names.is_empty() {
        // Only reachable through `--all`; an empty explicit list is a usage
        // error caught in `validated()`. A terms flag supplied to a run that
        // needs nothing is stale, same as everywhere else.
        if let Some(stale) = accepted_flags.first() {
            return Err(CliError::usage(format!(
                "--accept-license {}@{} is stale: this run installs and replaces nothing, so \
                 no terms document needs acknowledgement; remove the flag",
                stale.license, stale.version
            )));
        }
        let plan = Plan::new(request.mode.command(), target.label());
        let result = finish(
            plan,
            ApplyJournal::default(),
            vec!["every CLI-installed model is at the registry's head revision".to_string()],
            started,
            started_at,
            None,
            &target,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    let closure = crate::registry::index::expand_closure(&index, &names)?;

    // Preflight every closure entry against the root.
    let work = preflight(request.mode, &root, &closure, cli.timeout).await?;
    let wanted: Vec<&IndexModel> = work.items.iter().map(|item| item.entry.model).collect();
    if let ModelTarget::Embedded {
        context: Some(context),
        settings,
        ..
    } = &mut target
    {
        check_cluster_compatibility(context, settings, &work.items, output).await?;
    } else if let ModelTarget::Embedded { context: None, .. } = &target {
        if !work.items.is_empty() {
            output.note(
                "no database connection: extension compatibility is not checked; the serving \
                 side decides at load time",
            );
        }
    } else if !work.items.is_empty() {
        for model in &wanted {
            if model.min_postvec_version.is_some() {
                output.note(&format!(
                    "{}: min_postvec_version is not checked under --path (no extension to \
                     compare against)",
                    model.name
                ));
            }
        }
        // With no database to ask, compatibility is unknown, not passed.
        // Say so instead of implying a check happened.
        output.note(
            "--path: backend/build compatibility is unknown (no extension to ask); the \
             serving side decides at load time",
        );
    }
    check_disk_space(&root, &wanted)?;

    if let ModelTarget::Embedded { settings, .. } = &target {
        check_replacements_are_loadable(&settings.embedded_models(), &work)?;
    }

    // The plan.
    let mut plan = Plan::new(request.mode.command(), target.label());
    for skipped in &work.already_installed {
        output.note(&format!("{skipped}: already installed (no-op)"));
    }
    for note in &work.notes {
        output.note(note);
    }
    for item in &work.items {
        plan.push(PlanStep::DownloadArchive {
            name: item.entry.model.name.clone(),
            bytes: item.entry.model.archive.size,
            reason: item.entry.reason.describe(),
        });
        match &item.action {
            Action::Install => plan.push(PlanStep::InstallModel {
                name: item.entry.model.name.clone(),
                backend: item.entry.model.backend.clone(),
            }),
            Action::Upgrade { from_revision } => plan.push(PlanStep::UpgradeModel {
                name: item.entry.model.name.clone(),
                backend: item.entry.model.backend.clone(),
                from_revision: *from_revision,
                to_revision: item.entry.model.revision(),
                embed: item.entry.model.model_type == "embed",
            }),
        }
    }
    let (to_activate, unlisted) = activation_set(&target, &work);
    if !to_activate.is_empty() {
        plan.push(PlanStep::LoadModels {
            models: to_activate.clone(),
        });
    }
    // A fresh install lands deactivated, and the engine's startup preflight
    // treats a *listed* model that is disabled as an error, not a skip. So a
    // name on `postvec.embedded_models` must be activated before the next
    // restart or the engine will not come back at all.
    if let ModelTarget::Embedded { settings, .. } = &target {
        let allow_list = settings.embedded_models();
        let listed_and_fresh: Vec<String> = work
            .items
            .iter()
            .filter(|item| !item.is_upgrade())
            .map(|item| item.entry.model.name.clone())
            .filter(|name| allow_list.contains(name))
            .collect();
        if !listed_and_fresh.is_empty() {
            output.note(&format!(
                "IMPORTANT: {} {} named in postvec.embedded_models and will land deactivated. \
                 An explicitly preloaded model that is disabled fails engine startup — run \
                 `sudo postvec model activate {}` before the next cluster restart",
                listed_and_fresh.join(", "),
                if listed_and_fresh.len() == 1 {
                    "is"
                } else {
                    "are"
                },
                listed_and_fresh.join(" ")
            ));
        }
    }
    if let ModelTarget::Embedded { settings, .. } = &target {
        for database in settings.configured_databases() {
            plan.push(PlanStep::RefreshModelCache { database });
        }
    }

    // The terms plan: the exact documents covering what will actually
    // be downloaded or replaced (already-installed no-ops excluded), with
    // acknowledgement evidence honoured from the receipts being replaced.
    let terms_inputs: Vec<terms::TermsInput<'_>> = work
        .items
        .iter()
        .map(|item| terms::TermsInput {
            model: item.entry.model,
            installed_receipt: item.installed_receipt.as_ref(),
        })
        .collect();
    let terms_plan = terms::build(&terms_inputs)?;
    plan.terms = terms_plan.documents.clone();

    let licences: BTreeSet<String> = wanted.iter().filter_map(|m| m.license.clone()).collect();
    let download_total: u64 = wanted.iter().map(|m| m.archive.size).sum();
    let installed_total: u64 = wanted.iter().map(|m| m.archive.installed_size).sum();
    plan.inference = Some(format!(
        "{} download · {} installed · licences: {}",
        model::human_bytes(download_total),
        model::human_bytes(installed_total),
        if licences.is_empty() {
            "(none listed)".to_string()
        } else {
            licences.into_iter().collect::<Vec<_>>().join(", ")
        }
    ));
    if work.items.iter().any(|item| item.is_upgrade())
        && matches!(target, ModelTarget::Embedded { .. })
    {
        output.note(
            "the embedded engine serves no requests for a replaced model between its unload \
             and reload; worker batches retry, one-shot embed() and search() calls in that \
             window fail",
        );
    }
    output.show_plan(&plan);

    // Terms acknowledgement resolves before any download and before the
    // ordinary plan confirmation. Stale flags fail here in every mode.
    // A dry run asks nothing and tolerates missing flags: the plan above
    // printed the exact tokens.
    let acknowledged = terms::resolve(
        &terms_plan.documents,
        &accepted_flags,
        request.dry_run,
        Prompt::from_environment(),
        terms::interactive_acknowledgement,
    )?;

    if request.dry_run {
        let result = finish(
            plan,
            ApplyJournal::default(),
            vec!["--dry-run: nothing was changed".to_string()],
            started,
            started_at,
            None,
            &target,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    if work.items.is_empty() {
        let result = finish(
            plan,
            ApplyJournal::default(),
            vec![match request.mode {
                Mode::Install => "everything requested is already installed".to_string(),
                Mode::Upgrade => {
                    "everything requested is already at the registry's head revision".to_string()
                }
            }],
            started,
            started_at,
            None,
            &target,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    plan::confirm(&plan, request.yes, None, Prompt::from_environment())?;
    let _lock = lock; // held for the whole mutation

    // The per-model receipt evidence: preserved from the receipt being
    // replaced when it already covers the exact document, otherwise the
    // acknowledgement obtained above.
    let license_evidence: std::collections::BTreeMap<
        String,
        crate::registry::receipt::LicenseEvidence,
    > = work
        .items
        .iter()
        .filter_map(|item| {
            let model = item.entry.model;
            terms::evidence_for(model, &terms_plan.preserved, &acknowledged)
                .map(|evidence| (model.name.clone(), evidence))
        })
        .collect();

    let mut journal = ApplyJournal::default();

    // Phase 1: download everything (resumable, verified) before installing
    // anything.
    let archives = download_all(
        &root,
        &wanted,
        credential.as_ref().map(|c| c.key.clone()),
        cli.timeout,
        output,
    )
    .await?;

    // Phase 2: apply the whole batch as one transaction, under the interrupt
    // guard. Either every model in the closure lands and loads, or the root
    // is returned to exactly the state it was in.
    let guard = InterruptGuard::hold("finishing model installs — interrupt again to abort")?;
    let applied = apply(
        &root,
        &target,
        &work,
        &archives,
        &to_activate,
        &license_evidence,
        cli.timeout,
        &mut journal,
    )
    .await;
    drop(guard);
    applied?;

    // Phase 3: SQL cache refresh, cluster targets only.
    if let ModelTarget::Embedded { settings, .. } = &target {
        if !unlisted.is_empty() {
            journal.incomplete(format!(
                "postvec.embedded_models is an explicit list and does not include {}; \
                 to load them at startup run: {}",
                unlisted.join(", "),
                model::setup_change_hint(settings, &unlisted)
            ));
        }
    }
    model::refresh_databases(&mut target, &mut journal).await;

    // Nothing this command installed is serving yet. Naming the exact next
    // command is the whole reason `pull` is allowed to stop short of it.
    let fresh: Vec<String> = work
        .items
        .iter()
        .filter(|item| !item.is_upgrade())
        .map(|item| item.entry.model.name.clone())
        .collect();
    let next_step = journal
        .failed_databases
        .is_empty()
        .then(|| match (&target, fresh.is_empty()) {
            (ModelTarget::Path(root), false) => Some(format!(
                "models are on disk under {} and deactivated; copy the root to the serving host, \
                 then run: postvec model activate {}",
                root.models_dir().display(),
                fresh.join(" ")
            )),
            (_, false) => Some(format!(
                "installed but not serving; run: sudo postvec model activate {}",
                fresh.join(" ")
            )),
            // An upgrade-only batch installed nothing new; whatever was
            // serving before is serving again, and whatever was deactivated
            // still is.
            (_, true) => None,
        })
        .flatten();

    let result = finish(
        plan,
        journal,
        Vec::new(),
        started,
        started_at,
        next_step,
        &target,
    );
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

/// What preflight decided to do with one closure entry.
#[derive(Debug)]
enum Action {
    Install,
    Upgrade { from_revision: u64 },
}

#[derive(Debug)]
struct WorkItem<'a> {
    entry: ClosureEntry<'a>,
    action: Action,
    /// The receipt of the installation being replaced (upgrades only), so
    /// the terms plan can honour an acknowledgement it already records.
    installed_receipt: Option<crate::registry::receipt::Receipt>,
    /// The serving state the installed copy must land in: `false` for a fresh
    /// install (installing is not activating), the previous copy's bit for an
    /// upgrade (replacing bytes is not a decision about serving).
    enabled_after: bool,
}

impl WorkItem<'_> {
    fn is_upgrade(&self) -> bool {
        matches!(self.action, Action::Upgrade { .. })
    }

    /// Whether this item takes part in the engine at all. A deactivated model
    /// is a pure filesystem change: nothing to unload, nothing to load, and
    /// nothing for a rollback to put back into memory.
    fn touches_the_engine(&self) -> bool {
        self.enabled_after
    }
}

/// Everything preflight decided about the closure.
#[derive(Debug)]
struct PullWork<'a> {
    items: Vec<WorkItem<'a>>,
    already_installed: Vec<String>,
    /// Situations reported rather than acted on: an available update under
    /// `pull`, an outdated dependency under `upgrade`, a stale index.
    notes: Vec<String>,
}

/// `upgrade --all`: every CLI-installed model the registry offers a newer,
/// non-withdrawn revision of. Read-only; the exclusive lock follows.
fn outdated_installed(root: &ModelRoot, index: &Index) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for installed in root.installed()? {
        let Some(receipt) = &installed.receipt else {
            continue; // package-owned or manual: never the CLI's to replace
        };
        let Some(model) = index.model(&installed.dir_name) else {
            continue;
        };
        if model.withdrawn || model.backend != installed.backend {
            continue;
        }
        if model.revision() > receipt.revision() {
            names.push(installed.dir_name.clone());
        }
    }
    Ok(names)
}

async fn preflight<'a>(
    mode: Mode,
    root: &ModelRoot,
    closure: &[ClosureEntry<'a>],
    timeout: std::time::Duration,
) -> Result<PullWork<'a>> {
    let mut work = PullWork {
        items: Vec::new(),
        already_installed: Vec::new(),
        notes: Vec::new(),
    };
    let inventory = root.installed()?;

    for entry in closure {
        let model = entry.model;
        let requested = entry.reason == PullReason::Requested;
        let Some(existing_path) = root.find_installed_anywhere(&model.name)? else {
            if mode == Mode::Upgrade && requested {
                return Err(CliError::precondition(format!(
                    "{} is not installed under {}",
                    model.name,
                    root.models_dir().display()
                ))
                .with_fix("install it first with `postvec model pull`"));
            }
            // A dependency the newer revision needs is installed as part of
            // the upgrade closure — deactivated, like any other fresh install.
            work.items.push(WorkItem {
                entry: entry.clone(),
                action: Action::Install,
                installed_receipt: None,
                enabled_after: false,
            });
            continue;
        };
        let existing_backend = existing_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        if existing_backend != model.backend {
            return Err(cross_backend_error(model, &existing_path));
        }
        let installed = inventory
            .iter()
            .find(|m| m.dir_name == model.name && m.backend == existing_backend);

        let Some(receipt) = installed.and_then(|m| m.receipt.as_ref()) else {
            // Present but not CLI-owned: distinguish package vs manual in the
            // refusal. Ownership is never overridable, in either mode.
            let ownership = match installed {
                Some(m) => m.ownership(timeout).await,
                None => Ownership::Manual,
            };
            return Err(CliError::precondition(format!(
                "{} already exists at {} and is {}",
                model.name,
                existing_path.display(),
                ownership.describe()
            ))
            .with_fix(match ownership {
                Ownership::Package => {
                    "the package manager owns it; upgrade the package instead".to_string()
                }
                _ => format!(
                    "remove {} manually if it should be replaced by the registry copy",
                    existing_path.display()
                ),
            }));
        };

        let installed_revision = receipt.revision();
        let head_revision = model.revision();

        if head_revision == installed_revision {
            if receipt.archive_digest == model.archive.digest {
                work.already_installed.push(model.name.clone());
                continue;
            }
            // Identical revisions must mean identical bytes: this is a
            // publication or integrity fault, never an update.
            return Err(CliError::precondition(format!(
                "{} is installed at revision {installed_revision} with archive digest {} but \
                 the registry offers {} for the same revision; one of the two is wrong",
                model.name, receipt.archive_digest, model.archive.digest
            ))
            .with_fix(
                "verify the install with `postvec model show --verify`; if it is intact, \
                 report this registry inconsistency",
            ));
        }

        if head_revision < installed_revision {
            if mode == Mode::Upgrade && requested {
                return Err(CliError::precondition(format!(
                    "{}: the registry offers revision {head_revision}; revision \
                     {installed_revision} is installed",
                    model.name
                ))
                .with_fix(
                    "there is no downgrade; a genuine rollback is published forwards as a new \
                     revision. A stale authenticated index caches for a minute — retry, or \
                     check the registry",
                ));
            }
            work.notes.push(format!(
                "{}: revision {installed_revision} is installed but the registry currently \
                 offers {head_revision} — a cached or rolled-back index",
                model.name
            ));
            continue;
        }

        // head_revision > installed_revision.
        if mode == Mode::Install || !requested {
            work.notes.push(format!(
                "{}: revision {installed_revision} installed, {head_revision} available — run \
                 `postvec model upgrade {}`",
                model.name, model.name
            ));
            continue;
        }

        // The client's own copy of the identity contract: even a
        // registry served over a valid TLS session cannot move an installed
        // column's vector space, because this compares against data the CLI
        // wrote at install time.
        let next = Identity::of_index(model);
        let refuse = |problem: String| {
            CliError::precondition(format!("{}: {problem}", model.name)).with_fix(
                "this is not a revision of the installed model; if the registry is right, \
                 remove the install and pull the new model under its own name",
            )
        };
        match receipt.identity() {
            Some(installed) => identity::check_identity(&installed, &next).map_err(refuse)?,
            None => {
                // A receipt written before the identity block: model type and
                // backend are still exact, the vector space is genuinely
                // unknown, and saying so is the honest report.
                identity::check_identity_core(&receipt.model_type, &receipt.backend, &next)
                    .map_err(refuse)?;
                work.notes.push(format!(
                    "{}: this install predates the recorded identity block, so its vector \
                     space and dimensions could not be re-checked locally — only the model \
                     type and backend were",
                    model.name
                ));
            }
        }

        // Replacing a model's bytes is not a decision about whether it should
        // serve, so the operator's power switch is carried across unchanged.
        let enabled_after = installed.is_some_and(|m| m.enabled);
        if !enabled_after {
            work.notes.push(format!(
                "{}: deactivated, so the replacement is a filesystem swap only — nothing is \
                 unloaded or loaded, and it stays deactivated afterwards",
                model.name
            ));
        }
        work.items.push(WorkItem {
            entry: entry.clone(),
            action: Action::Upgrade {
                from_revision: installed_revision,
            },
            installed_receipt: Some(receipt.clone()),
            enabled_after,
        });
    }
    Ok(work)
}

/// A replacement excluded by an explicit `postvec.embedded_models` list
/// cannot be proven loadable online — the engine would never be asked to load
/// it — so its predecessor could only be discarded on faith. Refuse, and
/// point at the offline path. An empty list means "scan everything", so
/// nothing is excluded.
///
/// A **deactivated** model is exempt: its predecessor was not resident either,
/// so the swap risks no availability and there is nothing an engine load could
/// prove. That is the same position `--path` has always held.
fn check_replacements_are_loadable(allow_list: &[String], work: &PullWork<'_>) -> Result<()> {
    if allow_list.is_empty() {
        return Ok(());
    }
    let excluded: Vec<String> = work
        .items
        .iter()
        .filter(|item| item.is_upgrade() && item.touches_the_engine())
        .map(|item| item.entry.model.name.clone())
        .filter(|name| !allow_list.contains(name))
        .collect();
    if excluded.is_empty() {
        return Ok(());
    }
    Err(CliError::precondition(format!(
        "{} cannot be replaced online: postvec.embedded_models is an explicit list that does \
         not include {}, so the engine would never be asked to load the replacement and \
         nothing could prove it works",
        excluded.join(", "),
        if excluded.len() == 1 { "it" } else { "them" }
    ))
    .with_fix(
        "either add the model to the list (postvec setup --embedded --model …) and rerun, or \
         stop the cluster and upgrade the root directly with --path <DIR>",
    ))
}

fn cross_backend_error(model: &IndexModel, existing: &std::path::Path) -> CliError {
    CliError::precondition(format!(
        "{} already exists under a different backend ({}); the engine resolves a model by \
         its first directory-name match, so a second copy would be a silent wrong-model bug",
        model.name,
        existing.display()
    ))
    .with_fix("remove the existing directory first if the registry copy should replace it")
}

/// Everything this transaction changed, so any later step can undo all of it.
#[derive(Default)]
struct Landed {
    transaction: Option<crate::registry::root::SwapTransaction>,
    replaced: Vec<String>,
    installed: Vec<(String, std::path::PathBuf)>,
}

/// Apply the whole batch as one **runtime** transaction, not merely a
/// filesystem one.
///
/// Order, and why:
///
/// 1. stage every archive — a bad download cannot leave the root half-updated;
/// 2. check each staged descriptor's identity against the install receipt —
///    the engine consumes the descriptor, so that is the identity that could
///    move a column's vector space;
/// 3. **record the transaction durably, before the first engine call** — an
///    unload that times out may have been applied in part, and the next
///    command must find that state;
/// 4. quiesce: unload every replacement, exact result set verified;
/// 5. move everything into place;
/// 6. load, exact result set verified, requiring a genuine `loaded` for each
///    replacement;
/// 7. confirm — and only now may a predecessor be discarded.
///
/// Any failure from step 3 onward runs the rollback protocol: quiesce
/// everything this command could have made resident, restore the
/// predecessors, remove fresh installs, reload the complete predecessor set,
/// and clear the record last. If any of that cannot be proven, the record and
/// every copy are retained and the error says so.
#[allow(clippy::too_many_arguments)]
async fn apply(
    root: &ModelRoot,
    target: &ModelTarget,
    work: &PullWork<'_>,
    archives: &[(String, PathBuf, Option<String>)],
    to_activate: &[String],
    license_evidence: &std::collections::BTreeMap<
        String,
        crate::registry::receipt::LicenseEvidence,
    >,
    timeout: std::time::Duration,
    journal: &mut ApplyJournal,
) -> Result<()> {
    let engine = model::engine_listen(target);
    let engine = engine.as_deref();

    // 1–2. Stage everything, and bind identity to the staged descriptor.
    let mut staged = Vec::new();
    let stage_result = (|| -> Result<()> {
        for item in &work.items {
            let model = item.entry.model;
            let (_, part_path, source_host) = archives
                .iter()
                .find(|(name, _, _)| name == &model.name)
                .ok_or_else(|| {
                    CliError::internal(format!("{}: archive missing after download", model.name))
                })?;
            let ready = crate::registry::stage_from_archive(
                root,
                model,
                part_path,
                source_host.clone(),
                license_evidence.get(&model.name),
                item.enabled_after,
            )?;
            let _ = std::fs::remove_file(part_path);
            for warning in &ready.warnings {
                journal.record(format!("warning: {warning}"));
            }
            if let Action::Upgrade { .. } = item.action {
                check_staged_identity(root, model, &ready.identity)?;
            }
            staged.push(ready);
        }
        Ok(())
    })();
    if let Err(e) = stage_result {
        for ready in &staged {
            let _ = std::fs::remove_dir_all(&ready.path);
        }
        return Err(e);
    }

    // The record covers the **whole** batch, not just its replacements. A
    // fresh dependency that is installed and then made resident by a load that
    // crashes is indistinguishable, to the next process, from one that never
    // happened — unless it was written down first. Recovery uses the roles to
    // decide what to restore and what to remove.
    let batch: Vec<crate::registry::root::SwapModel> = work
        .items
        .iter()
        .map(|item| crate::registry::root::SwapModel {
            name: item.entry.model.name.clone(),
            backend: item.entry.model.backend.clone(),
            role: if item.is_upgrade() {
                crate::registry::root::SwapRole::Replace
            } else {
                crate::registry::root::SwapRole::Install
            },
            // Only an enabled replacement has a predecessor the engine held.
            // Asking it to reload a deactivated one would be refused, and the
            // refusal would fail the very proof the rollback exists to make.
            reload_on_rollback: item.is_upgrade() && item.touches_the_engine(),
        })
        .collect();
    // The quiesce set covers every replacement, enabled or not: an unload of
    // something that is not resident answers `not-loaded`, and paying that
    // costs nothing next to assuming a deactivated model cannot be in memory.
    let replaced_names: Vec<String> = work
        .items
        .iter()
        .filter(|item| item.is_upgrade())
        .map(|item| item.entry.model.name.clone())
        .collect();

    // 3. The record exists before anything can become unknown — including for
    //    a fresh-only batch, which would otherwise have no transaction.
    let mut landed = Landed::default();
    if !batch.is_empty() {
        match root.begin_swap(batch) {
            Ok(transaction) => landed.transaction = Some(transaction),
            Err(e) => {
                for ready in &staged {
                    let _ = std::fs::remove_dir_all(&ready.path);
                }
                return Err(e);
            }
        }
    }

    // 4. Quiesce. A failure here leaves the record in place on purpose: the
    //    engine's state is unknown, and the next command settles it.
    if let Err(e) = model::quiesce(engine, &replaced_names, timeout).await {
        for ready in &staged {
            let _ = std::fs::remove_dir_all(&ready.path);
        }
        return Err(retained(&mut landed, e));
    }

    // 5. Move everything into place, dependency-first.
    let mut failure: Option<CliError> = None;
    for (item, ready) in work.items.iter().zip(staged.iter()) {
        let model = item.entry.model;
        let outcome = match &item.action {
            Action::Install => root
                .install_staged(&ready.path, &model.backend, &model.name)
                .map(|path| {
                    landed.installed.push((model.name.clone(), path.clone()));
                    format!("installed {} at {}", model.name, path.display())
                }),
            Action::Upgrade { from_revision } => {
                let transaction = landed.transaction.as_ref().expect("upgrades opened one");
                root.swap_in(transaction, &ready.path, &model.backend, &model.name)
                    .map(|()| {
                        landed.replaced.push(model.name.clone());
                        format!(
                            "{}: revision {from_revision} → {}, replaced in place",
                            model.name,
                            model.revision()
                        )
                    })
            }
        };
        match outcome {
            Ok(message) => journal.record(message),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&ready.path);
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failure {
        return Err(undo(root, engine, &mut landed, timeout, e).await);
    }
    // Staging trees that were never consumed (the loop stopped early) are
    // swept by the next lock holder; the consumed ones no longer exist.

    // 6. Load, verified exactly.
    if let Some(listen) = engine {
        let expected: Vec<(String, &'static [&'static str])> = to_activate
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    if landed.replaced.contains(name) {
                        // After a confirmed unload, `already-loaded` would
                        // mean the engine still holds the previous bytes.
                        admin::LOADED_FRESH
                    } else {
                        admin::LOADED_ANY
                    },
                )
            })
            .collect();
        if !expected.is_empty() {
            let loaded = match admin::load(listen, to_activate, timeout).await {
                Ok(outcomes) => admin::verify_outcomes(&expected, &outcomes)
                    .map(|()| outcomes)
                    .map_err(|problem| {
                        CliError::apply(format!("the engine did not load cleanly: {problem}"))
                            .with_fix("check the engine log")
                    }),
                // A lost answer is an unknown partial application: the engine
                // may have made any of them resident.
                Err(e) => Err(CliError::apply(format!(
                    "the embedded engine did not accept the load: {e}"
                ))),
            };
            match loaded {
                Ok(outcomes) => {
                    for outcome in &outcomes {
                        journal.record(format!("{}: {}", outcome.model, outcome.status));
                    }
                }
                Err(e) => return Err(undo(root, engine, &mut landed, timeout, e).await),
            }
        }
    }

    // 7. Proven. Only now may the predecessors go.
    if let Some(transaction) = &landed.transaction {
        root.confirm_swap(transaction)?;
    }
    for name in landed
        .replaced
        .iter()
        .chain(landed.installed.iter().map(|(name, _)| name))
    {
        journal.succeeded(name.clone());
    }
    Ok(())
}

/// The client's independent identity boundary: the
/// **staged descriptor** — the bytes the engine will consume — must declare
/// exactly the identity the install recorded. Preflight already compared the
/// receipt with the index; this compares it with what actually arrived, which
/// is what a registry cannot forge by keeping its catalogue entry stable.
fn check_staged_identity(root: &ModelRoot, model: &IndexModel, staged: &Identity) -> Result<()> {
    let installed = root
        .installed()?
        .into_iter()
        .find(|m| m.dir_name == model.name && m.backend == model.backend)
        .and_then(|m| m.receipt);
    let Some(receipt) = installed else {
        return Ok(()); // preflight already refused a replacement without one
    };
    let refuse = |problem: String| {
        CliError::precondition(format!(
            "{}: the archive the registry served declares a different model than the one              installed — {problem}",
            model.name
        ))
        .with_fix(
            "nothing was installed. The catalogue entry and the archive it points at disagree;              report this registry inconsistency",
        )
    };
    match receipt.identity() {
        Some(recorded) => identity::check_identity(&recorded, staged).map_err(refuse),
        None => identity::check_identity_core(&receipt.model_type, &receipt.backend, staged)
            .map_err(refuse),
    }
}

/// A failure that leaves the transaction recorded and untouched, because the
/// engine's state is unknown and settling it would be a guess.
fn retained(landed: &mut Landed, cause: CliError) -> CliError {
    if landed.transaction.take().is_none() {
        return cause;
    }
    CliError::apply(format!(
        "{cause}. The replacement is recorded under models/.swap/ and every copy is retained;          rerun any `postvec model` command to finish recovery"
    ))
    .with_fix(cause.remediation().unwrap_or("resolve the cause and rerun"))
}

/// Roll the whole batch back: quiesce everything this command could have made
/// resident, restore the predecessors, remove fresh installs, reload the
/// complete predecessor set, and clear the record last.
async fn undo(
    root: &ModelRoot,
    engine: Option<&str>,
    landed: &mut Landed,
    timeout: std::time::Duration,
    cause: CliError,
) -> CliError {
    let Some(transaction) = landed.transaction.take() else {
        // Only reachable when the batch was empty, so there is nothing to undo.
        return CliError::apply(format!(
            "{cause}. Nothing had been recorded, so nothing changed"
        ))
        .with_fix(cause.remediation().unwrap_or("resolve the cause and rerun"));
    };

    // Everything that could hold new bytes, in one request: the record already
    // names the whole batch — replacements and fresh installs alike — so no
    // in-process bookkeeping can leave a resident model unquiesced.
    let resident = transaction.names();
    if let Err(e) = model::quiesce(engine, &resident, timeout).await {
        landed.transaction = Some(transaction);
        return CliError::apply(format!(
            "{cause}. The engine could not be quiesced ({e}), so nothing was restored: the              replacement stays recorded under models/.swap/ and every copy is retained"
        ))
        .with_fix("rerun any `postvec model` command once the engine is reachable");
    }
    // One path from here: `restore_and_prove` restores replacements, removes
    // the recorded fresh installs, reloads the predecessors and clears the
    // record. Rollback and crash recovery run exactly the same code, which is
    // why there is only one place for that sequence to be right.
    landed.installed.clear();
    if let Err(e) = model::restore_and_prove(root, engine, &transaction, timeout).await {
        landed.transaction = Some(transaction);
        return CliError::apply(format!("{cause}. {e}"))
            .with_fix(e.remediation().unwrap_or("rerun to finish recovery"));
    }
    CliError::apply(format!(
        "{cause}. The previous revision was restored and reloaded; nothing changed"
    ))
    .with_fix(cause.remediation().unwrap_or("resolve the cause and rerun"))
}

/// The cluster-side compatibility gate, per configured database:
///
/// - `min_postvec_version` fails closed: it is compared against the loaded
///   library version (`postvec.version()`). Catalogue/library skew blocks
///   the pull (it means an unfinished extension upgrade). An unparseable
///   installed version is a refusal, never a silent skip. Malformed
///   minimums cannot reach here: index validation rejects them.
/// - Backend capability: when the extension reports its loadable
///   backends, every closure entry's backend must be among them. An
///   incompatible model is refused before any download. An older
///   extension that does not report backends produces an explicit
///   "unverified" note, not a pass.
async fn check_cluster_compatibility(
    context: &mut Context,
    settings: &crate::facts::SettingsSnapshot,
    items: &[WorkItem<'_>],
    output: &Output,
) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let mut needs: Vec<(&str, semver::Version)> = Vec::new();
    for item in items {
        if let Some(raw) = item.entry.model.min_postvec_version.as_deref() {
            let min = crate::validate::parse_extension_version(raw).ok_or_else(|| {
                CliError::precondition(format!(
                    "{}: min_postvec_version {raw:?} does not parse",
                    item.entry.model.name
                ))
            })?;
            needs.push((item.entry.model.name.as_str(), min));
        }
    }

    let mut capability_verified = false;
    let mut any_extension = false;
    for database in settings.configured_databases() {
        let facts = context.db.inspect_database(&database).await?;
        let Some(extension) = facts.postvec else {
            continue;
        };
        any_extension = true;
        if extension.catalog_version != extension.library_version {
            return Err(CliError::precondition(format!(
                "{database:?} has SQL catalogue version {} but library {}; the extension \
                 upgrade is unfinished, so compatibility gates cannot be trusted",
                extension.catalog_version, extension.library_version
            ))
            .with_fix("run `ALTER EXTENSION postvec UPDATE` there first (install.md §11)"));
        }
        let installed = crate::validate::parse_extension_version(&extension.library_version)
            .ok_or_else(|| {
                CliError::precondition(format!(
                    "{database:?} reports postvec library version {:?}, which does not parse; \
                     refusing to guess compatibility",
                    extension.library_version
                ))
                .with_fix("reinstall or upgrade the extension, then rerun")
            })?;
        for (name, min) in &needs {
            if &installed < min {
                return Err(CliError::precondition(format!(
                    "{name} needs postvec >= {min}, but {database:?} has {installed}"
                ))
                .with_fix("upgrade the extension first (install.md §11)"));
            }
        }
        if let Some(backends) = extension
            .build_info
            .as_ref()
            .and_then(|info| info.model_backends.as_ref())
        {
            capability_verified = true;
            for item in items {
                if !backends.iter().any(|b| b == &item.entry.model.backend) {
                    return Err(CliError::precondition(format!(
                        "{}: needs backend {:?}, but the postvec build serving {database:?} \
                         can only load [{}]",
                        item.entry.model.name,
                        item.entry.model.backend,
                        backends.join(", ")
                    ))
                    .with_fix(
                        "install a postvec build with that backend compiled in, or pick a \
                         model this build supports",
                    ));
                }
            }
        }
    }
    if !capability_verified {
        output.note(if any_extension {
            "the installed extension does not report its loadable backends (older build); \
             backend compatibility was NOT verified"
        } else {
            "no configured database has the extension installed; version and backend \
             compatibility were NOT verified"
        });
    }
    Ok(())
}

fn check_disk_space(root: &ModelRoot, wanted: &[&IndexModel]) -> Result<()> {
    let Some(free) = root.free_disk_bytes() else {
        return Ok(()); // unknowable is not a refusal; the download will say
    };
    let needed: u64 = wanted
        .iter()
        .map(|m| m.archive.size + m.archive.installed_size)
        .sum();
    let needed = (needed as f64 * (1.0 + DISK_MARGIN)) as u64;
    if needed > free {
        return Err(CliError::precondition(format!(
            "not enough disk: the run needs about {} (archives + installed + 10%), {} free at {}",
            model::human_bytes(needed),
            model::human_bytes(free),
            root.models_dir().display()
        )));
    }
    Ok(())
}

/// Which of this batch's models the engine must hold when the command
/// finishes, and which an explicit `postvec.embedded_models` list leaves out.
///
/// Only **enabled** models are in either set. A fresh install lands
/// deactivated and is nobody's business until `postvec model activate`; an
/// upgrade of a deactivated model was never resident, and asking the engine to
/// load a disabled descriptor is a refusal, not a load. An upgrade of an
/// *enabled* model is in the set because the swap unloaded it and it must come
/// back.
fn activation_set(target: &ModelTarget, work: &PullWork<'_>) -> (Vec<String>, Vec<String>) {
    let ModelTarget::Embedded { settings, .. } = target else {
        return (Vec::new(), Vec::new());
    };
    let allow_list = settings.embedded_models();
    let mut eligible = Vec::new();
    let mut unlisted = Vec::new();
    for item in work.items.iter().filter(|item| item.touches_the_engine()) {
        let name = item.entry.model.name.clone();
        if allow_list.is_empty() || allow_list.contains(&name) {
            eligible.push(name);
        } else {
            unlisted.push(name);
        }
    }
    (eligible, unlisted)
}

/// Download every archive, ≤4 concurrently, with one aggregate progress
/// readout. Returns `(name, part_path, source_host)` per model.
async fn download_all(
    root: &ModelRoot,
    models: &[&IndexModel],
    bearer: Option<String>,
    timeout: std::time::Duration,
    output: &Output,
) -> Result<Vec<(String, PathBuf, Option<String>)>> {
    root.ensure_staging()?;
    let insecure =
        urls::public_index_url().overridden || urls::authenticated_index_url().overridden;
    let client = Arc::new(RegistryClient::new(timeout, insecure)?);
    let progress = Arc::new(Progress::default());

    let render = spawn_progress_renderer(progress.clone(), output);

    let results: Vec<Result<(String, PathBuf, Option<String>)>> =
        futures_util::stream::iter(models.iter().map(|model| {
            let client = client.clone();
            let progress = progress.clone();
            let bearer = bearer.clone();
            let root = root.clone();
            let model = (*model).clone();
            async move {
                let digest_hex = model
                    .archive
                    .digest_hex()
                    .map_err(CliError::precondition)?
                    .to_string();
                // Part files are keyed by digest, so a half-downloaded old
                // revision can never collide with a new one.
                let part = root.part_path(&digest_hex);
                // Progress registration happens exactly once per model —
                // the download call below may be retried after an
                // auth-expiry refresh and must not re-count anything.
                progress.add_total(model.archive.size);
                let existing = tokio::fs::metadata(&part)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if existing > 0 {
                    progress.add_done(existing.min(model.archive.size));
                }
                match client.download(&model, &part, &progress).await {
                    Ok(host) => Ok((model.name.clone(), part, Some(host))),
                    Err(DownloadError::AuthExpired) => {
                        // Refetch the authenticated index once for fresh
                        // presigned sources, then resume.
                        let Some(key) = bearer.as_deref() else {
                            return Err(CliError::precondition(format!(
                                "{}: the source demands authentication but this run is \
                                 anonymous",
                                model.name
                            ))
                            .with_fix("run `postvec login` first"));
                        };
                        let target = urls::authenticated_index_url();
                        let fresh_index = client.fetch_index(&target.url, Some(key)).await?;
                        let fresh = fresh_index.model(&model.name).ok_or_else(|| {
                            CliError::precondition(format!(
                                "{} disappeared from the authenticated catalogue mid-run",
                                model.name
                            ))
                        })?;
                        if fresh.archive.digest != model.archive.digest {
                            return Err(CliError::precondition(format!(
                                "{}: the registry published a new revision during this \
                                 download; nothing was installed",
                                model.name
                            ))
                            .with_fix("rerun the command"));
                        }
                        match client.download(fresh, &part, &progress).await {
                            Ok(host) => Ok((model.name.clone(), part, Some(host))),
                            Err(DownloadError::AuthExpired) => {
                                Err(CliError::precondition(format!(
                                    "{}: authentication kept failing after a fresh index",
                                    model.name
                                )))
                            }
                            Err(DownloadError::Failed(e)) => Err(e),
                        }
                    }
                    Err(DownloadError::Failed(e)) => Err(e),
                }
            }
        }))
        .buffer_unordered(DOWNLOAD_CONCURRENCY)
        .collect()
        .await;

    render.abort();
    finish_progress_line(&progress, output);

    let mut done = Vec::new();
    for result in results {
        done.push(result?);
    }
    Ok(done)
}

fn spawn_progress_renderer(
    progress: Arc<Progress>,
    output: &Output,
) -> tokio::task::JoinHandle<()> {
    let json = output.is_json();
    let tty = proc::is_stderr_tty();
    let color = output.style.is_enabled() && tty;
    tokio::spawn(async move {
        if json {
            return; // JSON: nothing but the final object (stderr stays quiet too)
        }
        let interval = if tty {
            std::time::Duration::from_millis(200)
        } else {
            std::time::Duration::from_secs(5)
        };
        loop {
            tokio::time::sleep(interval).await;
            let (done, total) = progress.snapshot();
            if total == 0 {
                continue;
            }
            let line = download_progress_line(done, total, proc::stderr_width(), color);
            if tty {
                eprint!("\r\x1b[2K{line}");
            } else {
                eprintln!("{line}");
            }
        }
    })
}

/// One progress line, sized to the terminal. Colour is decorative; the
/// numbers always carry the same information.
fn download_progress_line(done: u64, total: u64, width: usize, color: bool) -> String {
    let percent = if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64 * 100.0).min(100.0)
    };
    let numbers = format!(
        "{} / {} ({percent:.0}%)",
        model::human_bytes(done),
        model::human_bytes(total)
    );
    let prefix = "downloading ";
    let budget = width.saturating_sub(prefix.len() + numbers.len() + 3);
    let bar_w = budget.clamp(10, 28);
    let filled = if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * bar_w as f64).round() as usize
    };
    let filled = filled.min(bar_w);
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
    let bar = if color {
        format!("\x1b[32m{bar}\x1b[0m")
    } else {
        bar
    };
    format!("{prefix}[{bar}] {numbers}")
}

fn finish_progress_line(progress: &Progress, output: &Output) {
    if output.is_json() {
        return;
    }
    let (done, total) = progress.snapshot();
    if total == 0 {
        return;
    }
    if proc::is_stderr_tty() {
        eprint!("\r\x1b[2K");
    }
    output.progress(&format!(
        "downloaded {} of {}",
        model::human_bytes(done.min(total)),
        model::human_bytes(total)
    ));
}

fn finish(
    plan: Plan,
    journal: ApplyJournal,
    mut messages: Vec<String>,
    started: Instant,
    started_at: String,
    next_step: Option<String>,
    target: &ModelTarget,
) -> CommandResult {
    let exit = journal.exit();
    for entry in &journal.applied {
        messages.push(format!("postvec: {entry}"));
    }
    for failed in &journal.failed_databases {
        messages.push(format!("postvec: {} FAILED: {}", failed.name, failed.error));
    }
    for unfinished in &journal.incomplete {
        messages.push(format!("postvec: INCOMPLETE: {unfinished}"));
    }
    CommandResult {
        schema_version: crate::checks::SCHEMA_VERSION,
        command: plan.command,
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: target.label(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        plan,
        applied: journal.applied.clone(),
        messages,
        checks: Vec::new(),
        next_step,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::index::{ArchiveInfo, Index};
    use crate::registry::receipt::{Receipt, ReceiptIdentity};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn root_fixture() -> (tempfile::TempDir, ModelRoot) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        fs::set_permissions(dir.path().join("models"), fs::Permissions::from_mode(0o755)).unwrap();
        let root = ModelRoot::new(dir.path().canonicalize().unwrap());
        (dir, root)
    }

    fn entry(revision: u64, digest_byte: &str) -> IndexModel {
        IndexModel {
            name: "m".into(),
            access: "public".into(),
            model_type: "convert".into(),
            backend: "onnx-runtime".into(),
            quantization: None,
            source_model: Some("space-a".into()),
            target_model: Some("space-b".into()),
            source_dim: Some(384),
            target_dim: Some(1536),
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
            revision: Some(revision),
            required_entitlements: vec![],
            archive: ArchiveInfo {
                digest: format!("sha256:{}", digest_byte.repeat(32)),
                size: 10,
                installed_size: 5,
                sources: vec!["https://example.invalid/a".into()],
            },
        }
    }

    /// Install `model` for real enough that preflight sees a CLI-owned copy.
    fn install(root: &ModelRoot, model: &IndexModel, identity: Option<ReceiptIdentity>) {
        let dir = root.models_dir().join(&model.backend).join(&model.name);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            dir.join("ninference.hub.json"),
            format!(r#"{{"name":"{}","enabled":true}}"#, model.name),
        )
        .unwrap();
        let mut receipt = Receipt::new(model, None, &[], &Identity::of_index(model), None);
        receipt.identity = identity;
        receipt.write(&dir).unwrap();
    }

    fn index_of(model: IndexModel) -> Index {
        Index {
            schema_version: crate::registry::index::SCHEMA_VERSION,
            channel: "public".into(),
            authenticated: false,
            generated_at: None,
            models: vec![model],
            viewer: None,
        }
    }

    async fn run_preflight<'a>(
        mode: Mode,
        root: &ModelRoot,
        index: &'a Index,
    ) -> Result<PullWork<'a>> {
        let closure = crate::registry::index::expand_closure(index, &["m".to_string()]).unwrap();
        preflight(mode, root, &closure, std::time::Duration::from_secs(1)).await
    }

    /// The head moved: `pull` says so and does nothing, because a pull that
    /// replaced bytes would be the background updater this design rejects.
    #[tokio::test]
    async fn pull_reports_an_available_update_and_never_replaces() {
        let (_guard, root) = root_fixture();
        install(
            &root,
            &entry(1, "11"),
            Some(ReceiptIdentity {
                source_model: Some("space-a".into()),
                target_model: Some("space-b".into()),
                source_dim: Some(384),
                target_dim: Some(1536),
            }),
        );
        let index = index_of(entry(2, "22"));
        let work = run_preflight(Mode::Install, &root, &index).await.unwrap();
        assert!(work.items.is_empty());
        assert!(
            work.notes[0].contains("postvec model upgrade m"),
            "{:?}",
            work.notes
        );

        let work = run_preflight(Mode::Upgrade, &root, &index).await.unwrap();
        assert_eq!(work.items.len(), 1);
        assert!(work.items[0].is_upgrade());
    }

    /// Identical revisions must mean identical bytes; anything else is a
    /// publication or integrity fault, not an update.
    #[tokio::test]
    async fn the_same_revision_with_different_bytes_is_an_error() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(3, "11"), None);
        let index = index_of(entry(3, "22"));
        for mode in [Mode::Install, Mode::Upgrade] {
            let err = run_preflight(mode, &root, &index).await.unwrap_err();
            assert!(err.to_string().contains("one of the two is wrong"), "{err}");
        }
    }

    /// There is no downgrade: a stale or rolled-back index cannot walk an
    /// install backwards.
    #[tokio::test]
    async fn an_older_head_is_refused_on_upgrade_and_noted_on_pull() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(4, "11"), None);
        let index = index_of(entry(2, "22"));

        let err = run_preflight(Mode::Upgrade, &root, &index)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("revision 4 is installed"), "{err}");

        let work = run_preflight(Mode::Install, &root, &index).await.unwrap();
        assert!(work.items.is_empty());
        assert!(
            work.notes[0].contains("cached or rolled-back"),
            "{:?}",
            work.notes
        );
    }

    /// The client re-checks identity against data it wrote itself, so a
    /// registry cannot move an installed column's vector space.
    #[tokio::test]
    async fn an_identity_change_is_refused_against_the_local_receipt() {
        let (_guard, root) = root_fixture();
        install(
            &root,
            &entry(1, "11"),
            Some(ReceiptIdentity {
                source_model: Some("space-a".into()),
                target_model: Some("space-b".into()),
                source_dim: Some(384),
                target_dim: Some(1536),
            }),
        );
        let mut moved = entry(2, "22");
        moved.target_model = Some("space-c".into());
        let err = run_preflight(Mode::Upgrade, &root, &index_of(moved))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("target_model"), "{err}");
    }

    /// A receipt written before the identity block is "unknown", not
    /// "agrees": the checkable half is still checked and the gap is reported.
    #[tokio::test]
    async fn a_legacy_receipt_upgrades_but_says_what_it_could_not_verify() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(1, "11"), None);
        let index = index_of(entry(2, "22"));
        let work = run_preflight(Mode::Upgrade, &root, &index).await.unwrap();
        assert_eq!(work.items.len(), 1);
        assert!(
            work.notes
                .iter()
                .any(|n| n.contains("could not be re-checked")),
            "{:?}",
            work.notes
        );

        // …but the fields every receipt has are still exact.
        let (_guard, root) = root_fixture();
        install(&root, &entry(1, "11"), None);
        let mut retyped = entry(2, "22");
        retyped.model_type = "embed".into();
        let err = run_preflight(Mode::Upgrade, &root, &index_of(retyped))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("model_type"), "{err}");
    }

    /// M2: an upgrade work item carries the receipt of the installation
    /// being replaced, so the terms plan can honour an acknowledgement it
    /// already records — and a fresh install carries none.
    #[tokio::test]
    async fn preflight_carries_the_replaced_installations_receipt() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(1, "11"), None);
        let index = index_of(entry(2, "22"));
        let work = run_preflight(Mode::Upgrade, &root, &index).await.unwrap();
        let receipt = work.items[0]
            .installed_receipt
            .as_ref()
            .expect("an upgrade carries the replaced receipt");
        assert_eq!(receipt.name, "m");

        let (_guard, root) = root_fixture();
        let index = index_of(entry(1, "11"));
        let work = run_preflight(Mode::Install, &root, &index).await.unwrap();
        assert!(work.items[0].installed_receipt.is_none());
    }

    /// Upgrading something that is not installed is a usage mistake with an
    /// obvious fix, not an install.
    #[tokio::test]
    async fn upgrading_an_absent_model_names_pull() {
        let (_guard, root) = root_fixture();
        let err = run_preflight(Mode::Upgrade, &root, &index_of(entry(1, "11")))
            .await
            .unwrap_err();
        assert_eq!(
            err.remediation(),
            Some("install it first with `postvec model pull`")
        );
    }

    /// Ownership is never overridable: an upgrade may not replace a directory
    /// the CLI did not create.
    #[tokio::test]
    async fn a_directory_without_a_receipt_is_never_replaced() {
        let (_guard, root) = root_fixture();
        let dir = root.models_dir().join("onnx-runtime/m");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("ninference.hub.json"), r#"{"name":"m"}"#).unwrap();
        let err = run_preflight(Mode::Upgrade, &root, &index_of(entry(2, "22")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("manually managed"), "{err}");
    }

    /// An explicit `postvec.embedded_models` list that leaves a replacement
    /// out makes an online upgrade unprovable, so it is refused before any
    /// mutation. Fresh installs are unaffected — they have no predecessor to
    /// risk — and an empty list means "scan everything".
    #[tokio::test]
    async fn a_replacement_outside_an_explicit_allow_list_is_refused() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(1, "11"), None);
        let index = index_of(entry(2, "22"));
        let work = run_preflight(Mode::Upgrade, &root, &index).await.unwrap();
        assert!(work.items[0].is_upgrade());

        let err = check_replacements_are_loadable(&["other".to_string()], &work).unwrap_err();
        assert!(
            err.to_string().contains("cannot be replaced online"),
            "{err}"
        );
        assert!(err.remediation().unwrap().contains("--path"), "{err}");

        // Listed, or no list at all: allowed.
        check_replacements_are_loadable(&["m".to_string()], &work).unwrap();
        check_replacements_are_loadable(&[], &work).unwrap();

        // A fresh install is never blocked by the list.
        let (_guard, root) = root_fixture();
        let index = index_of(entry(1, "11"));
        let work = run_preflight(Mode::Install, &root, &index).await.unwrap();
        assert!(!work.items[0].is_upgrade());
        check_replacements_are_loadable(&["other".to_string()], &work).unwrap();
    }

    /// An upgrade plan never claims an update is vector-preserving.
    #[tokio::test]
    async fn an_upgrade_plan_makes_no_vector_preservation_claim() {
        let (_guard, root) = root_fixture();
        install(&root, &entry(1, "11"), None);
        let index = index_of(entry(2, "22"));
        let work = run_preflight(Mode::Upgrade, &root, &index).await.unwrap();
        let step = crate::plan::PlanStep::UpgradeModel {
            name: "m".into(),
            backend: "onnx-runtime".into(),
            from_revision: 1,
            to_revision: 2,
            embed: index.models[0].model_type == "embed",
        };
        let described = step.describe();
        assert!(described.contains("NOT rewritten"), "{described}");
        assert!(!described.to_lowercase().contains("preserv"), "{described}");
        assert!(work.items[0].is_upgrade());
    }

    #[test]
    fn download_progress_fits_the_requested_width() {
        let line = download_progress_line(1_200_000_000, 2_300_000_000, 60, false);
        assert!(
            crate::output::display_width(&line) <= 60,
            "{} is {} columns",
            line,
            crate::output::display_width(&line)
        );
        assert!(line.contains("1.2 GB"), "{line}");
        assert!(line.contains("2.3 GB"), "{line}");
        assert!(line.contains("52%") || line.contains("50%"), "{line}");
        assert!(!line.contains('\x1b'), "{line}");
        let colored = download_progress_line(1, 2, 60, true);
        assert!(colored.contains('\x1b'), "{colored}");
    }
}
