//! `postvec provider rm` — remove a provider file, or one model entry.
//!
//! Removing a provider takes an embedding route away from every column
//! bound to its public names (unless a local model also serves the name),
//! so the same in-use acknowledgement the `model` family uses gates it:
//! the plan names every affected column, and `--yes` never answers it.

use super::{columns_bound_to, reload_host, resolve_target, ProviderFileDoc, ProviderTarget};
use crate::cli::{Cli, ProviderRmArgs};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use std::time::Instant;

pub async fn run(cli: &Cli, args: ProviderRmArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let file_path = target.dir().join(format!("{}.toml", args.name));
    let Some(mut doc) = ProviderFileDoc::load(&file_path)? else {
        return Err(CliError::precondition(format!(
            "no provider {:?} is configured ({} does not exist)",
            args.name,
            file_path.display()
        )));
    };

    // Which public names go away with this change?
    let all_models = doc.models();
    let (going_away, remove_file): (Vec<String>, bool) = match &args.model {
        Some(id_or_name) => {
            let matched: Vec<String> = all_models
                .iter()
                .filter(|(name, id)| name == id_or_name || id == id_or_name)
                .map(|(name, _)| name.clone())
                .collect();
            if matched.is_empty() {
                return Err(CliError::precondition(format!(
                    "{:?} declares no model {id_or_name:?} (by public name or provider id)",
                    args.name
                )));
            }
            // Removing the last model removes the file: an enabled provider
            // file with no models is a load error, not a state to leave.
            (matched, all_models.len() == 1)
        }
        None => (
            all_models.iter().map(|(name, _)| name.clone()).collect(),
            true,
        ),
    };

    let scanned = matches!(target, ProviderTarget::Embedded { .. });
    let (columns, unknown_databases) = if scanned {
        columns_bound_to(&mut target, &going_away, cli.timeout).await
    } else {
        (Vec::new(), Vec::new())
    };

    let mut plan = Plan::new("provider rm", target.label());
    crate::commands::model::push_in_use_steps(&mut plan, &going_away, &columns, &unknown_databases);
    if remove_file {
        plan.push(PlanStep::RemoveConfig {
            path: file_path.clone(),
        });
    } else {
        plan.push(PlanStep::WriteConfig {
            path: file_path.clone(),
            before_sha256: Some("existing".to_string()),
            after_sha256: "provider file".to_string(),
        });
    }
    if matches!(target, ProviderTarget::Path { .. }) {
        output.note(
            "--path: no cluster is in scope, so columns bound to these names were not checked",
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

    if args.acknowledge_in_use && plan.in_use_models().is_empty() {
        output.note(if scanned {
            "--acknowledge-in-use was not needed: no managed column loses its embedding route \
             to this change"
        } else {
            "--acknowledge-in-use acknowledged nothing: with --path there is no cluster to \
             check, so no column was inspected"
        });
    }
    plan::confirm_in_use(
        &plan,
        args.acknowledge_in_use,
        args.yes,
        args.dry_run,
        Prompt::from_environment(),
        plan::interactive_in_use_acknowledgement,
    )?;
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;

    let mut journal = ApplyJournal::default();
    if remove_file {
        std::fs::remove_file(&file_path)
            .map_err(|e| CliError::apply(format!("cannot remove {}: {e}", file_path.display())))?;
        journal.record(format!("removed {}", file_path.display()));
    } else {
        let id_or_name = args.model.as_deref().expect("partial removal has --model");
        let (removed, remaining) = doc.remove_model(id_or_name);
        debug_assert!(removed, "matched above");
        let owner = match &target {
            ProviderTarget::Embedded { context, .. } => {
                context.as_ref().and_then(|ctx| ctx.cluster.owner.clone())
            }
            ProviderTarget::Path { .. } => None,
        };
        doc.write(owner.as_ref())?;
        journal.record(format!(
            "removed {id_or_name} from {} ({remaining} model(s) remain)",
            file_path.display()
        ));
    }
    for name in &going_away {
        journal.succeeded(name.clone());
    }

    reload_host(&target, cli.timeout, &mut journal).await;

    let result = finish(&target, plan, journal, Vec::new(), started, started_at);
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

fn finish(
    target: &ProviderTarget,
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
