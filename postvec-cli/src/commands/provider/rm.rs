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
    // When no host answers, the files must NOT quietly stand in for the
    // snapshot. They describe what the host *would* load, and when that
    // directory is over a ceiling or contested they describe an empty set —
    // while the unreachable host may well be serving the route this command
    // is about to delete. With nothing to compare against, the only honest
    // "now" is the conservative one: every route this file declares counts
    // as possibly served, so its removal is gated as a loss.
    let (served_now, now_source, host_unknown) = match live_served(&target, cli.timeout).await {
        Some(live) => (live, "the running host", false),
        None => {
            // This file's own routes, with their *real* identity read from
            // the document — not placeholders. A partial removal leaves some
            // of these models in place, and they must compare equal to
            // themselves afterwards or they would be reported as a handoff
            // to their own file.
            let mut conservative = structural_now;
            let provider = doc
                .provider_type()
                .map(providers::catalog::canonical_provider)
                .unwrap_or_default();
            let field = |key: &str| doc.value.get(key).and_then(toml::Value::as_str);
            let endpoint = providers::config::endpoint_digest(field("region"), field("base_url"));
            let dim_of = |name: &str| -> u32 {
                doc.value
                    .get("models")
                    .and_then(toml::Value::as_array)
                    .and_then(|models| {
                        models
                            .iter()
                            .find(|m| m.get("name").and_then(toml::Value::as_str) == Some(name))
                    })
                    .and_then(|m| m.get("dim"))
                    .and_then(toml::Value::as_integer)
                    .and_then(|d| u32::try_from(d).ok())
                    .unwrap_or_default()
            };
            // ROUTE ids, not raw provider ids: a converter's identity folds
            // in its pair, dims and postvec-side spaces via the same
            // derivation `served_names_if` and the live descriptor use — or
            // a partial removal would report the surviving converter as a
            // handoff to its own file.
            for (name, id) in doc.route_models() {
                let dim = dim_of(&name);
                conservative
                    .entry(name)
                    .or_insert_with(|| providers::config::ServedBy {
                        provider: provider.clone(),
                        file: args.name.clone(),
                        endpoint: endpoint.clone(),
                        model_id: id,
                        dim,
                    });
            }
            (
                conservative,
                "the files on disk — no running host answered, so every route this file \
                 declares is treated as live",
                true,
            )
        }
    };
    // `served_now` is an *upper* bound on what is served today: right for
    // losses (anything possibly live is possibly lost) and for drift, wrong
    // for activations — a sibling the files describe as served may not be,
    // on a host that loaded before it existed or refused its secret, and the
    // reload this command asks for would then bring it online ungated. The
    // lower bound is what the host *said* it serves; with no host that is
    // nothing, and everything served afterwards is treated as starting.
    let definitely_live_now = if host_unknown {
        std::collections::BTreeMap::new()
    } else {
        served_now.clone()
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
        drifted,
    } = diff_routes(&served_now, &definitely_live_now, &served_after);
    // With no host, a same-name handoff out of this file cannot be approved
    // from disk identity alone: the running host may serve the name from an
    // older snapshot whose identity is not what this file says now. Refused
    // on the same terms as drift — compatibility is not established, and an
    // acknowledgement does not establish it.
    if host_unknown {
        if let Some((name, by)) = served_after
            .iter()
            .find(|(name, by)| going_away.contains(name) && by.file != args.name)
        {
            return Err(CliError::precondition(format!(
                "removing this would hand {name} to {}.toml, and no running host answered: \
                 whether the vectors the host writes under {name} today are in the same \
                 space as {} {} at {} dimensions (endpoint {}) cannot be established from the \
                 files, and no acknowledgement establishes it",
                by.file, by.provider, by.model_id, by.dim, by.endpoint
            ))
            .with_fix(format!(
                "run this where the serving host answers (its loopback admin listener), or \
                 give that model its own public name in {}.toml and postvec.migrate() the \
                 columns to it",
                by.file
            )));
        }
    }
    // A same-name change of upstream model or width is not something an
    // acknowledgement can cover. The privacy acknowledgement says "text goes
    // to a different recipient from now on"; this says "the vectors already
    // stored under this name are in a different space from the ones written
    // next", and nothing migrates them. Refused: the right route is a new
    // public name and postvec.migrate().
    if let Some((name, now, after)) = drifted.first() {
        return Err(CliError::precondition(format!(
            "removing this would hand {name} to {}.toml, which declares it as {} {} at {} \
             dimensions (endpoint {}) where it is served as {} {} at {} (endpoint {}) today \
             ({now_source}); vectors already stored under {name} could no longer be assumed to \
             match the ones written next, and no acknowledgement migrates them",
            after.file,
            after.provider,
            after.model_id,
            after.dim,
            after.endpoint,
            now.provider,
            now.model_id,
            now.dim,
            now.endpoint
        ))
        .with_fix(format!(
            "give that model its own public name in {}.toml and postvec.migrate() the \
             columns to it, or remove it from {}.toml first",
            after.file, after.file
        )));
    }
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

    // `--path` cannot inspect a cluster, so "no bound column was found" is
    // not "no bound column exists". The lost-route acknowledgement is pushed
    // anyway, with the databases marked UNKNOWN — exactly as `provider add`
    // treats the mirror case. `--path` is the documented way to administer
    // remote nodes; it must not be the one mode where `--yes` takes a route
    // away unasked.
    // No host answered and the whole file goes: the unreachable host may be
    // serving routes from an *earlier* version of this file that the document
    // no longer declares — after an edit, or with its models section gone
    // altogether. Nothing on disk can recover them, so the file itself is the
    // route whose loss is acknowledged. With routes still declared they are
    // already in the lost set and carry the same acknowledgement.
    // The route is named by the file alone — one typeable token, because the
    // interactive acknowledgement is the operator typing the names back. The
    // explanation goes in the UNKNOWN line.
    let mut truly_going_away = truly_going_away;
    let file_is_the_route = host_unknown && remove_file && truly_going_away.is_empty();
    if file_is_the_route && !served_after.is_empty() {
        // Nothing on disk names what the host serves from this file, and
        // something else serves afterwards. One of those unknown routes may
        // share a public name with a survivor of a different vector identity
        // — a semantic replacement the loss and privacy acknowledgements
        // neither state nor prevent. Refused until a host can say what it
        // serves; with no survivor there is nothing to be replaced by, and
        // the file-route acknowledgement below is enough.
        return Err(CliError::precondition(format!(
            "{}.toml declares no model this command can recover, no running host answered, \
             and {} other route(s) would be served afterwards: one of the routes the host \
             still serves from this file may share a public name with one of them under a \
             different vector identity, and nothing here can tell",
            args.name,
            served_after.len()
        ))
        .with_fix(
            "run this where the serving host answers (its loopback admin listener). If that \
             host is gone for good, remove the file with rm(1) and restart: the new host's \
             snapshot is then the files, and this command can reason about it again",
        ));
    }
    if file_is_the_route {
        truly_going_away.push(format!("{}.toml", args.name));
    }
    let mut unknown_databases = if truly_going_away.is_empty() {
        Vec::new()
    } else if !scanned {
        vec!["every database served by this node (not inspectable from --path)".to_string()]
    } else {
        unknown_databases
    };
    if file_is_the_route {
        unknown_databases.push(
            "every route the unreachable host may still serve from this file — the file no \
             longer declares any, and the host did not answer"
                .to_string(),
        );
    } else if host_unknown && !truly_going_away.is_empty() {
        unknown_databases
            .push("whether the running host serves these routes (it did not answer)".to_string());
    }
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
        let provider = match served_now.get(name) {
            Some(from) if from == by => format!(
                "{} (file {}) — already configured; whether the host serves it today is \
                 unknown (it did not answer), so it is treated as starting",
                by.provider, by.file
            ),
            Some(from) => format!(
                "{} (file {}) — handed over from {} (file {}); if the new file does not load, \
                 these columns lose their route instead",
                by.provider, by.file, from.provider, from.file
            ),
            None => format!("{} (file {})", by.provider, by.file),
        };
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider,
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
        match (truly_going_away.is_empty(), activated.is_empty()) {
            (false, true) => plan::interactive_in_use_acknowledgement,
            (true, false) => plan::interactive_provider_privacy_acknowledgement,
            _ => plan::interactive_route_change_acknowledgement,
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
    /// Names whose recipient *changes*: not served now and served
    /// afterwards, **or** served by a different connector, file or endpoint
    /// afterwards. Columns bound to them start sending text somewhere that
    /// was not receiving it a moment ago — which is the event the privacy
    /// acknowledgement exists for, whether or not the name existed before.
    /// Each with who serves it afterwards.
    activated: Vec<(String, providers::config::ServedBy)>,
    /// Names served now and not afterwards: columns bound to them lose their
    /// route. Computed from the served sets, never from a file's contents —
    /// a name the host is not serving loses nothing when it goes.
    lost: Vec<String>,
    /// Names served on both sides under a **different semantic identity** —
    /// connector, endpoint, upstream model id or width — `(name, now,
    /// after)`. Not an activation: the vectors already stored under the name
    /// can no longer be assumed to match the ones written next, which no
    /// acknowledgement can repair. `rm` refuses these.
    drifted: Vec<(
        String,
        providers::config::ServedBy,
        providers::config::ServedBy,
    )>,
}

/// Same name, different vector space. The semantic identity is connector +
/// endpoint + model id + dimension: an equal model id string and width across
/// two connectors, or two endpoints of one connector, establishes nothing
/// about the vectors — only the file is allowed to differ. Both identities
/// have to be known for this to be stated; a snapshot that carries no model
/// id or width (none does today — the gateway always emits both) falls back
/// to the gated activation path, never to silence.
fn is_drift(now: &providers::config::ServedBy, after: &providers::config::ServedBy) -> bool {
    let known = |by: &providers::config::ServedBy| !by.model_id.is_empty() && by.dim != 0;
    known(now)
        && known(after)
        && (now.provider != after.provider
            || now.endpoint != after.endpoint
            || now.model_id != after.model_id
            || now.dim != after.dim)
}

/// Name equality is not route equality. A previous version compared names
/// only, so a same-name handoff from file A to file B — a different endpoint,
/// possibly a different company — read as "unchanged" and went ungated. It
/// also let an *unloadable* B hide the loss of A: the name was still "there"
/// afterwards, structurally, while the host would serve it from nothing.
/// Treating any change of server as an activation covers both: the operator
/// is told the name is being handed to B, and if B does not load, that
/// warning is the closest thing to the truth this process can state.
///
/// Two bounds on "now": `possibly_now` (everything that may be served — the
/// live snapshot, or the conservative view when no host answered) decides
/// losses and drift; `definitely_now` (only what a host confirmed — empty
/// without one) decides activations. With a host the two are the same map.
fn diff_routes(
    possibly_now: &std::collections::BTreeMap<String, providers::config::ServedBy>,
    definitely_now: &std::collections::BTreeMap<String, providers::config::ServedBy>,
    after: &std::collections::BTreeMap<String, providers::config::ServedBy>,
) -> RouteDiff {
    RouteDiff {
        drifted: after
            .iter()
            .filter_map(|(name, by)| {
                let before = possibly_now.get(name)?;
                is_drift(before, by).then(|| (name.clone(), before.clone(), by.clone()))
            })
            .collect(),
        activated: after
            .iter()
            .filter(|(name, by)| match definitely_now.get(*name) {
                Some(before) => before != *by,
                None => true,
            })
            .filter(|(name, by)| {
                possibly_now
                    .get(*name)
                    .is_none_or(|before| !is_drift(before, by))
            })
            .map(|(name, by)| (name.clone(), by.clone()))
            .collect(),
        lost: possibly_now
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
                        endpoint: m.provider_endpoint.unwrap_or_default(),
                        model_id: m.provider_model_id.unwrap_or_default(),
                        dim: m.target_dim.unwrap_or_default(),
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
            endpoint: "same".into(),
            model_id: "m".into(),
            dim: 4,
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

        let diff = diff_routes(&live_now, &live_now, &structural_after);
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

        let diff = diff_routes(&live_now, &live_now, &after);
        assert!(diff.lost.is_empty());
        assert_eq!(diff.activated.len(), 2);
        assert!(diff
            .activated
            .iter()
            .all(|(_, by)| by.provider == "mistral" && by.file == "beta"));
    }

    /// A name served by the *same* connector, file and endpoint on both
    /// sides is neither lost nor activated.
    #[test]
    fn an_unchanged_route_is_neither_lost_nor_activated() {
        let mut now = std::collections::BTreeMap::new();
        now.insert("openai-m0".to_string(), by("openai", "p0"));
        let after = now.clone();
        let diff = diff_routes(&now, &now, &after);
        assert!(diff.lost.is_empty() && diff.activated.is_empty());
    }

    /// Name equality is not route equality. The same public name served by a
    /// different file, connector or endpoint afterwards is a handoff — the
    /// columns' text goes somewhere new — and it has to be gated as an
    /// activation, not read as "unchanged". This also covers an unloadable
    /// successor hiding the loss of a working route: the operator is told
    /// the name is being handed over, which is the closest this process can
    /// come to the truth without the host's secrets.
    #[test]
    fn a_same_name_handoff_is_a_recipient_change() {
        let mut now = std::collections::BTreeMap::new();
        now.insert("shared".to_string(), by("openai", "alpha"));

        // Different file, same connector, endpoint, model and width: the one
        // same-name change that is a handoff — same vector space, possibly a
        // different account.
        let mut after = std::collections::BTreeMap::new();
        after.insert("shared".to_string(), by("openai", "beta"));
        let diff = diff_routes(&now, &now, &after);
        assert!(diff.lost.is_empty(), "the name is still served");
        assert_eq!(diff.activated.len(), 1, "…but by someone else");
        assert_eq!(diff.activated[0].1.file, "beta");
        assert!(diff.drifted.is_empty());

        // Same file, different endpoint digest: a moved base_url is another
        // deployment, whose vectors nothing proves compatible — drift.
        let mut moved = std::collections::BTreeMap::new();
        moved.insert(
            "shared".to_string(),
            ServedBy {
                provider: "openai".into(),
                file: "alpha".into(),
                endpoint: "elsewhere".into(),
                model_id: "m".into(),
                dim: 4,
            },
        );
        let diff = diff_routes(&now, &now, &moved);
        assert!(diff.activated.is_empty());
        assert_eq!(diff.drifted.len(), 1, "an endpoint change is drift");

        // Same model id string and width on a different connector: equal text
        // establishes nothing across companies — drift.
        let mut other = std::collections::BTreeMap::new();
        other.insert("shared".to_string(), by("mistral", "alpha"));
        let diff = diff_routes(&now, &now, &other);
        assert!(diff.activated.is_empty());
        assert_eq!(diff.drifted.len(), 1, "a connector change is drift");

        // Same everything except the upstream model, or the width: a
        // different vector space under the same name, which nothing
        // downstream can detect and no acknowledgement can migrate. Not an
        // activation — drift, which `rm` refuses outright.
        for (model_id, dim) in [("m-v2", 4u32), ("m", 8u32)] {
            let mut swapped = std::collections::BTreeMap::new();
            swapped.insert(
                "shared".to_string(),
                ServedBy {
                    provider: "openai".into(),
                    file: "alpha".into(),
                    endpoint: "same".into(),
                    model_id: model_id.into(),
                    dim,
                },
            );
            let diff = diff_routes(&now, &now, &swapped);
            assert!(
                diff.activated.is_empty(),
                "{model_id}/{dim} is not a handoff"
            );
            assert!(
                diff.lost.is_empty(),
                "{model_id}/{dim}: the name is still served"
            );
            assert_eq!(diff.drifted.len(), 1, "{model_id}/{dim} must be drift");
            assert_eq!(diff.drifted[0].1.model_id, "m");
            assert_eq!(diff.drifted[0].2.dim, dim);
        }
    }

    /// No host answered: the upper bound says a sibling is served, the lower
    /// bound says nothing is. The sibling is not lost (upper bound) and *is*
    /// an activation (lower bound) — the reload may bring it online.
    #[test]
    fn without_a_host_an_unchanged_sibling_is_an_activation_not_a_loss() {
        let mut possibly = std::collections::BTreeMap::new();
        possibly.insert("mine".to_string(), by("openai", "alpha"));
        possibly.insert("sibling".to_string(), by("mistral", "beta"));
        let definitely = std::collections::BTreeMap::new();
        let mut after = std::collections::BTreeMap::new();
        after.insert("sibling".to_string(), by("mistral", "beta"));
        let diff = diff_routes(&possibly, &definitely, &after);
        assert_eq!(diff.lost, vec!["mine".to_string()]);
        assert_eq!(diff.activated.len(), 1);
        assert_eq!(diff.activated[0].0, "sibling");
        assert!(diff.drifted.is_empty());
        // With a host that confirms the sibling, it is neither.
        let diff = diff_routes(&possibly, &possibly, &after);
        assert!(diff.activated.is_empty());
    }

    /// A same-name change of *both* recipient and space is drift first: the
    /// refusal must win over the acknowledgeable handoff.
    #[test]
    fn drift_wins_over_a_handoff() {
        let mut now = std::collections::BTreeMap::new();
        now.insert("shared".to_string(), by("openai", "alpha"));
        let mut after = std::collections::BTreeMap::new();
        after.insert(
            "shared".to_string(),
            ServedBy {
                provider: "mistral".into(),
                file: "beta".into(),
                endpoint: "other".into(),
                model_id: "mistral-embed".into(),
                dim: 1024,
            },
        );
        let diff = diff_routes(&now, &now, &after);
        assert_eq!(diff.drifted.len(), 1);
        assert!(diff.activated.is_empty());
    }
}
