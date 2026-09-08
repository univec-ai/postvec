//! `postvec model set-space ROUTE SPACE`

use crate::cli::{Cli, ModelSetSpaceArgs};
use crate::commands::provider::{
    reload_host, resolve_target, rewrite_entries, EntryEdit, ProviderFileDoc,
};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use providers::catalog;

pub async fn run(cli: &Cli, args: ModelSetSpaceArgs, output: &Output) -> Result<Exit> {
    catalog::validate_public_name(&args.space)
        .map_err(|e| CliError::usage(format!("SPACE: {e}")))?;
    let target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let dir = target.dir().to_path_buf();
    let files =
        crate::commands::provider::ls::provider_files(&dir).map_err(CliError::precondition)?;

    let mut found = None;
    let mut new_space_dim: Option<(String, u32)> = None;
    for path in &files {
        let Some(doc) = ProviderFileDoc::load(path)? else {
            continue;
        };
        for d in doc.descriptors() {
            if d.kind != providers::config::ModelKind::Embed {
                continue;
            }
            if d.name == args.route {
                found = Some((path.clone(), d.space_name().to_string(), d.dim));
            } else if d.space_name() == args.space {
                new_space_dim = Some((d.name.clone(), d.dim));
            }
        }
    }
    let Some((path, old_space, dim)) = found else {
        return Err(CliError::precondition(format!(
            "{:?} is not a provider route; a local route's space is the registry's decision",
            args.route
        ))
        .with_fix("name a [[models]] entry from `postvec model ls --provider`"));
    };
    if let Some((other, other_dim)) = new_space_dim {
        if other_dim != dim {
            return Err(CliError::precondition(format!(
                "route {:?} is dim {dim} but space {:?} is already served by {other:?} at \
                 dim {other_dim}",
                args.route, args.space
            )));
        }
    }

    output.note(&format!(
        "{}: space {old_space:?} -> {:?}",
        args.route, args.space
    ));
    let mut plan = Plan::new("model set-space", target.label());
    plan.push(PlanStep::WriteConfig {
        path: path.clone(),
        before_sha256: None,
        after_sha256: String::new(),
    });
    if args.dry_run {
        output.note("dry-run: no files written");
        return Ok(Exit::Success);
    }
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
    Ok(Exit::Success)
}
