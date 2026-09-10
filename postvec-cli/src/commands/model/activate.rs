//! `postvec model activate`.
//!
//! Turning a model on is two facts that must agree: the installed descriptor
//! says `enabled: true`, so every future engine start loads it, and the
//! running engine has it now. This command establishes both, disk first,
//! engine second, and finishes by refreshing every configured database's SQL
//! model cache.
//!
//! Naming a model activates its deactivated dependency closure with it.
//! The engine refuses a load whose closure contains a deactivated model, and
//! a restart would leave it unloaded too, so enabling only the named root
//! would activate nothing usable.
//!
//! A bare `activate` (or `--all`) activates every eligible CLI-installed
//! model in the root. `--path DIR` is the files-only form used by the
//! air-gapped flow: it flips the descriptors and stops, because there is no
//! engine on that host to talk to.

use crate::cli::{Cli, ModelActivateArgs};
use crate::commands::model::{
    admin, disabled_closure_to_enable, refresh_databases, require_cli_owned, require_root,
    resolve_target_for_mutation, resolve_unambiguous, setup_change_hint, ModelTarget,
};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use crate::registry::root::{EnabledChange, InstalledModel};
use std::collections::BTreeSet;
use std::time::Instant;

pub async fn run(cli: &Cli, args: ModelActivateArgs, output: &Output) -> Result<Exit> {
    let requested = args.validated()?;
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    let mut target = resolve_target_for_mutation(cli, args.path.as_deref(), output).await?;
    let root = require_root(&target, "activating models")?.clone();

    // Same preconditions the real run enforces, minus the exclusive lock.
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
        crate::commands::model::recover_pending_swap(&root, &target, cli.timeout, output).await?;
        Some(lock)
    };

    let inventory = root.installed()?;
    let allow_list = match &target {
        ModelTarget::Embedded { settings, .. } => settings.embedded_models(),
        _ => Vec::new(),
    };

    // What the engine holds right now. Unknown on a `--path` target, and a
    // hard error on a cluster target: activation's whole job is to load, and
    // an engine that cannot be asked cannot be shown to have done it.
    let loaded: Option<BTreeSet<String>> = match &target {
        ModelTarget::Embedded { settings, .. } => {
            match admin::loaded_inventory(&settings.embedded_http_listen(), cli.timeout).await {
                Some(inventory) => Some(inventory.enabled_names()),
                None => {
                    return Err(CliError::precondition(format!(
                        "the embedded engine's listener ({}) is not answering",
                        settings.embedded_http_listen()
                    ))
                    .with_fix(
                        "the engine may still be loading models, or the cluster is stopped; \
                         retry once `postvec doctor` shows the engine up. To mark models \
                         enabled on disk without an engine, use --path <engine root>",
                    ))
                }
            }
        }
        _ => None,
    };

    // Which models this run is about. In scan mode an explicit allow-list
    // narrows the set to what the engine could actually load — but the models
    // it leaves out are reported below rather than silently skipped, because
    // "activate did nothing for my model" with no explanation is the worst
    // possible answer.
    let selection = if requested.is_empty() {
        eligible_models(&inventory, &allow_list)
    } else {
        let mut selection = Vec::new();
        for name in &requested {
            let model = resolve_unambiguous(&inventory, name, &root.models_dir())?;
            require_cli_owned(model, "activate", cli.timeout).await?;
            selection.push(model.dir_name.clone());
        }
        selection
    };

    // Deactivated dependencies come along: the engine will not load a closure
    // containing one, and neither will the next restart.
    let to_enable = disabled_closure_to_enable(&inventory, &selection);
    // Everything the allow-list excludes: models the operator explicitly named
    // (so `selection` carries them) plus, in scan mode, the CLI-owned models
    // `eligible_models` filtered out. Either way the fix is the same
    // `setup --model` change, so say it once.
    let unlisted: Vec<String> = if allow_list.is_empty() {
        Vec::new()
    } else {
        let mut unlisted: Vec<String> = selection
            .iter()
            .filter(|name| !allow_list.contains(name))
            .cloned()
            .collect();
        if requested.is_empty() {
            for model in &inventory {
                if model.receipt.is_some()
                    && !allow_list.contains(&model.dir_name)
                    && !unlisted.contains(&model.dir_name)
                {
                    unlisted.push(model.dir_name.clone());
                }
            }
        }
        unlisted
    };
    // A `--path` target has no engine, so nothing is loaded there and the
    // plan must not claim otherwise. `loaded` is `Some` exactly when there is
    // an engine to talk to; a cluster target that could not be asked already
    // failed above.
    let to_load: Vec<String> = match &loaded {
        None => Vec::new(),
        Some(resident) => selection
            .iter()
            .filter(|name| !unlisted.contains(name))
            .filter(|name| !engine_holds(&inventory, name, resident))
            .cloned()
            .collect(),
    };

    let mut plan = Plan::new("model activate", target.label());
    for name in &to_enable {
        plan.push(PlanStep::SetModelEnabled {
            name: name.clone(),
            enabled: true,
            dependency_of: (!selection.contains(name))
                .then(|| dependant_of(&inventory, name, &selection))
                .flatten(),
        });
    }
    if !to_load.is_empty() {
        plan.push(PlanStep::LoadModels {
            models: to_load.clone(),
        });
    }
    if let ModelTarget::Embedded { settings, .. } = &target {
        if !plan.is_noop() {
            for database in settings.configured_databases() {
                plan.push(PlanStep::RefreshModelCache { database });
            }
        }
    }
    output.show_plan(&plan);

    let mut messages = Vec::new();
    if !unlisted.is_empty() {
        if let ModelTarget::Embedded { settings, .. } = &target {
            messages.push(format!(
                "not eligible (unlisted in postvec.embedded_models): {}; to include them run: {}",
                unlisted.join(", "),
                setup_change_hint(settings, &unlisted)
            ));
        }
    }
    if matches!(target, ModelTarget::Path(_)) && !to_enable.is_empty() {
        output.note(
            "--path: descriptors are marked enabled on disk only; the serving host loads them \
             at its next engine start, or with `postvec model activate` there",
        );
    }

    if args.dry_run {
        messages.push("--dry-run: nothing was changed".to_string());
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            messages,
            started,
            started_at,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    if plan.is_noop() {
        messages.insert(
            0,
            if requested.is_empty() {
                "every eligible model is already active".to_string()
            } else {
                format!("{} is already active", requested.join(", "))
            },
        );
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            messages,
            started,
            started_at,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;
    let _lock = lock;

    let mut journal = ApplyJournal::default();

    // Disk first. A crash after this leaves models enabled but not resident,
    // which `doctor` reports as `models.unactivated` and rerunning repairs.
    // The reverse order would let a load succeed against a descriptor a
    // restart then ignores.
    for name in &to_enable {
        let model = resolve_unambiguous(&inventory, name, &root.models_dir())?;
        match root.set_enabled(model, true)? {
            EnabledChange::Changed => journal.record(format!("{name}: enabled on disk")),
            EnabledChange::Unchanged => {}
        }
    }

    if let ModelTarget::Embedded { settings, .. } = &target {
        if !to_load.is_empty() {
            let outcomes = admin::load(&settings.embedded_http_listen(), &to_load, cli.timeout)
                .await
                .map_err(|e| {
                    CliError::apply(format!(
                        "the models are enabled on disk but the engine did not accept the load: \
                         {e}"
                    ))
                    .with_fix(
                        "rerun `postvec model activate` once the engine is reachable; the \
                         on-disk state already survives a restart",
                    )
                })?;
            for outcome in outcomes {
                if outcome.is_error() {
                    journal.failed(
                        outcome.model.clone(),
                        &CliError::apply(outcome.error.unwrap_or_default()),
                    );
                } else {
                    journal.record(format!("{}: {}", outcome.model, outcome.status));
                    journal.succeeded(outcome.model);
                }
            }
        }
    }
    if !unlisted.is_empty() {
        if let ModelTarget::Embedded { settings, .. } = &target {
            journal.incomplete(format!(
                "unlisted in postvec.embedded_models, so not loaded: {}; to include them run: {}",
                unlisted.join(", "),
                setup_change_hint(settings, &unlisted)
            ));
        }
    }

    refresh_databases(&mut target, &mut journal).await;

    let result = finish(&target, plan, journal, Vec::new(), started, started_at);
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

/// Scan-mode selection: every CLI-installed model the operator could
/// reasonably mean. Package and manual content is theirs to manage, and an
/// explicit allow-list narrows the set to what the engine would load anyway.
fn eligible_models(inventory: &[InstalledModel], allow_list: &[String]) -> Vec<String> {
    inventory
        .iter()
        .filter(|model| model.receipt.is_some())
        .filter(|model| allow_list.is_empty() || allow_list.contains(&model.dir_name))
        .map(|model| model.dir_name.clone())
        .collect()
}

/// Whether the engine already holds this model, under whichever name its
/// descriptor declares.
fn engine_holds(inventory: &[InstalledModel], name: &str, loaded: &BTreeSet<String>) -> bool {
    let engine_name = inventory
        .iter()
        .find(|model| model.dir_name == name)
        .and_then(|model| model.descriptor_name.as_deref())
        .unwrap_or(name);
    loaded.contains(engine_name)
}

/// Which of the requested roots pulled `name` into the closure, for the plan
/// line. The first is enough: the point is that this is not a model the
/// operator named.
fn dependant_of(inventory: &[InstalledModel], name: &str, selection: &[String]) -> Option<String> {
    selection
        .iter()
        .find(|root| {
            inventory.iter().any(|model| {
                &&model.dir_name == root && model.dependencies.iter().any(|d| d == name)
            })
        })
        .cloned()
        .or_else(|| selection.first().cloned())
}

fn finish(
    target: &ModelTarget,
    plan: Plan,
    journal: ApplyJournal,
    mut messages: Vec<String>,
    started: Instant,
    started_at: String,
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
        next_step: None,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::root::InstalledModel;
    use std::path::PathBuf;

    fn model(name: &str, enabled: bool, dependencies: &[&str]) -> InstalledModel {
        InstalledModel {
            dir_name: name.to_string(),
            descriptor_name: Some(name.to_string()),
            backend: "onnx-runtime".to_string(),
            enabled,
            dependencies: dependencies.iter().map(|d| d.to_string()).collect(),
            model_type: Some("embed".to_string()),
            source_model: None,
            target_model: None,
            target_dim: Some(384),
            path: PathBuf::from(format!("/root/models/onnx-runtime/{name}")),
            receipt: None,
            receipt_error: None,
            disk_bytes: 1,
        }
    }

    /// Activating a converter brings its deactivated embed model with it, and
    /// puts the dependency first — the engine refuses a closure containing a
    /// deactivated model, so the other order would enable nothing loadable.
    #[test]
    fn activation_enables_the_deactivated_dependency_closure_depth_first() {
        let inventory = vec![
            model("converter", false, &["embed-dep", "embed-bridge"]),
            model("embed-dep", false, &[]),
            model("embed-bridge", true, &[]),
        ];
        let order = crate::commands::model::disabled_closure_to_enable(
            &inventory,
            &["converter".to_string()],
        );
        assert_eq!(order, ["embed-dep", "converter"]);
        assert_eq!(
            dependant_of(&inventory, "embed-dep", &["converter".to_string()]).as_deref(),
            Some("converter")
        );
    }

    /// Already-enabled models are absent from the flip set: activation is not
    /// a rewrite of every descriptor in the root.
    #[test]
    fn an_already_enabled_closure_needs_no_flips() {
        let inventory = vec![model("a", true, &["b"]), model("b", true, &[])];
        assert!(
            crate::commands::model::disabled_closure_to_enable(&inventory, &["a".to_string()])
                .is_empty()
        );
    }

    /// A dependency cycle terminates rather than recursing forever; the
    /// engine refuses the cycle itself with its own message.
    #[test]
    fn a_dependency_cycle_terminates() {
        let inventory = vec![model("a", false, &["b"]), model("b", false, &["a"])];
        let order =
            crate::commands::model::disabled_closure_to_enable(&inventory, &["a".to_string()]);
        assert_eq!(order.len(), 2, "{order:?}");
    }

    /// Load eligibility follows the descriptor's declared name, which is what
    /// the engine registers a model under.
    #[test]
    fn engine_residency_is_matched_by_descriptor_name() {
        let mut renamed = model("dir-name", true, &[]);
        renamed.descriptor_name = Some("declared-name".to_string());
        let inventory = vec![renamed];
        let loaded: BTreeSet<String> = ["declared-name".to_string()].into();
        assert!(engine_holds(&inventory, "dir-name", &loaded));
        assert!(!engine_holds(&inventory, "other", &loaded));
    }
}
