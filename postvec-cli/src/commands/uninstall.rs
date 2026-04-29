//! `postvec uninstall` — remove postvec's database objects and stop serving a
//! database.
//!
//! Default policy is **retain user data**: the database, pgvector, the shadow
//! vector columns, the model assets and the OS packages are all left alone.
//! What goes away is postvec's generated triggers and runtime objects, the SQL
//! extension, and the database's entry in the CLI-owned launcher configuration.
//!
//! Two rules shape the implementation:
//!
//! - `DROP EXTENSION` never uses `CASCADE`. If something still depends on
//!   postvec, that is for the operator to see and decide about.
//! - Configuration is only changed for databases whose SQL removal actually
//!   succeeded, and only where the CLI owns the configuration in the first
//!   place.

use super::{collect, purge, require_host_privileges, Context};
use crate::checks::{self, SCHEMA_VERSION};
use crate::cli::{Cli, UninstallArgs};
use crate::config;
use crate::config::owned::{sha256_hex, ClusterState, HostLock, Ownership, STATE_SCHEMA_VERSION};
use crate::error::{CliError, Exit, Result};
use crate::facts::{DatabaseFacts, SettingsSnapshot};
use crate::output::{CommandResult, Output};
use crate::plan::{ApplyJournal, DatabasePlan, Plan, PlanStep, Prompt};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

/// Bound on how long the removal transaction waits for locks. A blocked user
/// table rolls the whole thing back, leaving the extension usable, rather than
/// stalling behind an open `ALTER TABLE`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(cli: &Cli, args: UninstallArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = checks::timestamp_now();
    let explicit = args.validated()?;

    if args.drop_columns && !args.acknowledge_data_loss {
        return Err(CliError::usage(
            "--drop-columns destroys stored embeddings and needs --acknowledge-data-loss",
        ));
    }

    let mut context = Context::open(cli, output).await?;
    // The host side is optional: an SQL-only removal is a documented use for a
    // server this machine does not host. Without it, only --keep-config makes
    // sense, because nothing here can change that server's configuration.
    let paths = context.owned_paths().ok();
    if paths.is_none() && !args.keep_config {
        return Err(CliError::precondition(
            "there is no local configuration for this cluster, so the database cannot be \
             removed from the launcher configuration",
        )
        .with_fix(
            "pass --keep-config for an SQL-only removal (the worker will keep connecting to \
             the database and idling until its configuration is changed on the host), or run \
             the command on the database host",
        ));
    }
    if !args.keep_config {
        // Only meaningful when there is host configuration to change.
        context.require_database_is_the_selected_cluster().await?;
    }
    let _lock = match (&paths, args.dry_run) {
        (Some(paths), true) => HostLock::acquire_shared_if_present(&paths.lock)?,
        (Some(paths), false) => {
            if !args.keep_config {
                require_host_privileges(&context.cluster)?;
            }
            Some(HostLock::acquire(&paths.lock)?)
        }
        // Nothing on this host to serialize against.
        (None, _) => None,
    };

    let snapshot = collect::cluster_snapshot(&mut context).await?;
    let ownership = match &paths {
        Some(paths) => paths.inspect()?,
        None => Ownership::Unmanaged,
    };

    // Which databases. `--all` is every database the launcher is configured
    // to serve — from the live setting, the owned file and the CLI's memory —
    // plus any other database in the cluster that has the extension installed
    // (a `CREATE EXTENSION` by hand, or a database `setup` never saw).
    let (targets, facts) = if args.all {
        discover_all(&mut context, &snapshot, &ownership, output).await?
    } else {
        let mut facts = Vec::new();
        for name in &explicit {
            let mut db = context.read_only();
            facts.push(db.inspect_database(name).await?);
        }
        (explicit, facts)
    };
    if args.all {
        output.note(&if targets.is_empty() {
            "--all: no database is configured for postvec and none has the extension installed"
                .to_string()
        } else {
            format!("--all: {}", targets.join(", "))
        });
    }

    // Preflight: show what will be torn down, per database.
    let mut plan = Plan::new("uninstall", context.cluster.identity.id.clone());
    plan.config_path = paths.as_ref().map(|paths| paths.config.clone());
    for database in &facts {
        plan.databases.push(DatabasePlan {
            name: database.name.clone(),
            exists: database.exists,
            extension_installed: database.postvec.is_some(),
        });
        if database.postvec.is_some() {
            plan.push(PlanStep::CleanupPostvecObjects {
                database: database.name.clone(),
                drop_columns: args.drop_columns,
                drop_destinations: args.drop_destinations(),
            });
            plan.push(PlanStep::DropExtension {
                database: database.name.clone(),
            });
        }
    }
    for summary in summarize(&facts, args.drop_columns, args.drop_destinations()) {
        output.note(&summary);
    }

    // Configuration reconciliation.
    let config_change = match &paths {
        Some(paths) => plan_config_change(&context, paths, &ownership, &snapshot, &targets, &args)?,
        None => ConfigChange::None { note: None },
    };
    match (&config_change, &paths) {
        (ConfigChange::Rewrite { rendered, .. }, Some(paths)) => plan.push(PlanStep::WriteConfig {
            path: paths.config.clone(),
            before_sha256: paths.read_config()?.as_deref().map(sha256_hex),
            after_sha256: sha256_hex(rendered),
        }),
        (ConfigChange::Remove, Some(paths)) => plan.push(PlanStep::RemoveConfig {
            path: paths.config.clone(),
        }),
        (ConfigChange::None { note: Some(note) }, _) => output.note(note),
        _ => {}
    }
    let restart_needed = !matches!(config_change, ConfigChange::None { .. }) && !args.keep_config;
    if restart_needed {
        if let Some(restart) = &context.cluster.restart {
            plan.push(PlanStep::RestartCluster {
                unit: restart.label.clone(),
            });
        }
    }

    // The host sweep, planned now so the operator confirms every path, and
    // applied last — after the restart, when nothing runs from those files.
    let purge_plan = match (&args.purge, &paths) {
        (true, Some(paths)) => {
            if !host_will_be_clear(&config_change, &ownership, &snapshot.settings) {
                return Err(CliError::precondition(
                    "--purge refused: postvec would still be configured for this cluster after \
                     the removal (the launcher configuration is not this CLI's to remove), and \
                     deleting the files under a configured extension would break the next start",
                )
                .with_fix(
                    "remove postvec from shared_preload_libraries and postvec.database in the \
                     file named above, restart, then rerun `uninstall --all --purge`",
                ));
            }
            let roots = purge_roots(&context, &snapshot.settings, &ownership, paths);
            let purge_plan = purge::plan(&roots, cli.timeout).await?;
            for step in purge_plan.steps() {
                plan.push(step);
            }
            for note in &purge_plan.notes {
                output.note(note);
            }
            if let Some(note) = purge_plan.package_note() {
                output.note(&note);
            }
            Some(purge_plan)
        }
        // clap: --purge requires --all and conflicts with --keep-config, and
        // without --keep-config a missing host configuration errored above.
        _ => None,
    };

    output.show_plan(&plan);
    if args.dry_run {
        let mut planned = ApplyJournal::default();
        if let (false, ConfigChange::None { note: Some(note) }) = (args.keep_config, &config_change)
        {
            planned.incomplete(note.clone());
        }
        let result = result_for(
            &context,
            plan,
            planned,
            started,
            started_at,
            vec!["--dry-run: nothing was changed".to_string()],
            Some("rerun without --dry-run to apply this plan".to_string()),
        );
        output.show_result(&result)?;
        // The dry run changed nothing, but it still predicts the outcome — and
        // the process exit must agree with the exit_code in the report.
        let exit = Exit::from_code(result.exit_code);
        context.close().await;
        return Ok(exit);
    }
    // A typed database name is required for data loss; `--yes` alone is
    // deliberately not enough for it.
    let acknowledgement = (args.drop_columns && !targets.is_empty()).then(|| targets.join(","));
    crate::plan::confirm(
        &plan,
        args.yes,
        acknowledgement.as_deref(),
        Prompt::from_environment(),
    )?;

    // --- apply ------------------------------------------------------------
    let mut journal = ApplyJournal::default();
    let mut messages = Vec::new();
    for database in &plan.databases {
        if !database.exists {
            journal.record(format!("database {} does not exist", database.name));
            journal.succeeded(&database.name);
            continue;
        }
        match context
            .db
            .uninstall_extension(
                &database.name,
                args.drop_columns,
                args.drop_destinations(),
                LOCK_TIMEOUT,
            )
            .await
        {
            Ok(outcome) if outcome.was_present => {
                let message = format!(
                    "removed postvec from {} ({} registry entry/entries cleaned{})",
                    database.name,
                    outcome.cleaned_entries,
                    match (args.drop_columns, args.drop_destinations()) {
                        (true, true) => ", shadow vector columns and chunk destinations dropped",
                        (true, false) => ", shadow vector columns dropped, chunk destinations kept",
                        (false, _) => ", shadow vector columns kept",
                    }
                );
                output.progress(&format!("postvec: {message}"));
                journal.record(message);
                journal.succeeded(&database.name);
            }
            // Already absent: an idempotent no-op, not a failure.
            Ok(_) => {
                journal.record(format!("postvec was not installed in {}", database.name));
                journal.succeeded(&database.name);
            }
            Err(error) => {
                output.progress(&format!("postvec: {} failed: {error}", database.name));
                journal.failed(&database.name, &error);
            }
        }
    }

    // Only databases whose SQL removal succeeded leave the configuration.
    // (With `--all` and nothing to remove there is nothing to fail.)
    if journal.succeeded_databases.is_empty() && !plan.databases.is_empty() {
        let first = journal
            .failed_databases
            .first()
            .map(|failed| failed.error.clone())
            .unwrap_or_else(|| "nothing could be removed".to_string());
        return Err(CliError::apply(first).with_fix(
            journal
                .failed_databases
                .first()
                .and_then(|failed| failed.remediation.clone())
                .unwrap_or_else(|| "fix the error above and rerun".to_string()),
        ));
    }

    let mut host_clear = false;
    if let (false, Some(paths)) = (args.keep_config, paths.as_ref()) {
        // Recomputed against the databases whose SQL removal actually
        // succeeded: a database that could not be cleaned up stays configured.
        let effective = plan_config_change(
            &context,
            paths,
            &ownership,
            &snapshot,
            &journal.succeeded_databases.clone(),
            &args,
        )?;
        host_clear = host_will_be_clear(&effective, &ownership, &snapshot.settings);
        match effective {
            ConfigChange::Rewrite { rendered, state } => {
                paths.commit(&rendered, &state)?;
                journal.record(format!("rewrote {}", paths.config.display()));
                output.progress(&format!("postvec: rewrote {}", paths.config.display()));
            }
            ConfigChange::Remove => {
                paths.remove()?;
                journal.record(format!("removed {}", paths.config.display()));
                output.progress(&format!(
                    "postvec: removed {} (revealing the configuration that was there before)",
                    paths.config.display()
                ));
                // Nothing postvec-managed is left on this host, so this is the
                // moment the provider credentials become orphaned. Teardown
                // never destroys what it did not create, and it never deletes
                // credentials silently either — so say where they are. With
                // --purge they are in the confirmed plan instead.
                if purge_plan.is_none() {
                    if let Some(note) = leftover_provider_files(&snapshot.settings) {
                        messages.push(format!("postvec: {note}"));
                    }
                }
            }
            // Nothing was changed, and the command was asked to change
            // something: the worker keeps connecting to a database the operator
            // believes is gone, so this is a partial result, not a success.
            ConfigChange::None { note } => {
                let note = note
                    .unwrap_or_else(|| "the launcher configuration was not changed".to_string());
                messages.push(format!("postvec: {note}"));
                journal.incomplete(note);
            }
        }
        if restart_needed {
            if args.no_restart {
                journal.restart_deferred = true;
            } else if context.cluster.restart.is_some() {
                let previous = context.server.clone();
                // Abandoning the process mid-restart would leave the service in
                // a state nobody has looked at.
                let _interrupt = crate::proc::InterruptGuard::hold(
                    "a restart is in progress; finishing this step — press Ctrl-C again to \
                     abort and leave the cluster in an unverified state",
                )?;
                output.progress("postvec: restarting the cluster to stop the worker");
                let server = context
                    .cluster
                    .restart_and_verify(&mut context.db, &previous, Duration::from_secs(45))
                    .await?;
                context.server = server;
                journal.record("restarted the cluster");
            } else {
                journal.restart_deferred = true;
                messages.push(
                    "postvec: no restart command is known for this cluster; restart it yourself \
                     to stop the worker"
                        .to_string(),
                );
            }
        }
    } else if args.keep_config {
        messages.push(
            "postvec: --keep-config was given, so the launcher configuration was not changed; \
             the worker will keep connecting to this database and idling"
                .to_string(),
        );
    }

    if let Some(purge_plan) = &purge_plan {
        if host_clear {
            output.progress("postvec: deleting postvec's files on this host");
            purge::apply(purge_plan, &mut journal);
            if let Some(note) = purge_plan.package_note() {
                messages.push(format!("postvec: {note}"));
            }
            for note in &purge_plan.notes {
                messages.push(format!("postvec: {note}"));
            }
        } else {
            // Planned against a prediction that did not hold (a database
            // failed to clean up, so its configuration stayed). Deleting the
            // files under a still-configured extension is the one thing this
            // must never do.
            journal.incomplete(
                "--purge was skipped: postvec is still configured for this cluster after the \
                 partial removal; fix the failure above and rerun"
                    .to_string(),
            );
        }
    }

    let result = result_for(&context, plan, journal, started, started_at, messages, None);
    output.show_result(&result)?;
    let exit = Exit::from_code(result.exit_code);
    context.close().await;
    Ok(exit)
}

/// `--all`: the databases to remove postvec from, with their facts.
///
/// Configured databases are targets whether or not they exist (a dropped
/// database must still leave the launcher configuration). Every other
/// database in the cluster is inspected and joins only when the extension is
/// installed; one that cannot be connected to is reported and skipped rather
/// than failing the whole command.
async fn discover_all(
    context: &mut Context,
    snapshot: &collect::ClusterSnapshot,
    ownership: &Ownership,
    output: &Output,
) -> Result<(Vec<String>, Vec<DatabaseFacts>)> {
    let mut configured: BTreeSet<String> = snapshot
        .settings
        .configured_databases()
        .into_iter()
        .collect();
    if let Some(state) = ownership.state() {
        configured.extend(state.managed_databases.iter().cloned());
        configured.extend(state.preserved_databases.iter().cloned());
    }
    configured.extend(ownership.configured_databases());

    let mut candidates: Vec<(String, bool)> =
        configured.iter().map(|name| (name.clone(), true)).collect();
    match context.db.list_databases().await {
        Ok(all) => candidates.extend(
            all.into_iter()
                .filter(|name| !configured.contains(name))
                .map(|name| (name, false)),
        ),
        Err(error) => output.note(&format!(
            "could not list the cluster's databases ({error}); only the configured ones are \
             considered"
        )),
    }

    let mut targets = Vec::new();
    let mut facts = Vec::new();
    for (name, is_configured) in candidates {
        let database = {
            let mut db = context.read_only();
            db.inspect_database(&name).await?
        };
        if !is_configured {
            if let Some(reason) = &database.unreachable {
                output.note(&format!("{name}: skipped, cannot connect: {reason}"));
                continue;
            }
            if database.postvec.is_none() {
                continue;
            }
        }
        targets.push(name);
        facts.push(database);
    }
    Ok((targets, facts))
}

/// Whether, after `change` is applied, nothing on this host configures
/// postvec any more — the precondition for deleting its files.
fn host_will_be_clear(
    change: &ConfigChange,
    ownership: &Ownership,
    settings: &SettingsSnapshot,
) -> bool {
    match change {
        ConfigChange::Remove => true,
        ConfigChange::Rewrite { .. } => false,
        // Nothing owned to remove: clear only if nothing configures postvec
        // at all (a purge after an earlier uninstall, say).
        ConfigChange::None { .. } => {
            matches!(ownership, Ownership::Unmanaged)
                && !settings
                    .preload_items()
                    .iter()
                    .any(|item| item == "postvec")
                && settings.configured_databases().is_empty()
        }
    }
}

fn purge_roots(
    context: &Context,
    settings: &SettingsSnapshot,
    ownership: &Ownership,
    paths: &crate::config::owned::OwnedPaths,
) -> purge::PurgeRoots {
    // `preload_was_already_present`: postvec was in shared_preload_libraries
    // from a file the CLI does not own before setup ran, and removing the
    // owned snippet leaves it there — the library must stay.
    let foreign_preload = match ownership.state() {
        Some(state) => state.preload_was_already_present,
        None => settings
            .preload_items()
            .iter()
            .any(|item| item == "postvec"),
    };
    purge::PurgeRoots {
        engine_root: settings
            .engine_path()
            .unwrap_or_else(|| std::path::PathBuf::from(config::DEFAULT_ENGINE_ROOT)),
        engine_root_in_use: purge::server_process_running(),
        providers_dir: settings.providers_path(),
        state_dir: paths
            .state
            .parent()
            .and_then(|clusters| clusters.parent())
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/postvec")),
        lock_dir: paths
            .lock
            .parent()
            .map(|dir| dir.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("/run/lock/postvec")),
        cluster_key: context.cluster.identity.key(),
        pkglibdir: context.cluster.pkglibdir.clone(),
        sharedir: context.cluster.sharedir.clone(),
        extension_files_removable: !foreign_preload,
        cli_binary: std::env::current_exe().ok(),
    }
}

/// What should happen to the owned configuration file.
///
/// One of these exists per invocation, so the size difference between variants
/// is irrelevant and boxing would only obscure the code.
#[allow(clippy::large_enum_variant)]
enum ConfigChange {
    /// Other managed databases remain, so the file is rewritten without this one.
    Rewrite {
        rendered: String,
        state: ClusterState,
    },
    /// Nothing managed remains: remove the CLI's file and reveal whatever the
    /// operator had before it.
    Remove,
    /// Nothing to do, with an explanation when the CLI cannot promise anything.
    None { note: Option<String> },
}

fn plan_config_change(
    context: &Context,
    paths: &crate::config::owned::OwnedPaths,
    ownership: &Ownership,
    snapshot: &collect::ClusterSnapshot,
    removing: &[String],
    args: &UninstallArgs,
) -> Result<ConfigChange> {
    if args.keep_config {
        return Ok(ConfigChange::None { note: None });
    }
    let Some(state) = ownership.state() else {
        // With no ownership record the CLI cannot promise to remove the
        // setting, and it will not guess. Name the file that still configures
        // it so the operator can finish by hand.
        let source = snapshot
            .settings
            .get("postvec.database")
            .and_then(|row| {
                row.sourcefile.as_ref().map(|file| match row.sourceline {
                    Some(line) => format!("{file}:{line}"),
                    None => file.clone(),
                })
            })
            .unwrap_or_else(|| "an unknown configuration file".to_string());
        return Ok(ConfigChange::None {
            note: Some(format!(
                "the launcher configuration was not written by this CLI, so it was left \
                 untouched; {source} still names this database. Pass --keep-config to \
                 acknowledge an SQL-only removal"
            )),
        });
    };
    if !ownership.is_writable() {
        return Ok(ConfigChange::None {
            note: Some(format!(
                "{}: {} — the file was left untouched",
                paths.config.display(),
                ownership
                    .drift_description()
                    .unwrap_or_else(|| "ownership is unclear".to_string())
            )),
        });
    }

    let mut managed: Vec<String> = state
        .managed_databases
        .iter()
        .filter(|name| !removing.contains(name))
        .cloned()
        .collect();
    // A preserved name is one the CLI found already configured rather than
    // added. Removing it from the file it owns does stop that database being
    // served — but whatever configured it originally is still there and will
    // reappear if the owned file is ever removed, so say so.
    let preserved: Vec<String> = state
        .preserved_databases
        .iter()
        .filter(|name| !removing.contains(name))
        .cloned()
        .collect();
    let reappearing: Vec<&String> = removing
        .iter()
        .filter(|name| state.preserved_databases.contains(name))
        .collect();

    if managed.is_empty() && preserved.is_empty() {
        return Ok(ConfigChange::Remove);
    }

    let mut databases = managed.clone();
    databases.extend(preserved.iter().cloned());
    databases.sort();
    databases.dedup();
    managed.sort();
    if !reappearing.is_empty() {
        // Not fatal, and not silent either.
        eprintln!(
            "postvec: {} was configured outside this CLI; it is removed from the owned \
             snippet, but the configuration that originally named it is untouched and would \
             take effect again if that snippet is ever removed",
            reappearing
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Rebuild the file from the recorded state, so the mode and the inference
    // settings that other databases still rely on are preserved verbatim.
    let previous = paths.read_config()?.unwrap_or_default();
    let rendered = rewrite_database_list(&previous, &databases)?;
    let digest = sha256_hex(&rendered);
    let state = ClusterState {
        schema_version: STATE_SCHEMA_VERSION,
        cluster: context.cluster.identity.id.clone(),
        config_path: paths.config.clone(),
        config_sha256: digest,
        managed_databases: managed,
        preserved_databases: preserved,
        preload_was_already_present: state.preload_was_already_present,
        preload_base: state.preload_base.clone(),
        mode: state.mode.clone(),
        updated_by_cli_version: crate::CLI_VERSION.to_string(),
        updated_at: checks::timestamp_now(),
    };
    Ok(ConfigChange::Rewrite { rendered, state })
}

/// Replace the `postvec.database` line, leaving every other line of the owned
/// file exactly as it was.
///
/// Rewriting only that line (rather than re-rendering from scratch) keeps the
/// mode and inference settings the remaining databases depend on byte-identical,
/// which matters because this command must not change how they are served.
fn rewrite_database_list(previous: &str, databases: &[String]) -> Result<String> {
    let value = crate::validate::config_literal(&config::guc::render_extension_list(databases))?;
    let mut out = String::with_capacity(previous.len());
    let mut replaced = false;
    for line in previous.lines() {
        if line.trim_start().starts_with("postvec.database") && !line.trim_start().starts_with('#')
        {
            out.push_str(&format!("postvec.database = {value}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("postvec.database = {value}\n"));
    }
    Ok(out)
}

/// Report — never remove — provider connector files left behind by a
/// teardown. They hold credentials this command did not create, so deleting
/// them is the operator's call; saying nothing would leave API keys on a host
/// nobody is looking at any more.
fn leftover_provider_files(settings: &SettingsSnapshot) -> Option<String> {
    if settings.mode() != Some(crate::cli::Mode::Embedded) {
        return None;
    }
    let dir = settings.providers_path();
    let files = match crate::commands::provider::ls::provider_files(&dir) {
        Ok(files) => files,
        // An unreadable directory is not an empty one: say so, because the
        // whole point of this note is that credentials may remain.
        Err(problem) => {
            return Some(format!(
                "{problem}; external-provider connector files may remain in {} and are never \
                 removed by uninstall",
                dir.display()
            ))
        }
    };
    if files.is_empty() {
        return None;
    }
    Some(format!(
        "{} still holds {} provider connector file(s) with API credentials; they were not \
         removed — delete them yourself once no other host needs them",
        dir.display(),
        files.len()
    ))
}

/// Per-database preflight summary: what exists and what will be torn down.
fn summarize(facts: &[DatabaseFacts], drop_columns: bool, drop_destinations: bool) -> Vec<String> {
    let mut out = Vec::new();
    for database in facts {
        if !database.exists {
            out.push(format!("{}: does not exist", database.name));
            continue;
        }
        if database.postvec.is_none() {
            out.push(format!("{}: postvec is not installed", database.name));
            continue;
        }
        let queue = database.queue.clone().unwrap_or_default();
        let unfinished = database.migrations.len();
        out.push(format!(
            "{}: {} enabled column(s), {} pending job(s), {} dead-lettered, {} unfinished \
             migration(s)",
            database.name,
            database.registry.len(),
            queue.pending,
            queue.dead,
            unfinished
        ));
        for entry in &database.registry {
            match &entry.destination {
                Some(destination) => out.push(format!(
                    "  {}.{} -> {}.{} (chunk destination, {}){}",
                    entry.relation,
                    entry.source_column,
                    destination,
                    entry.vector_column,
                    entry.state,
                    if drop_columns && drop_destinations {
                        " — TABLE AND VIEW WILL BE DROPPED"
                    } else {
                        " — kept as an ordinary table"
                    }
                )),
                None => out.push(format!(
                    "  {}.{} -> {} ({}){}",
                    entry.relation,
                    entry.source_column,
                    entry.vector_column,
                    entry.state,
                    if drop_columns {
                        " — WILL BE DROPPED (unless adopted)"
                    } else {
                        " — kept"
                    }
                )),
            }
        }
        if unfinished > 0 {
            // Not a blocker: postvec.uninstall() marks them aborted safely. It
            // does make the plan worth reading twice.
            out.push(format!(
                "  note: {unfinished} unfinished migration(s) will be marked aborted"
            ));
        }
    }
    out
}

fn result_for(
    context: &Context,
    plan: Plan,
    journal: ApplyJournal,
    started: Instant,
    started_at: String,
    extra_messages: Vec<String>,
    next_step: Option<String>,
) -> CommandResult {
    let exit = journal.exit();
    let mut messages = Vec::new();
    for entry in &journal.applied {
        messages.push(format!("postvec: {entry}"));
    }
    for failed in &journal.failed_databases {
        messages.push(format!("postvec: {} FAILED: {}", failed.name, failed.error));
    }
    for unfinished in &journal.incomplete {
        messages.push(format!("postvec: INCOMPLETE: {unfinished}"));
    }
    messages.extend(extra_messages);
    if journal.restart_deferred {
        messages.push(
            "postvec: a restart is still needed to stop the worker for the removed database(s)"
                .to_string(),
        );
    }
    CommandResult {
        schema_version: SCHEMA_VERSION,
        command: "uninstall",
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: context.cluster.identity.id.clone(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        plan,
        applied: journal.applied.clone(),
        messages,
        checks: Vec::new(),
        next_step,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNED: &str = "\
# Managed by postvec. Manual edits are refused; use `postvec setup`.
# postvec-cli 0.1.0
shared_preload_libraries = 'pg_stat_statements,postvec'
postvec.database = 'analytics,univec'
postvec.mode = 'grpc'
postvec.grpc_endpoints = '192.0.2.2:33333'
postvec.http_endpoints = 'https://192.0.2.2:22222'
";

    #[test]
    fn rewriting_the_database_list_leaves_every_other_line_untouched() {
        let rewritten = rewrite_database_list(OWNED, &["analytics".to_string()]).unwrap();
        assert!(rewritten.contains("postvec.database = 'analytics'\n"));
        assert!(!rewritten.contains("univec"));
        // Everything else is byte-identical, so the databases that remain are
        // served exactly as before.
        for line in OWNED
            .lines()
            .filter(|line| !line.contains("postvec.database"))
        {
            assert!(rewritten.contains(line), "lost line: {line}");
        }
        assert_eq!(rewritten.lines().count(), OWNED.lines().count());
    }

    #[test]
    fn rewriting_adds_the_setting_when_it_is_somehow_absent() {
        let rewritten = rewrite_database_list("# only a comment\n", &["a".to_string()]).unwrap();
        assert!(rewritten.contains("postvec.database = 'a'"));
        assert!(rewritten.contains("# only a comment"));
    }

    #[test]
    fn commented_out_settings_are_not_replaced() {
        let previous = "#postvec.database = 'old'\npostvec.database = 'live'\n";
        let rewritten = rewrite_database_list(previous, &["new".to_string()]).unwrap();
        assert!(rewritten.contains("#postvec.database = 'old'"));
        assert!(rewritten.contains("postvec.database = 'new'"));
        assert!(!rewritten.contains("'live'"));
    }

    #[test]
    fn a_database_name_needing_quoting_is_escaped() {
        let rewritten = rewrite_database_list(OWNED, &["o'brien".to_string()]).unwrap();
        assert!(rewritten.contains("postvec.database = 'o''brien'"));
    }

    fn facts(name: &str, installed: bool) -> DatabaseFacts {
        let mut facts = DatabaseFacts::absent(name);
        facts.exists = true;
        if installed {
            facts.postvec = Some(crate::facts::ExtensionFacts {
                catalog_version: "0.1.0".into(),
                library_version: "0.1.0".into(),
                build_info: None,
            });
            facts.queue = Some(crate::facts::QueueFacts {
                pending: 4,
                claimed: 0,
                dead: 1,
                oldest_pending_s: Some(3.0),
                dead_reasons: vec![],
            });
            facts.registry = vec![crate::facts::RegistryEntryFacts {
                registry_id: 1,
                relation: "public.docs".into(),
                source_column: "body".into(),
                vector_column: "body_semantic".into(),
                model: "m".into(),
                dim: 4,
                state: "active".into(),
                pending_jobs: 4,
                dead_jobs: 1,
                has_vector_index: true,
                index_mode: "manual".into(),
                index_error: None,
                has_expected_opclass_index: true,
                last_error: None,
                relation_exists: true,
                source_column_exists: true,
                vector_column_exists: true,
                trigger_count: 3,
                destination: None,
            }];
            facts.migrations = vec![crate::facts::MigrationFacts {
                id: 1,
                registry_id: 1,
                state: "awaiting_finalize".into(),
                rows_done: 1,
                rows_total: 2,
                error: None,
                age_s: Some(5.0),
                retry_failures: 0,
            }];
        }
        facts
    }

    #[test]
    fn the_summary_names_what_will_be_kept_or_dropped() {
        let keeping = summarize(&[facts("univec", true)], false, false).join("\n");
        assert!(keeping.contains("1 enabled column(s)"));
        assert!(keeping.contains("4 pending job(s)"));
        assert!(keeping.contains("1 dead-lettered"));
        assert!(keeping.contains("body_semantic"));
        assert!(keeping.contains("kept"));
        assert!(keeping.contains("marked aborted"));

        let dropping = summarize(&[facts("univec", true)], true, true).join("\n");
        assert!(dropping.contains("WILL BE DROPPED"));
    }

    /// A chunked entry names its destination table, and says whether the
    /// table goes: only with both flags, never on `--drop-columns` alone
    /// when `--keep-destinations` was given.
    #[test]
    fn the_summary_names_chunk_destinations() {
        let mut chunked = facts("univec", true);
        chunked.registry[0].destination = Some("public.docs_body_chunks".into());
        let kept = summarize(std::slice::from_ref(&chunked), true, false).join("\n");
        assert!(kept.contains("public.docs_body_chunks"), "{kept}");
        assert!(kept.contains("kept as an ordinary table"), "{kept}");
        assert!(!kept.contains("TABLE AND VIEW WILL BE DROPPED"));
        let dropped = summarize(std::slice::from_ref(&chunked), true, true).join("\n");
        assert!(
            dropped.contains("TABLE AND VIEW WILL BE DROPPED"),
            "{dropped}"
        );
    }

    #[test]
    fn the_host_is_clear_only_when_nothing_configures_postvec() {
        let empty = settings(&[]);
        assert!(host_will_be_clear(
            &ConfigChange::Remove,
            &Ownership::Unmanaged,
            &empty
        ));
        assert!(!host_will_be_clear(
            &ConfigChange::Rewrite {
                rendered: String::new(),
                state: ClusterState {
                    schema_version: STATE_SCHEMA_VERSION,
                    cluster: "18/main".into(),
                    config_path: "/c".into(),
                    config_sha256: String::new(),
                    managed_databases: vec![],
                    preserved_databases: vec![],
                    preload_was_already_present: false,
                    preload_base: vec![],
                    mode: "embedded".into(),
                    updated_by_cli_version: String::new(),
                    updated_at: String::new(),
                },
            },
            &Ownership::Unmanaged,
            &empty
        ));
        // Nothing owned and nothing configured: a purge after an earlier
        // uninstall is fine.
        assert!(host_will_be_clear(
            &ConfigChange::None { note: None },
            &Ownership::Unmanaged,
            &empty
        ));
        // Nothing owned but a hand-written configuration still preloads it.
        let foreign = settings(&[("shared_preload_libraries", "postvec")]);
        assert!(!host_will_be_clear(
            &ConfigChange::None { note: None },
            &Ownership::Unmanaged,
            &foreign
        ));
        let foreign_file = Ownership::Foreign {
            content: String::new(),
        };
        assert!(!host_will_be_clear(
            &ConfigChange::None { note: None },
            &foreign_file,
            &empty
        ));
    }

    #[test]
    fn the_summary_handles_absent_and_uninstalled_databases() {
        let absent = summarize(&[DatabaseFacts::absent("gone")], false, false).join("\n");
        assert!(absent.contains("does not exist"));
        let clean = summarize(&[facts("univec", false)], false, false).join("\n");
        assert!(clean.contains("postvec is not installed"));
    }

    fn settings(pairs: &[(&str, &str)]) -> SettingsSnapshot {
        SettingsSnapshot {
            rows: pairs
                .iter()
                .map(|(name, value)| crate::facts::SettingRow {
                    name: (*name).to_string(),
                    setting: (*value).to_string(),
                    context: "postmaster".to_string(),
                    source: "configuration file".to_string(),
                    sourcefile: None,
                    sourceline: None,
                    pending_restart: false,
                })
                .collect(),
            file_rows: Vec::new(),
        }
    }

    /// Teardown must name orphaned credentials and leave every one of them on
    /// disk — the same rule as shadow columns, applied to API keys.
    #[test]
    fn teardown_reports_leftover_provider_files_without_deleting_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let providers = dir.path().join("providers.d");
        std::fs::create_dir(&providers).expect("mkdir");
        std::fs::write(providers.join("openai.toml"), "provider = 'openai'\n").expect("write");

        let embedded = settings(&[
            ("postvec.mode", "embedded"),
            (
                "postvec.providers_path",
                providers.to_str().expect("utf-8 path"),
            ),
        ]);
        let note = leftover_provider_files(&embedded).expect("a report");
        assert!(note.contains("1 provider connector file"), "{note}");
        assert!(note.contains("were not removed"), "{note}");
        assert!(providers.join("openai.toml").exists());

        // grpc clusters keep their provider files on the postvec-server nodes.
        let remote = settings(&[
            ("postvec.mode", "grpc"),
            (
                "postvec.providers_path",
                providers.to_str().expect("utf-8 path"),
            ),
        ]);
        assert!(leftover_provider_files(&remote).is_none());

        // Zero-config: nothing to say.
        std::fs::remove_file(providers.join("openai.toml")).expect("rm");
        assert!(leftover_provider_files(&embedded).is_none());
    }
}
