//! `postvec model set-space ROUTE SPACE`: relabel the space a provider route
//! serves. A local route's space is the registry's decision.

use crate::cli::{Cli, ModelSetSpaceArgs};
use crate::commands::provider::{
    columns_bound_to, lock_provider_dir, reload_host, resolve_target, rewrite_entries,
    space_routes, EntryEdit, ProviderFileDoc, Scan,
};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};

pub async fn run(cli: &Cli, args: ModelSetSpaceArgs, output: &Output) -> Result<Exit> {
    providers::catalog::validate_public_name(&args.space)
        .map_err(|e| CliError::usage(format!("SPACE: {e}")))?;
    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let dir = target.dir().to_path_buf();
    let _lock = (!args.dry_run)
        .then(|| lock_provider_dir(&dir, target.owner()))
        .transpose()?;

    let mut found = None;
    for path in
        crate::commands::provider::ls::provider_files(&dir).map_err(CliError::precondition)?
    {
        let Some(doc) = ProviderFileDoc::load(&path)? else {
            continue;
        };
        if let Some(d) = doc.descriptors().into_iter().find(|d| d.name == args.route) {
            if d.kind != providers::config::ModelKind::Embed {
                return Err(CliError::usage(format!(
                    "{:?} is a converter; converters connect spaces and have none of their own",
                    args.route
                )));
            }
            let provider = doc.provider_type().unwrap_or_default().to_string();
            found = Some((path, d.space_name().to_string(), d.dim, provider));
        }
    }
    let Some((path, old_space, dim, provider)) = found else {
        return Err(CliError::precondition(format!(
            "{:?} is not a provider route; a local route's space is the registry's decision",
            args.route
        ))
        .with_fix("name a [[models]] entry from `postvec provider ls`"));
    };
    if old_space == args.space {
        output.note(&format!(
            "{} already serves space {:?}",
            args.route, args.space
        ));
        return Ok(Exit::Success);
    }
    // A space has one width: refuse on disagreement with any route of the
    // new space, in the files or served by the host (local models included).
    let mut widths: Vec<(String, u32)> = space_routes(&dir, &args.space)?
        .into_iter()
        .filter_map(|(p, name, _)| {
            let d = ProviderFileDoc::load(&p).ok().flatten()?;
            let dim = d.descriptors().into_iter().find(|d| d.name == name)?.dim;
            Some((name, dim))
        })
        .collect();
    if let Some(listen) = target.embedded_listen() {
        if let Some(inv) =
            crate::commands::model::admin::loaded_inventory(&listen, cli.timeout).await
        {
            widths.extend(inv.models.iter().filter_map(|m| {
                (m.enabled && m.space.as_deref() == Some(&args.space))
                    .then(|| Some((m.name.clone(), m.target_dim?)))
                    .flatten()
            }));
        }
    }
    if let Some((other, other_dim)) = widths.iter().find(|(_, d)| *d != dim) {
        return Err(CliError::precondition(format!(
            "route {:?} is dim {dim} but space {:?} is served by {other:?} at dim {other_dim}; \
             that is a different model, not a label",
            args.route, args.space
        )));
    }

    output.progress(&format!(
        "{}: space {old_space:?} -> {:?}",
        args.route, args.space
    ));
    let mut plan = Plan::new("model set-space", target.label());
    plan.push(PlanStep::WriteConfig {
        path: path.clone(),
        before_sha256: Some("existing".into()),
        after_sha256: "space".into(),
    });
    let route = [(args.route.clone(), args.space.clone())];
    let (lost, gained, unknown) = if target.files_only().is_some() {
        (
            Vec::new(),
            Vec::new(),
            vec!["databases served by this host (files only)".into()],
        )
    } else {
        let (lost, mut unknown) =
            columns_bound_to(&mut target, &route, Scan::Changes, cli.timeout).await;
        let (gained, more_unknown) = columns_bound_to(
            &mut target,
            &route,
            Scan::Gains { prefer: true },
            cli.timeout,
        )
        .await;
        unknown.extend(more_unknown);
        unknown.sort();
        unknown.dedup();
        (lost, gained, unknown)
    };
    if !lost.is_empty() || !unknown.is_empty() {
        plan.push(PlanStep::AcknowledgeInUse {
            model: args.route.clone(),
            columns: lost,
            unknown_databases: unknown.clone(),
        });
    }
    if !gained.is_empty() {
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider,
            model: args.route.clone(),
            columns: gained,
            unknown_databases: Vec::new(),
        });
    }
    output.show_plan(&plan);
    output.note(&format!(
        "columns bound to {:?} by name keep it and now count as space {:?}; columns bound to \
         {old_space:?} that it served can be re-attributed with postvec.disable(..., \
         drop_column => false) then postvec.adopt(..., model => {:?})",
        args.route, args.space, args.space
    ));
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
        plan::interactive_route_change_acknowledgement,
        "columns served by {models} lose or change their route; pass --acknowledge-in-use \
         with --yes to accept that",
    )?;
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;
    let edit = if args.space == args.route {
        EntryEdit::Remove { key: "space" }
    } else {
        EntryEdit::Set {
            key: "space",
            value: toml::Value::String(args.space.clone()),
        }
    };
    rewrite_entries(
        &dir,
        vec![(path, args.route.clone(), vec![edit])],
        target.owner(),
    )?;
    let mut journal = ApplyJournal::default();
    reload_host(&target, cli.timeout, &mut journal).await;
    for line in journal.applied.iter().chain(&journal.incomplete) {
        output.progress(line);
    }
    Ok(Exit::Success)
}
