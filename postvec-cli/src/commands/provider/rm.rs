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
    // **now** — and "now" is the live snapshot where one can be asked, not
    // the files. The two diverge: a reload that failed kept the previous
    // snapshot, so a route can be serving from a file the directory as it
    // stands would no longer load (over a ceiling, a contest introduced
    // since). Modelling "now" from the files would call that route absent,
    // and `rm --yes` would take a working route away with no acknowledgement.
    // The files are the fallback when no host answers, and the plan says
    // which source it used.
    let structural_now =
        providers::config::served_names_if(target.dir(), &file_path, Some(&doc.body()?))
            .map_err(CliError::precondition)?;
    let (served_now, now_source) = match live_served(&target, cli.timeout).await {
        Some(live) => (live, "the running host"),
        None => (
            structural_now,
            "the files on disk (no running host answered)",
        ),
    };
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
    // "After" is what the host will *try* to serve at the next reload — a
    // structural prediction, because this process cannot resolve the host's
    // secrets. A file whose secret fails there will not actually serve; the
    // gate errs toward naming it, which is the safe direction.
    let served_after =
        providers::config::served_names_if(target.dir(), &file_path, prospective_body.as_deref())
            .map_err(CliError::precondition)?;
    let RouteDiff {
        activated,
        lost: truly_going_away,
    } = diff_routes(&served_now, &served_after);
    // `going_away` still drives the journal: it is what this file declared.
    output.note(&format!("current provider routes read from {now_source}"));

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
    for (name, by) in &activated {
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
        // The recipient is the connector, not the file: a stem says nothing
        // about where text goes. The file is named alongside so the operator
        // knows which one to edit.
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider: format!("{} (file {})", by.provider, by.file),
            model: name.clone(),
            columns: mine,
            unknown_databases: unknown,
        });
    }
    if !activated.is_empty() {
        output.note(&format!(
            "removing this brings other provider files online: {} start(s) being served the \
             moment the host reloads",
            activated
                .iter()
                .map(|(n, by)| format!("{n} (by {} from {}.toml)", by.provider, by.file))
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
    // One confirmation, worded for what this removal actually does. The
    // generic form substitutes every in-use model into one sentence, which
    // reads wrongly when some columns *lose* a route and others are *handed*
    // to a different recipient — two different events, each of which has to
    // be stated on its own terms.
    let lost_list = truly_going_away.join(", ");
    let activated_list = activated
        .iter()
        .map(|(n, by)| format!("{n} → {}", by.provider))
        .collect::<Vec<_>>()
        .join(", ");
    let consequence = match (truly_going_away.is_empty(), activated.is_empty()) {
        (false, true) => format!(
            "managed columns lose their embedding route to {lost_list}; pass \
             --acknowledge-in-use together with --yes to proceed knowing those entries will \
             fail. --yes deliberately does not stand in for it"
        ),
        (true, false) => format!(
            "removing this hands {activated_list} to another provider file, so existing \
             columns bound to those names start sending their source text to a different \
             recipient on the next worker cycle; pass --acknowledge-in-use together with \
             --yes to proceed knowingly. --yes deliberately does not stand in for it"
        ),
        _ => format!(
            "removing this takes the embedding route away from {lost_list} AND hands \
             {activated_list} to another provider file, whose bound columns start sending \
             their source text to a different recipient on the next worker cycle; pass \
             --acknowledge-in-use together with --yes to proceed knowingly. --yes \
             deliberately does not stand in for it"
        ),
    };
    plan::confirm_in_use_with(
        &plan,
        args.acknowledge_in_use,
        args.yes,
        args.dry_run,
        Prompt::from_environment(),
        if activated.is_empty() {
            plan::interactive_in_use_acknowledgement
        } else {
            plan::interactive_provider_privacy_acknowledgement
        },
        &consequence,
    )?;
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

/// What a change to the served set does to the columns bound to it.
struct RouteDiff {
    /// Names served afterwards that are not served now: columns bound to
    /// them start sending text to a recipient that was not serving a moment
    /// ago. Each with who serves it.
    activated: Vec<(String, providers::config::ServedBy)>,
    /// Names served now and not afterwards: columns bound to them lose their
    /// route. Computed from the served sets, never from a file's contents —
    /// a name the host is not serving loses nothing when it goes.
    lost: Vec<String>,
}

fn diff_routes(
    now: &std::collections::BTreeMap<String, providers::config::ServedBy>,
    after: &std::collections::BTreeMap<String, providers::config::ServedBy>,
) -> RouteDiff {
    RouteDiff {
        activated: after
            .iter()
            .filter(|(name, _)| !now.contains_key(*name))
            .map(|(name, by)| (name.clone(), by.clone()))
            .collect(),
        lost: now
            .keys()
            .filter(|name| !after.contains_key(*name))
            .cloned()
            .collect(),
    }
}

/// The provider routes the running host serves **right now**, keyed by
/// public name, or `None` when no host could be asked. Read from `/config`,
/// which carries each provider entry's connector type and file stem.
async fn live_served(
    target: &ProviderTarget,
    timeout: std::time::Duration,
) -> Option<std::collections::BTreeMap<String, providers::config::ServedBy>> {
    let listen = target.embedded_listen()?;
    let inventory = crate::commands::model::admin::loaded_inventory(&listen, timeout).await?;
    Some(
        inventory
            .models
            .into_iter()
            .filter(|m| m.enabled)
            .filter_map(|m| {
                Some((
                    m.name,
                    providers::config::ServedBy {
                        provider: m.provider?,
                        file: m.provider_file.unwrap_or_default(),
                    },
                ))
            })
            .collect(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use providers::config::ServedBy;

    fn by(provider: &str, file: &str) -> ServedBy {
        ServedBy {
            provider: provider.into(),
            file: file.into(),
        }
    }

    /// "Now" is the live snapshot, and the live snapshot can hold a route the
    /// files no longer describe — a reload failed and the host kept serving
    /// what it had. Removing that file takes a *working* route away, and the
    /// diff has to say so even though a structural model of the directory
    /// would call the route already absent. This is the property that was
    /// missing: the structural "now" was empty for an over-ceiling
    /// directory, so nothing was ever "lost".
    #[test]
    fn a_route_the_live_host_serves_is_lost_even_if_the_files_no_longer_describe_it() {
        let mut live_now = std::collections::BTreeMap::new();
        live_now.insert("openai-m0".to_string(), by("openai", "p0"));
        // The files as they stand are over a ceiling: structurally, nothing
        // is served, before or after.
        let structural_after = std::collections::BTreeMap::new();

        let diff = diff_routes(&live_now, &structural_after);
        assert_eq!(diff.lost, vec!["openai-m0".to_string()]);
        assert!(diff.activated.is_empty());
    }

    /// Activation is the mirror: a name the files would serve after the
    /// change that the live host does not serve now.
    #[test]
    fn a_name_only_served_afterwards_is_activated_with_its_recipient() {
        let live_now = std::collections::BTreeMap::new();
        let mut after = std::collections::BTreeMap::new();
        after.insert("shared-name".to_string(), by("mistral", "beta"));
        after.insert("beta-only".to_string(), by("mistral", "beta"));

        let diff = diff_routes(&live_now, &after);
        assert!(diff.lost.is_empty());
        assert_eq!(diff.activated.len(), 2);
        assert!(diff
            .activated
            .iter()
            .all(|(_, by)| by.provider == "mistral" && by.file == "beta"));
    }

    /// A name present on both sides is neither: the same recipient keeps
    /// serving it, whatever file it comes from.
    #[test]
    fn an_unchanged_route_is_neither_lost_nor_activated() {
        let mut now = std::collections::BTreeMap::new();
        now.insert("openai-m0".to_string(), by("openai", "p0"));
        let after = now.clone();
        let diff = diff_routes(&now, &after);
        assert!(diff.lost.is_empty() && diff.activated.is_empty());
    }
}
