//! `postvec model rm`.
//!
//! Refusals, in order: not installed; package-owned (never overridable);
//! manual with no receipt; named in an explicit `postvec.embedded_models`
//! allow-list (the operator must update that first, or the next restart
//! fails or silently changes availability); depended on by another enabled
//! installed model (`--force` overrides after naming the breakage).
//!
//! Columns that lose their embedding route are **not** a refusal, and they
//! carry the same acknowledgement `model deactivate` uses: the plan names every
//! affected entry, interactive use types the model names back, and
//! non-interactive use needs `--acknowledge-in-use` alongside `--yes`.
//! `--force` answers a different question (breaking other *models*) and
//! deliberately does not stand in for it.
//!
//! Apply order on a cluster target: one batched unload request for the
//! whole removal set (so the engine's dependents-before-dependencies
//! ordering sees every name), every outcome verified
//! `unloaded`/`not-loaded`, then rename-to-trash for every directory,
//! then the SQL cache refresh, and only then trash deletion. Any unload
//! failure (a transport error, a missing or `error` outcome) aborts with
//! the disk untouched. An Embedded target implies a live postmaster
//! (this command is connected to it), so an unreachable engine means
//! "racing a starting engine", never "cluster is down". Offline removal
//! is `--path`, which performs no unload and tells the operator to stop
//! the serving process first.

use crate::cli::{Cli, ModelRmArgs};
use crate::commands::model::{
    admin, enabled_dependants, in_use_columns, push_in_use_steps, refresh_databases,
    refuse_if_on_allow_list, require_root, resolve_target_for_mutation, resolve_unambiguous,
    ModelTarget,
};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use crate::registry::root::Ownership;
use std::path::PathBuf;
use std::time::Instant;

/// The embedded admin endpoint accepts at most this many models per request,
/// and the whole removal set must ride one request for its unload ordering
/// to be correct — so this is also the per-command ceiling on a cluster
/// target.
const MAX_CLUSTER_REMOVALS: usize = 32;

pub async fn run(cli: &Cli, args: ModelRmArgs, output: &Output) -> Result<Exit> {
    let mut names = Vec::new();
    for raw in &args.names {
        crate::registry::index::valid_model_name(raw).map_err(CliError::usage)?;
        if !names.contains(raw) {
            names.push(raw.clone());
        }
    }
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    let mut target = resolve_target_for_mutation(cli, args.path.as_deref(), output).await?;
    let root = require_root(&target, "removing models")?.clone();
    if matches!(target, ModelTarget::Embedded { .. }) && names.len() > MAX_CLUSTER_REMOVALS {
        return Err(CliError::usage(format!(
            "at most {MAX_CLUSTER_REMOVALS} models per rm on a running cluster (the whole set \
             must unload in one engine request for correct ordering)"
        )));
    }

    // Same preconditions the real run enforces, minus the exclusive lock —
    // see `ModelRoot::check_mutable`.
    let _shared = if args.dry_run {
        root.check_mutable()?;
        root.lock_shared()?
    } else {
        None
    };
    // Shared host lock before the engine-root lock: a `--purge` in progress
    // refuses us, and we hold it off for the duration.
    let _host_lock = if args.dry_run {
        None
    } else {
        crate::config::owned::host_lock_shared()?
    };
    let lock = if args.dry_run {
        None
    } else {
        let lock = root.lock_exclusive()?;
        // An interrupted replacement is settled before this command decides
        // what is installed: removing a model whose predecessor is still
        // parked would otherwise be ambiguous.
        crate::commands::model::recover_pending_swap(&root, &target, cli.timeout, output).await?;
        Some(lock)
    };

    let inventory = root.installed()?;

    // Preflight every requested name before touching anything.
    let mut removals = Vec::new();
    for name in &names {
        let model = resolve_unambiguous(&inventory, name, &root.models_dir())?;
        match model.ownership(cli.timeout).await {
            Ownership::Cli => {}
            Ownership::Package => {
                // Never overridable, not even with --force.
                return Err(CliError::precondition(format!(
                    "{name} is owned by a package; postvec model rm never removes package \
                     content"
                ))
                .with_fix("remove the package with apt/dnf instead"));
            }
            Ownership::Manual => {
                let detail = model
                    .receipt_error
                    .clone()
                    .unwrap_or_else(|| "no .postvec-install.json receipt".to_string());
                return Err(CliError::precondition(format!(
                    "{name} was not installed by this CLI ({detail})"
                ))
                .with_fix(format!(
                    "remove {} manually if that is really wanted",
                    model.path.display()
                )));
            }
        }

        // Reverse dependencies: anything else installed and enabled that
        // needs this name. A deactivated dependant is not going to be loaded,
        // so removing what it needs breaks nothing that was working.
        let dependents = enabled_dependants(&inventory, name, &names);
        if !dependents.is_empty() && !args.force {
            return Err(CliError::precondition(format!(
                "{name} is required by {}; removing it would break them",
                dependents.join(", ")
            ))
            .with_fix("remove the dependents first, or pass --force to break them knowingly"));
        }

        // An explicit preload allow-list naming this model must change first —
        // the same gate `deactivate` applies, for the same reason.
        if let ModelTarget::Embedded { settings, .. } = &target {
            refuse_if_on_allow_list(&settings.embedded_models(), name, "removing")?;
        }
        removals.push((model.dir_name.clone(), model.path.clone(), dependents));
    }

    let removal_names: Vec<String> = removals.iter().map(|(name, _, _)| name.clone()).collect();
    // Which columns lose their embedding route? A `--path` target has no
    // cluster in scope, so this is not applicable rather than unknown.
    let scanned = matches!(target, ModelTarget::Embedded { .. });
    let (in_use, unknown_databases) = if scanned {
        in_use_columns(&mut target, &removal_names, &inventory).await
    } else {
        (Vec::new(), Vec::new())
    };

    let mut plan = Plan::new("model rm", target.label());
    push_in_use_steps(&mut plan, &removal_names, &in_use, &unknown_databases);
    for (name, path, dependents) in &removals {
        if !dependents.is_empty() {
            output.progress(&format!(
                "--force: removing {name} breaks {}",
                dependents.join(", ")
            ));
        }
        plan.push(PlanStep::UnloadModel { name: name.clone() });
        plan.push(PlanStep::RemoveModel {
            name: name.clone(),
            path: path.clone(),
        });
    }
    if let ModelTarget::Embedded { settings, .. } = &target {
        for database in settings.configured_databases() {
            plan.push(PlanStep::RefreshModelCache { database });
        }
    }
    if matches!(target, ModelTarget::Path(_)) {
        output.note(
            "--path: stop any process serving this root first; an unlinked open file keeps \
             serving from its inode",
        );
    }
    output.show_plan(&plan);

    if args.dry_run {
        let result = finish(&target, plan, ApplyJournal::default(), started, started_at);
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    // A flag that acknowledged nothing is reported rather than refused (see
    // `Plan::in_use_models`), and the report says *why* it was unnecessary —
    // "nothing is affected" and "nothing was looked at" are different facts.
    if args.acknowledge_in_use && plan.in_use_models().is_empty() {
        output.note(if scanned {
            "--acknowledge-in-use was not needed: no managed column loses its embedding route \
             to this change"
        } else {
            "--acknowledge-in-use acknowledged nothing: with --path there is no cluster to \
             check, and no column was inspected"
        });
    }
    // The in-use gate runs before the ordinary confirmation, and before any
    // mutation. `--force` covers model-level dependants only.
    plan::confirm_in_use(
        &plan,
        args.acknowledge_in_use,
        args.yes,
        args.dry_run,
        Prompt::from_environment(),
        plan::interactive_in_use_acknowledgement,
    )?;
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;
    let _lock = lock;

    let mut journal = ApplyJournal::default();

    // Phase 1 — one batched unload, verified, before any disk mutation.
    if let ModelTarget::Embedded { settings, .. } = &target {
        let listen = settings.embedded_http_listen();
        let outcomes = admin::unload(&listen, &removal_names, cli.timeout)
            .await
            .map_err(|e| {
                CliError::precondition(format!(
                    "the embedded engine at {listen} is unreachable for unload ({e}); the \
                     cluster is running, so removing files now would leave the engine serving \
                     unlinked models while disk and SQL say they are gone"
                ))
                .with_fix(
                    "wait for the engine to finish starting and rerun; or stop the cluster and \
                     rerun with --path <engine root> for offline removal",
                )
            })?;
        verify_unload_outcomes(&removal_names, &outcomes).map_err(|problem| {
            CliError::precondition(format!(
                "unload did not complete cleanly: {problem}; nothing was removed from disk"
            ))
            .with_fix("resolve the engine-side error (server log) and rerun")
        })?;
        for outcome in &outcomes {
            journal.record(format!("{}: {}", outcome.model, outcome.status));
        }
    }

    // Phase 2 — retire every directory into root-local trash; on any failure
    // restore what was already retired so a partial multi-model removal
    // cannot leave a broken closure.
    let mut retired: Vec<(String, PathBuf, PathBuf)> = Vec::new();
    let mut retire_failure: Option<(String, CliError)> = None;
    for (name, path, _) in &removals {
        match root.retire_to_trash(path) {
            Ok(grave) => retired.push((name.clone(), path.clone(), grave)),
            Err(e) => {
                retire_failure = Some((name.clone(), e.into()));
                break;
            }
        }
    }
    if let Some((failed_name, error)) = retire_failure {
        for (name, original, grave) in retired.iter().rev() {
            match root.restore_retired(grave, original) {
                Ok(()) => journal.record(format!("{name}: restored after batch failure")),
                Err(e) => journal.incomplete(format!(
                    "{name}: could not be restored after batch failure: {e}"
                )),
            }
        }
        journal.failed(failed_name, &error);
    } else {
        // Phase 3 — SQL caches, while the bytes are still recoverable.
        refresh_databases(&mut target, &mut journal).await;
        // Phase 4 — delete the retired bytes.
        for (name, original, grave) in &retired {
            match root.purge_retired(original, grave) {
                Ok(()) => journal.record(format!("removed {name} ({})", original.display())),
                Err(e) => journal.incomplete(e.to_string()),
            }
            journal.succeeded(name.clone());
        }
    }

    let result = finish(&target, plan, journal, started, started_at);
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

/// Every requested name must come back exactly once, as `unloaded` or
/// `not-loaded` — the shared strict verifier in [`admin::verify_outcomes`],
/// which the upgrade path uses too.
fn verify_unload_outcomes(
    requested: &[String],
    outcomes: &[admin::AdminOutcome],
) -> std::result::Result<(), String> {
    let expected: Vec<(String, &'static [&'static str])> = requested
        .iter()
        .map(|name| (name.clone(), admin::UNLOADED))
        .collect();
    admin::verify_outcomes(&expected, outcomes)
}

fn finish(
    target: &ModelTarget,
    plan: Plan,
    journal: ApplyJournal,
    started: Instant,
    started_at: String,
) -> CommandResult {
    let exit = journal.exit();
    let mut messages = Vec::new();
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
        next_step: None,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::model::admin::AdminOutcome;

    fn outcome(model: &str, status: &str, error: Option<&str>) -> AdminOutcome {
        AdminOutcome {
            model: model.to_string(),
            status: status.to_string(),
            error: error.map(|e| e.to_string()),
        }
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn clean_unload_outcomes_pass() {
        let requested = names(&["a", "b"]);
        let outcomes = vec![
            outcome("a", "unloaded", None),
            outcome("b", "not-loaded", None),
        ];
        assert!(verify_unload_outcomes(&requested, &outcomes).is_ok());
    }

    #[test]
    fn an_error_outcome_fails_the_batch() {
        let requested = names(&["a", "b"]);
        let outcomes = vec![
            outcome("a", "unloaded", None),
            outcome("b", "error", Some("session teardown failed")),
        ];
        let err = verify_unload_outcomes(&requested, &outcomes).unwrap_err();
        assert!(err.contains("session teardown failed"), "{err}");
    }

    #[test]
    fn a_missing_result_fails_the_batch() {
        let requested = names(&["a", "b"]);
        let outcomes = vec![outcome("a", "unloaded", None)];
        let err = verify_unload_outcomes(&requested, &outcomes).unwrap_err();
        assert!(err.contains("no result"), "{err}");
    }

    #[test]
    fn duplicate_and_unrequested_results_fail_the_batch() {
        let requested = names(&["a"]);
        let doubled = vec![
            outcome("a", "unloaded", None),
            outcome("a", "unloaded", None),
        ];
        assert!(verify_unload_outcomes(&requested, &doubled)
            .unwrap_err()
            .contains("2 results"));

        let foreign = vec![
            outcome("a", "unloaded", None),
            outcome("x", "unloaded", None),
        ];
        assert!(verify_unload_outcomes(&requested, &foreign)
            .unwrap_err()
            .contains("not requested"));
    }

    #[test]
    fn unknown_statuses_fail_the_batch() {
        let requested = names(&["a"]);
        let outcomes = vec![outcome("a", "loaded", None)];
        assert!(verify_unload_outcomes(&requested, &outcomes).is_err());
    }

    fn installed(name: &str, backend: &str) -> crate::registry::root::InstalledModel {
        crate::registry::root::InstalledModel {
            dir_name: name.to_string(),
            descriptor_name: Some(name.to_string()),
            backend: backend.to_string(),
            enabled: true,
            dependencies: vec![],
            model_type: Some("embed".to_string()),
            source_model: None,
            target_model: None,
            target_dim: Some(384),
            path: std::path::PathBuf::from(format!("/root/models/{backend}/{name}")),
            receipt: None,
            receipt_error: None,
            disk_bytes: 1,
        }
    }

    /// Duplicates across backends refuse with every path named. A unique
    /// name resolves; an absent name errors.
    #[test]
    fn duplicate_backend_names_refuse_removal() {
        let inventory = vec![
            installed("m", "onnx-runtime"),
            installed("m", "candle"),
            installed("other", "onnx-runtime"),
        ];
        let models_dir = std::path::Path::new("/root/models");

        let err = resolve_unambiguous(&inventory, "m", models_dir).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("multiple backends"), "{text}");
        assert!(text.contains("ambiguous"), "{text}");
        assert!(text.contains("/root/models/onnx-runtime/m"), "{text}");
        assert!(text.contains("/root/models/candle/m"), "{text}");

        let unique = resolve_unambiguous(&inventory, "other", models_dir).unwrap();
        assert_eq!(unique.backend, "onnx-runtime");

        assert!(resolve_unambiguous(&inventory, "ghost", models_dir)
            .unwrap_err()
            .to_string()
            .contains("not installed"));
    }
}
