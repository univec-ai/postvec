//! `postvec model prefer SPACE ROUTE...`: explicit priorities for the
//! provider routes of one space (1, 2, ... in the order given); every other
//! provider route of the space returns to the default order behind local.

use crate::cli::{Cli, ModelPreferArgs};
use crate::commands::provider::{
    columns_bound_to, lock_provider_dir, reload_host, resolve_target, rewrite_entries,
    space_routes, EntryEdit, ProviderFileDoc, Scan,
};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};

pub async fn run(cli: &Cli, args: ModelPreferArgs, output: &Output) -> Result<Exit> {
    providers::catalog::validate_public_name(&args.space)
        .map_err(|e| CliError::usage(format!("SPACE: {e}")))?;
    if args.default != args.routes.is_empty() {
        return Err(CliError::usage(
            "name the routes in preferred order, or pass --default alone to drop every \
             explicit priority",
        ));
    }
    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let dir = target.dir().to_path_buf();
    let _lock = (!args.dry_run)
        .then(|| lock_provider_dir(&dir, target.owner()))
        .transpose()?;
    let known = space_routes(&dir, &args.space)?;
    if known.is_empty() {
        return Err(CliError::precondition(format!(
            "no provider route serves space {:?} in {}",
            args.space,
            dir.display()
        ))
        .with_fix("a local model's priority is fixed at 100 and cannot be listed"));
    }

    let mut listed: Vec<String> = Vec::new();
    for route in &args.routes {
        if known.iter().any(|(_, d)| d.name == *route) {
            if !listed.contains(route) {
                listed.push(route.clone());
            }
        } else {
            output.note(&format!(
                "unknown route {route:?} (no provider file declares it in space {:?}); skipping",
                args.space
            ));
        }
    }
    if !args.default && listed.is_empty() {
        return Err(CliError::precondition(
            "none of the named routes is a provider route of that space",
        )
        .with_fix("run `postvec provider ls` and name entries by their public name"));
    }

    let mut edits = Vec::new();
    for (path, d) in &known {
        let (name, priority) = (&d.name, d.priority);
        let want = listed.iter().position(|r| r == name).map(|i| i as u32 + 1);
        if priority == want {
            continue;
        }
        let edit = match want {
            Some(p) => EntryEdit::Set {
                key: "priority",
                value: toml::Value::Integer(p as i64),
            },
            None => EntryEdit::Remove { key: "priority" },
        };
        edits.push((path.clone(), name.clone(), vec![edit]));
    }
    output.progress(&args.space);
    for (path, d) in &known {
        let (name, priority) = (&d.name, d.priority);
        let want = listed.iter().position(|r| r == name).map(|i| i as u32 + 1);
        output.progress(&format!(
            "  {name:<44} {}{}",
            match want {
                Some(p) => format!("priority {p}"),
                None => "default".to_string(),
            },
            match (priority, want) {
                (a, b) if a == b => String::new(),
                (Some(p), _) => format!(" (was {p}, {})", stem(path)),
                (None, _) => format!(" (was default, {})", stem(path)),
            }
        ));
    }

    if edits.is_empty() {
        output.note("priorities are already configured; nothing changed");
        return Ok(Exit::Success);
    }

    let mut plan = Plan::new("model prefer", target.label());
    let mut files: Vec<_> = edits.iter().map(|(p, ..)| p.clone()).collect();
    files.dedup();
    for path in files {
        plan.push(PlanStep::WriteConfig {
            path,
            before_sha256: Some("existing".into()),
            after_sha256: "priority".into(),
        });
    }
    // The route that becomes preferred is where the space's columns embed
    // from the next refresh on: a provider, so a privacy step.
    if let Some(first) = listed.first() {
        let (columns, unknown) = if target.files_only().is_some() {
            (
                Vec::new(),
                vec!["databases served by this host (files only)".into()],
            )
        } else {
            let routes = [(first.clone(), args.space.clone())];
            columns_bound_to(
                &mut target,
                &routes,
                Scan::Gains { prefer: true },
                cli.timeout,
            )
            .await
        };
        if !columns.is_empty() || !unknown.is_empty() {
            let provider = known
                .iter()
                .find(|(_, d)| d.name == *first)
                .and_then(|(p, _)| ProviderFileDoc::load(p).ok().flatten())
                .and_then(|d| d.provider_type().map(str::to_string))
                .unwrap_or_default();
            plan.push(PlanStep::AcknowledgeProviderPrivacy {
                provider,
                model: first.clone(),
                columns,
                unknown_databases: unknown,
            });
        }
    }
    if args.default {
        let routes: Vec<_> = known
            .iter()
            .map(|(_, d)| (d.name.clone(), args.space.clone()))
            .collect();
        let (columns, unknown) = if target.files_only().is_some() {
            (
                Vec::new(),
                vec!["databases served by this host (files only)".into()],
            )
        } else {
            columns_bound_to(&mut target, &routes, Scan::Changes, cli.timeout).await
        };
        if !columns.is_empty() || !unknown.is_empty() {
            plan.push(PlanStep::AcknowledgeProviderPrivacy {
                provider: "configured providers".into(),
                model: args.space.clone(),
                columns,
                unknown_databases: unknown,
            });
        }
    }
    output.show_plan(&plan);
    if args.dry_run {
        output.note("--dry-run: nothing was changed");
        return Ok(Exit::Success);
    }
    plan::confirm_in_use_with(
        &plan,
        args.acknowledge_in_use,
        args.yes,
        args.dry_run,
        Prompt::from_environment(),
        plan::interactive_provider_privacy_acknowledgement,
        "columns in {models}'s space start embedding through that route at the next worker \
         cycle; pass --acknowledge-in-use with --yes to accept that",
    )?;
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;
    rewrite_entries(&dir, edits, target.owner())?;
    let mut journal = ApplyJournal::default();
    reload_host(&target, cli.timeout, &mut journal).await;
    for line in journal.applied.iter().chain(&journal.incomplete) {
        output.progress(line);
    }
    Ok(Exit::Success)
}

fn stem(path: &std::path::Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}
