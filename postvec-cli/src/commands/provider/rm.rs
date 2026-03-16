//! `postvec provider rm` — remove a provider file, or one model entry.
//!
//! Removing a provider takes an embedding route away from every column
//! bound to its public names (unless a local model also serves the name),
//! so the same in-use acknowledgement the `model` family uses gates it:
//! the plan names every affected column, and `--yes` never answers it.

use super::{
    columns_bound_to, reload_host, resolve_target, validate_provider_name, ProviderFileDoc,
    ProviderTarget,
};
use crate::cli::{Cli, ProviderRmArgs};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use std::time::Instant;

pub async fn run(cli: &Cli, args: ProviderRmArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    // Before any host access: this name becomes the path this command
    // deletes.
    validate_provider_name(&args.name, "NAME")?;
    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    // Same read-modify-write, same lock: a concurrent `add` must not have its
    // append removed by this command's rewrite, or vice versa.
    let _lock = (!args.dry_run)
        .then(|| super::lock_provider_dir(target.dir(), target.owner()))
        .transpose()?;
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

    // What the host would serve after this change, against what it serves
    // now. Removing a file does not only take a route *away*: if the file
    // was one claimant of a contested name, the host refused every claimant
    // as a unit — and removing this one hands that name, plus every other
    // model in the surviving file, to a provider that was not serving a
    // moment ago. Those columns' source text starts going somewhere new,
    // which is the event `provider add`'s privacy gate exists for. The
    // in-use acknowledgement ("these columns lose their route") is the wrong
    // question for them; they get the recipient one instead.
    // "Now" is evaluated from the document already loaded through the
    // bounded, `O_NOFOLLOW` reader — not from a second, unguarded read of the
    // same path, which would be the one read in this command that a planted
    // symlink or an oversized file could reach.
    let served_now =
        providers::config::served_names_if(target.dir(), &file_path, Some(&doc.body()?))
            .map_err(CliError::precondition)?;
    let prospective_body = if remove_file {
        None
    } else {
        let mut after = ProviderFileDoc {
            path: doc.path.clone(),
            value: doc.value.clone(),
        };
        after.remove_model(args.model.as_deref().expect("partial removal has --model"));
        Some(after.body()?)
    };
    let served_after =
        providers::config::served_names_if(target.dir(), &file_path, prospective_body.as_deref())
            .map_err(CliError::precondition)?;
    // name -> the provider file stem that takes it over.
    let activated: Vec<(String, String)> = served_after
        .iter()
        .filter(|(name, _)| !served_now.contains_key(*name))
        .map(|(name, stem)| (name.clone(), stem.clone()))
        .collect();
    // What actually *loses* a route: names served now and not afterwards.
    // This is not "the names in the file minus the activated ones" — a name
    // the file declares but the host was not serving (the file was contested,
    // or the directory was over a ceiling) loses nothing when it goes, and
    // saying its columns "lose their route" would be asking for an
    // acknowledgement of an event that is not happening.
    let truly_going_away: Vec<String> = served_now
        .keys()
        .filter(|name| !served_after.contains_key(*name))
        .cloned()
        .collect();
    // `going_away` still drives the journal: it is what this file declared.

    let scanned = matches!(target, ProviderTarget::Embedded { .. });
    let (columns, unknown_databases) = if scanned {
        columns_bound_to(&mut target, &truly_going_away, cli.timeout).await
    } else {
        (Vec::new(), Vec::new())
    };
    let activated_names: Vec<String> = activated.iter().map(|(n, _)| n.clone()).collect();
    let (activated_columns, activated_unknown) = if scanned && !activated_names.is_empty() {
        columns_bound_to(&mut target, &activated_names, cli.timeout).await
    } else {
        (Vec::new(), Vec::new())
    };

    let mut plan = Plan::new("provider rm", target.label());
    crate::commands::model::push_in_use_steps(
        &mut plan,
        &truly_going_away,
        &columns,
        &unknown_databases,
    );
    for (name, stem) in &activated {
        let mine: Vec<crate::plan::InUseColumn> = activated_columns
            .iter()
            .filter(|column| &column.model == name)
            .cloned()
            .collect();
        // `--path` cannot inspect a cluster: UNKNOWN, never empty, for the
        // same reason `provider add` treats it that way.
        let unknown = if !scanned {
            vec!["every database served by this node (not inspectable from --path)".to_string()]
        } else {
            activated_unknown.clone()
        };
        if mine.is_empty() && unknown.is_empty() {
            continue;
        }
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider: stem.clone(),
            model: name.clone(),
            columns: mine,
            unknown_databases: unknown,
        });
    }
    if !activated.is_empty() {
        output.note(&format!(
            "removing this resolves a contested name: {} start(s) being served by {} the \
             moment the host reloads",
            activated
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            activated
                .iter()
                .map(|(_, s)| format!("{s:?}"))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
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
    if activated.is_empty() {
        plan::confirm_in_use(
            &plan,
            args.acknowledge_in_use,
            args.yes,
            args.dry_run,
            Prompt::from_environment(),
            plan::interactive_in_use_acknowledgement,
        )?;
    } else {
        plan::confirm_in_use_with(
            &plan,
            args.acknowledge_in_use,
            args.yes,
            args.dry_run,
            Prompt::from_environment(),
            plan::interactive_provider_privacy_acknowledgement,
            "removing this hands {models} to another provider file, so existing columns bound \
             to those names start sending their source text to a different recipient on the \
             next worker cycle; pass --acknowledge-in-use together with --yes to proceed \
             knowingly. --yes deliberately does not stand in for it",
        )?;
    }
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;

    let mut journal = ApplyJournal::default();
    if remove_file {
        std::fs::remove_file(&file_path)
            .map_err(|e| CliError::apply(format!("cannot remove {}: {e}", file_path.display())))?;
        // Same durability as the write path: a removal this command reports
        // must not come back after a crash, still serving a provider the
        // operator took away.
        super::sync_directory(target.dir(), &file_path)?;
        journal.record(format!("removed {}", file_path.display()));
    } else {
        let id_or_name = args.model.as_deref().expect("partial removal has --model");
        let (removed, remaining) = doc.remove_model(id_or_name);
        debug_assert!(removed, "matched above");
        doc.write(target.owner())?;
        journal.record(format!(
            "removed {id_or_name} from {} ({remaining} model(s) remain)",
            file_path.display()
        ));
    }
    for name in &going_away {
        journal.succeeded(name.clone());
    }

    reload_host(&target, cli.timeout, &mut journal).await;
    // Mirror of `provider add`: without this the removed name stays in
    // `postvec.models` until the worker's next discovery cycle, so a route
    // that no longer exists remains selectable and produces avoidable
    // model-not-found retries and failover attempts.
    super::refresh_databases(&mut target, &mut journal).await;

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
