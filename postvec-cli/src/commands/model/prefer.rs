//! `postvec model prefer SPACE ROUTE...`

use crate::cli::{Cli, ModelPreferArgs};
use crate::commands::provider::{
    reload_host, resolve_target, rewrite_entries, EntryEdit, ProviderFileDoc,
};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};

pub async fn run(cli: &Cli, args: ModelPreferArgs, output: &Output) -> Result<Exit> {
    if args.space.trim().is_empty() {
        return Err(CliError::usage("SPACE must not be empty"));
    }
    if args.default && !args.routes.is_empty() {
        return Err(CliError::usage("--default takes no ROUTE arguments"));
    }
    if !args.default && args.routes.is_empty() {
        return Err(CliError::usage(
            "name at least one ROUTE, or pass --default to drop explicit priorities",
        ));
    }
    let target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let dir = target.dir().to_path_buf();
    let files =
        crate::commands::provider::ls::provider_files(&dir).map_err(CliError::precondition)?;

    let mut known: Vec<(std::path::PathBuf, String, Option<u32>)> = Vec::new();
    for path in &files {
        let Some(doc) = ProviderFileDoc::load(path)? else {
            continue;
        };
        for d in doc.descriptors() {
            if d.kind != providers::config::ModelKind::Embed {
                continue;
            }
            if d.space_name() != args.space {
                continue;
            }
            known.push((path.clone(), d.name.clone(), d.priority));
        }
    }

    let mut listed = Vec::new();
    if !args.default {
        for route in &args.routes {
            if known.iter().any(|(_, name, _)| name == route) {
                listed.push(route.clone());
            } else {
                output.note(&format!("unknown route {route:?}; skipping"));
            }
        }
        if listed.is_empty() {
            return Err(CliError::precondition(format!(
                "none of the named routes serve space {:?}",
                args.space
            ))
            .with_fix("run `postvec model ls` and name provider routes of that space"));
        }
    }

    let mut edits = Vec::new();
    let mut files_touched = std::collections::BTreeSet::new();
    for (path, name, priority) in &known {
        if args.default {
            if priority.is_some() {
                files_touched.insert(path.clone());
                edits.push((
                    path.clone(),
                    name.clone(),
                    vec![EntryEdit::Remove { key: "priority" }],
                ));
            }
            continue;
        }
        if let Some(rank) = listed.iter().position(|r| r == name) {
            let want = (rank as u32) + 1;
            if *priority != Some(want) {
                files_touched.insert(path.clone());
                edits.push((
                    path.clone(),
                    name.clone(),
                    vec![EntryEdit::Set {
                        key: "priority",
                        value: toml::Value::Integer(want as i64),
                    }],
                ));
            }
        } else if priority.is_some() {
            files_touched.insert(path.clone());
            edits.push((
                path.clone(),
                name.clone(),
                vec![EntryEdit::Remove { key: "priority" }],
            ));
        }
    }

    let mut plan = Plan::new("model prefer", target.label());
    for path in &files_touched {
        plan.push(PlanStep::WriteConfig {
            path: path.clone(),
            before_sha256: None,
            after_sha256: String::new(),
        });
    }
    if args.dry_run {
        output.note("dry-run: no files written");
        return Ok(Exit::Success);
    }
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;
    rewrite_entries(&dir, edits, target.owner())?;
    let mut journal = ApplyJournal::default();
    reload_host(&target, cli.timeout, &mut journal).await;
    Ok(Exit::Success)
}
