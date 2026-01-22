//! `postvec model deactivate`.
//!
//! The inverse of `activate`, and the only other command that changes serving
//! state: take the model out of the running engine, then mark its installed
//! descriptor `enabled: false` so a PostgreSQL restart does not bring it back.
//! Nothing on disk is removed and no stored vector is touched — `model rm` is
//! the command that deletes bytes.
//!
//! Order is **unload first, flip second**. A crash after a proven unload
//! leaves the model out of memory but still enabled on disk, so a restart
//! reloads it and rerunning finishes the job; the reverse order could leave a
//! model marked disabled while still serving. "Deactivate returned an error"
//! should mean "still on".
//!
//! Refusals, in order: not installed; ambiguous across backends;
//! package-owned or manual (never overridable — a package upgrade rewrites the
//! descriptor, so the flip could not be kept); named in an explicit
//! `postvec.embedded_models` allow-list (a disabled explicit root is a startup
//! error, not a skip); depended on by another **enabled** installed model
//! (`--force` overrides after naming the breakage).
//!
//! Columns that lose their embedding route are **not** a refusal. They are a
//! loud acknowledgement: the plan names every affected entry, and `--force` is
//! deliberately not the flag that answers it. "Loses its route" is the
//! resolver's question, not a name match — a column on a convert-only space is
//! served by a converter plus `embed-bridge`, neither of which carries that
//! space's name.

use crate::cli::{Cli, ModelDeactivateArgs};
use crate::commands::model::{
    admin, enabled_dependants, in_use_columns, push_in_use_steps, refresh_databases,
    refuse_if_on_allow_list, require_cli_owned, require_root, resolve_target_for_mutation,
    resolve_unambiguous, ModelTarget,
};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use crate::registry::root::EnabledChange;
use std::time::Instant;

/// The embedded admin endpoint accepts at most this many models per request,
/// and the whole set rides one request so the engine's
/// dependents-before-dependencies ordering sees every name.
const MAX_CLUSTER_DEACTIVATIONS: usize = 32;

pub async fn run(cli: &Cli, args: ModelDeactivateArgs, output: &Output) -> Result<Exit> {
    let names = args.validated()?;
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    let mut target = resolve_target_for_mutation(cli, args.path.as_deref(), output).await?;
    let root = require_root(&target, "deactivating models")?.clone();
    if matches!(target, ModelTarget::Embedded { .. }) && names.len() > MAX_CLUSTER_DEACTIVATIONS {
        return Err(CliError::usage(format!(
            "at most {MAX_CLUSTER_DEACTIVATIONS} models per deactivate on a running cluster \
             (the whole set must unload in one engine request for correct ordering)"
        )));
    }

    let _shared = if args.dry_run {
        root.check_mutable()?;
        root.lock_shared()?
    } else {
        None
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

    // Preflight every requested name before touching anything.
    let mut selected = Vec::new();
    for name in &names {
        let model = resolve_unambiguous(&inventory, name, &root.models_dir())?;
        require_cli_owned(model, "deactivate", cli.timeout).await?;
        refuse_if_on_allow_list(&allow_list, name, "deactivating")?;

        let dependants = enabled_dependants(&inventory, name, &names);
        if !dependants.is_empty() && !args.force {
            return Err(CliError::precondition(format!(
                "{name} is required by enabled model(s) {}; deactivating it would leave them \
                 loaded but unable to serve",
                dependants.join(", ")
            ))
            .with_fix(
                "deactivate the dependants first, or pass --force to break them knowingly \
                 (they stay enabled; keeping them usable is then yours to arrange)",
            ));
        }
        if !dependants.is_empty() {
            output.progress(&format!(
                "--force: deactivating {name} breaks {}",
                dependants.join(", ")
            ));
        }
        if !model.enabled {
            output.note(&format!("{name} is already deactivated"));
            continue;
        }
        selected.push(model.dir_name.clone());
    }

    // Which columns lose their embedding route? A `--path` target has no
    // cluster in scope, so this is genuinely not applicable rather than
    // unknown — the same position `model rm --path` has always taken.
    let scanned = matches!(target, ModelTarget::Embedded { .. });
    let (columns, unknown_databases) = if scanned {
        in_use_columns(&mut target, &selected, &inventory).await
    } else {
        (Vec::new(), Vec::new())
    };

    let mut plan = Plan::new("model deactivate", target.label());
    push_in_use_steps(&mut plan, &selected, &columns, &unknown_databases);
    for name in &selected {
        if scanned {
            plan.push(PlanStep::UnloadModel { name: name.clone() });
        }
        plan.push(PlanStep::SetModelEnabled {
            name: name.clone(),
            enabled: false,
            dependency_of: None,
        });
    }
    if let ModelTarget::Embedded { settings, .. } = &target {
        if !plan.is_noop() {
            for database in settings.configured_databases() {
                plan.push(PlanStep::RefreshModelCache { database });
            }
        }
    }
    if matches!(target, ModelTarget::Path(_)) && !selected.is_empty() {
        output.note(
            "--path: no cluster is in scope, so columns still declaring these models were not \
             checked, and nothing was unloaded — stop the serving process, or run this against \
             the cluster, if either matters",
        );
    }
    output.show_plan(&plan);

    if args.dry_run {
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            vec!["--dry-run: nothing was changed".to_string()],
            started,
            started_at,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    if plan.is_noop() {
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            vec!["every requested model is already deactivated".to_string()],
            started,
            started_at,
        );
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
             check, so no column was inspected"
        });
    }
    // The in-use gate runs before the ordinary confirmation, and before any
    // mutation: `--yes` answers "this is the change I meant", never "I accept
    // that these columns stop working".
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

    // Phase 1 — one batched unload, verified, before any descriptor changes.
    if let ModelTarget::Embedded { settings, .. } = &target {
        let listen = settings.embedded_http_listen();
        let outcomes = admin::unload(&listen, &selected, cli.timeout)
            .await
            .map_err(|e| {
                CliError::precondition(format!(
                    "the embedded engine at {listen} is unreachable for unload ({e}); nothing \
                     was changed, so the models are still active"
                ))
                .with_fix(
                    "wait for the engine to finish starting and rerun; or stop the cluster and \
                     rerun with --path <engine root> to flip the descriptors offline",
                )
            })?;
        let expected: Vec<(String, &'static [&'static str])> = selected
            .iter()
            .map(|name| (name.clone(), admin::UNLOADED))
            .collect();
        admin::verify_outcomes(&expected, &outcomes).map_err(|problem| {
            CliError::precondition(format!(
                "unload did not complete cleanly: {problem}; nothing was changed on disk"
            ))
            .with_fix("resolve the engine-side error (server log) and rerun")
        })?;
        for outcome in &outcomes {
            journal.record(format!("{}: {}", outcome.model, outcome.status));
        }
    }

    // Phase 2 — the persistent flip.
    for name in &selected {
        let model = resolve_unambiguous(&inventory, name, &root.models_dir())?;
        match root.set_enabled(model, false) {
            Ok(EnabledChange::Changed) => {
                journal.record(format!("{name}: disabled on disk (survives a restart)"));
                journal.succeeded(name.clone());
            }
            Ok(EnabledChange::Unchanged) => journal.succeeded(name.clone()),
            Err(e) => {
                journal.incomplete(format!(
                    "{name} was unloaded but its descriptor still says enabled ({e}); a restart \
                     would bring it back — rerun `postvec model deactivate {name}`"
                ));
                journal.failed(name.clone(), &e);
            }
        }
    }

    // Phase 3 — SQL caches, so the model stops resolving for new work.
    refresh_databases(&mut target, &mut journal).await;

    let result = finish(&target, plan, journal, Vec::new(), started, started_at);
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
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
    use crate::commands::model::{enabled_dependants, refuse_if_on_allow_list};
    use crate::plan::InUseColumn;
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

    fn column(model: &str, relation: &str) -> InUseColumn {
        InUseColumn {
            model: model.to_string(),
            declared_model: model.to_string(),
            database: "app".to_string(),
            relation: relation.to_string(),
            column: "body".to_string(),
            state: "active".to_string(),
        }
    }

    /// A column served *through* a model it does not name must say so, or the
    /// warning reads as a false positive and gets acknowledged without thought.
    #[test]
    fn a_bridged_column_explains_why_it_is_affected() {
        let bridged = InUseColumn {
            model: "convert-bge-to-ada".to_string(),
            declared_model: "openai-text-embedding-ada-002".to_string(),
            ..column("convert-bge-to-ada", "public.docs")
        };
        let described = bridged.describe();
        assert!(
            described.contains("public.docs.body (active)"),
            "{described}"
        );
        assert!(
            described.contains("declared on openai-text-embedding-ada-002"),
            "{described}"
        );
        assert!(described.contains("served through it"), "{described}");

        // A column that names the model directly stays terse.
        let direct = column("baai-bge-m3", "public.notes").describe();
        assert_eq!(direct, "app: public.notes.body (active)");
    }

    /// Only an **enabled** dependant blocks: a deactivated one is not going to
    /// be loaded, so nothing breaks. Names in the same batch are exempt.
    #[test]
    fn only_enabled_dependants_block_a_deactivation() {
        let inventory = vec![
            model("embed-dep", true, &[]),
            model("live-converter", true, &["embed-dep"]),
            model("off-converter", false, &["embed-dep"]),
        ];
        assert_eq!(
            enabled_dependants(&inventory, "embed-dep", &[]),
            ["live-converter"]
        );
        assert!(
            enabled_dependants(&inventory, "embed-dep", &["live-converter".to_string()]).is_empty()
        );
    }

    /// An explicitly preloaded model must leave the allow-list first: the
    /// engine's startup preflight refuses a disabled explicit root and fails
    /// the whole initialization, so this would be a cluster that will not come
    /// back.
    #[test]
    fn an_allow_listed_model_is_refused_with_the_exact_setup_change() {
        let allow_list = vec!["keep".to_string(), "drop".to_string()];
        assert!(refuse_if_on_allow_list(&allow_list, "other", "deactivating").is_ok());
        let err = refuse_if_on_allow_list(&allow_list, "drop", "deactivating").unwrap_err();
        assert!(err.to_string().contains("postvec.embedded_models"), "{err}");
        assert!(err.remediation().unwrap().contains("--model keep"), "{err}");
    }

    /// The warning names every affected entry and states exactly what breaks,
    /// and an unreadable database is reported as unknown rather than omitted.
    #[test]
    fn the_in_use_warning_names_the_columns_and_the_consequence() {
        let mut plan = Plan::new("model deactivate", "18/main");
        push_in_use_steps(
            &mut plan,
            &["m".to_string(), "other".to_string()],
            &[column("m", "public.docs"), column("m", "public.notes")],
            &["analytics".to_string()],
        );
        let described = plan.describe().join("\n");
        assert!(
            described.contains("WARNING: m is the embedding route for these columns:"),
            "{described}"
        );
        assert!(
            described.contains("app: public.docs.body (active)"),
            "{described}"
        );
        assert!(
            described.contains("app: public.notes.body (active)"),
            "{described}"
        );
        assert!(
            described.contains("search(), embed() and the worker will fail"),
            "{described}"
        );
        assert!(
            described.contains("Stored vectors are not touched"),
            "{described}"
        );
        // The unreadable database is named for both models, never assumed clean.
        assert!(
            described.contains("analytics: could not be inspected"),
            "{described}"
        );
        assert_eq!(plan.in_use_models(), ["m", "other"]);
    }

    /// A model nothing declares, on a fully readable cluster, adds no step at
    /// all — the acknowledgement exists for real breakage only.
    #[test]
    fn an_unused_model_needs_no_acknowledgement() {
        let mut plan = Plan::new("model deactivate", "18/main");
        push_in_use_steps(&mut plan, &["m".to_string()], &[], &[]);
        assert!(plan.in_use_models().is_empty());
        assert!(plan.is_noop());
    }
}
