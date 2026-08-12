//! `postvec setup`: configure a cluster and install the extension.
//!
//! Create databases and CREATE EXTENSION before writing launcher config
//! and restarting. A configured missing database makes its worker fail
//! to connect and respawn every ~15s.
//!
//! discover -> validate -> plan -> confirm -> create DBs -> CREATE
//! EXTENSION -> write config -> postgres -C -> restart or reload -> smoke.

use super::{collect, require_host_privileges, Context};
use crate::checks::{self, CheckResult, CheckStatus, SCHEMA_VERSION};
use crate::cli::{Cli, Mode, ModeTarget, SetupArgs, TlsPolicy, ValidatedSetup};
use crate::config::owned::{sha256_hex, ClusterState, HostLock, Ownership, STATE_SCHEMA_VERSION};
use crate::config::{self, DesiredConfig, EmbeddedSettings, InferenceSettings, RemoteSettings};
use crate::db::ExtensionExpectation;
use crate::engine;
use crate::error::{CliError, Exit, Result};
use crate::facts::SettingsSnapshot;
use crate::output::{CommandResult, Output};
use crate::plan::{ApplyJournal, DatabasePlan, Plan, PlanStep, Prompt};
use crate::validate;
use std::time::{Duration, Instant};

/// How long to wait for the worker's heartbeat after a restart. Embedded mode
/// loads models before serving, which legitimately takes much longer.
const REMOTE_STARTUP_DEADLINE: Duration = Duration::from_secs(45);
const EMBEDDED_STARTUP_DEADLINE: Duration = Duration::from_secs(120);

pub async fn run(cli: &Cli, args: SetupArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = checks::timestamp_now();
    let validated = args.validated()?;

    let mut context = Context::open(cli, output).await?;
    // setup changes host configuration and restarts a service, so the server it
    // installs into must be the one it is about to reconfigure.
    context.require_database_is_the_selected_cluster().await?;
    let paths = context.owned_paths()?;

    // The lock is held across plan revalidation and apply, and state is
    // re-read after acquiring it: a plan built before the lock could be stale.
    // Host-wide (shared) before cluster (exclusive): a purge in progress on
    // this host refuses us, and we hold it off while we run.
    let _host_lock = if args.dry_run {
        None
    } else {
        crate::config::owned::host_lock_shared()?
    };
    let _lock = if args.dry_run {
        HostLock::acquire_shared_if_present(&paths.lock)?
    } else {
        require_host_privileges(&context.cluster)?;
        Some(HostLock::acquire(&paths.lock)?)
    };

    let snapshot = collect::cluster_snapshot(&mut context).await?;
    let ownership = paths.inspect()?;

    preflight(
        &context,
        &snapshot.settings,
        &ownership,
        &validated,
        &args,
        output,
    )
    .await?;

    // The plan is built for everything requested; what is finally written is
    // rebuilt from the databases that actually succeeded (see below).
    let desired = build_desired(
        &snapshot.settings,
        &ownership,
        &validated.target,
        &validated.databases,
    )?;
    let mode = desired.mode();
    let current = paths.read_config()?;
    let before_digest = current.as_deref().map(sha256_hex);
    let config_changed =
        before_digest.as_deref() != Some(sha256_hex(&desired.render(crate::CLI_VERSION)?).as_str());

    let mut plan = Plan::new("setup", context.cluster.identity.id.clone());
    plan.mode = Some(mode);
    plan.inference = Some(describe_inference(&desired.inference));
    plan.config_path = Some(paths.config.clone());

    for name in &validated.databases {
        let facts = {
            let mut db = context.read_only();
            db.inspect_database(name).await?
        };
        plan.databases.push(DatabasePlan {
            name: name.clone(),
            exists: facts.exists,
            extension_installed: facts.postvec.is_some(),
        });
        if !facts.exists {
            plan.push(PlanStep::CreateDatabase { name: name.clone() });
        }
        if facts.postvec.is_none() {
            plan.push(PlanStep::CreateExtension {
                database: name.clone(),
            });
        }
    }
    if config_changed {
        plan.push(PlanStep::WriteConfig {
            path: paths.config.clone(),
            before_sha256: before_digest.clone(),
            after_sha256: sha256_hex(&desired.render(crate::CLI_VERSION)?),
        });
    }
    // Driven by the difference between the desired settings and the ones in
    // effect — not by whether the file changed on this run. After
    // `setup --no-restart` a rerun writes nothing and still owes the restart.
    match activation(&desired, &snapshot.settings) {
        Activation::Restart => match &context.cluster.restart {
            Some(restart) => plan.push(PlanStep::RestartCluster {
                unit: restart.label.clone(),
            }),
            None if args.no_restart => {}
            None => {
                return Err(CliError::precondition(format!(
                    "cluster {} needs a restart to apply these settings, but no restart \
                     command is known for it",
                    context.cluster.identity.id
                ))
                .with_fix(
                    "rerun with --no-restart and restart the service yourself, then run \
                     `postvec doctor`",
                ))
            }
        },
        Activation::Reload => plan.push(PlanStep::ReloadConfig {
            database: validated.databases[0].clone(),
        }),
        Activation::None => {}
    }
    for name in &validated.databases {
        plan.push(PlanStep::SmokeCheck {
            database: name.clone(),
        });
    }

    output.show_plan(&plan);
    if args.dry_run {
        let result = CommandResult {
            schema_version: SCHEMA_VERSION,
            command: "setup",
            cli_version: crate::CLI_VERSION.to_string(),
            cluster: context.cluster.identity.id.clone(),
            started_at,
            duration_ms: started.elapsed().as_millis() as u64,
            plan,
            applied: Vec::new(),
            messages: vec!["--dry-run: nothing was changed".to_string()],
            checks: Vec::new(),
            next_step: Some("rerun without --dry-run to apply this plan".to_string()),
            exit_code: Exit::Success.code(),
        };
        output.show_result(&result)?;
        context.close().await;
        return Ok(Exit::Success);
    }
    crate::plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;

    // --- apply ------------------------------------------------------------
    let mut journal = ApplyJournal::default();
    let expectation = ExtensionExpectation {
        require_embedded_build: mode == Mode::Embedded,
    };
    for database in &plan.databases {
        match install_database(&mut context, database, &expectation, output).await {
            Ok(messages) => {
                for message in messages {
                    journal.record(message);
                }
                journal.succeeded(&database.name);
            }
            // Multi-database setup is not globally transactional. A failure is
            // journaled and the remaining targets are still attempted; a rerun
            // resumes safely because every step is idempotent.
            Err(error) => {
                output.progress(&format!("postvec: {} failed: {error}", database.name));
                journal.failed(&database.name, &error);
            }
        }
    }
    if journal.succeeded_databases.is_empty() {
        let first = journal
            .failed_databases
            .first()
            .map(|failed| failed.error.clone())
            .unwrap_or_else(|| "no database could be prepared".to_string());
        return Err(CliError::apply(first).with_fix(
            journal
                .failed_databases
                .first()
                .and_then(|failed| failed.remediation.clone())
                .unwrap_or_else(|| "fix the error above and rerun".to_string()),
        ));
    }

    // A database that failed to install must not reach postvec.database: its
    // worker would then fail to connect and be respawned every ~15 seconds
    // forever. The configuration is therefore rebuilt from what succeeded, not
    // from what was requested.
    let installed = journal.succeeded_databases.clone();
    if installed.len() != plan.databases.len() {
        output.progress(&format!(
            "postvec: configuring only the database(s) that installed successfully: {}",
            installed.join(", ")
        ));
    }
    let desired = build_desired(
        &snapshot.settings,
        &ownership,
        &validated.target,
        &installed,
    )?;
    let rendered = desired.render(crate::CLI_VERSION)?;
    let after_digest = sha256_hex(&rendered);
    let config_changed = before_digest.as_deref() != Some(after_digest.as_str());
    let activation = activation(&desired, &snapshot.settings);

    // From here until the restart is verified, a Ctrl-C would abandon the
    // cluster in a state nobody has looked at.
    let _interrupt = crate::proc::InterruptGuard::hold(
        "a configuration change is in progress; finishing this step — press Ctrl-C again to \
         abort and leave the cluster in an unverified state",
    )?;

    let mut committed = None;
    if config_changed {
        let state = build_state(
            &context,
            &paths,
            &desired,
            &ownership,
            &after_digest,
            &snapshot.settings,
            &installed,
        );
        committed = Some(paths.commit(&rendered, &state)?);
        journal.record(format!("wrote {}", paths.config.display()));
        output.progress(&format!("postvec: wrote {}", paths.config.display()));

        // Parse the candidate configuration the way the postmaster will, before
        // asking it to restart on it.
        if let Err(error) = validate_offline(&context, &desired).await {
            if let Some(committed) = &committed {
                committed.restore()?;
                output.progress("postvec: restored the previous configuration");
            }
            return Err(error);
        }
    }

    // The providers.d directory the embedded host reads. Created here rather
    // than by the packages, which can neither name the cluster owner nor
    // safely chown at unpack time; `postvec provider add` creates it too, so
    // this only makes the location exist for an operator who drops a file in
    // by hand. Nothing depends on it: an absent directory is the zero-config
    // path, so a failure here is a note, never a failed setup.
    if let InferenceSettings::Embedded(embedded) = &desired.inference {
        let dir = embedded
            .providers_path
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(config::DEFAULT_PROVIDERS_PATH));
        let owner = context
            .cluster
            .owner
            .as_ref()
            .map(crate::commands::provider::FileOwner::from);
        match crate::commands::provider::ensure_private_dir(&dir, owner) {
            Ok(true) => {
                journal.record(format!("created {} (0700)", dir.display()));
                output.progress(&format!("postvec: created {} (0700)", dir.display()));
            }
            Ok(false) => {}
            Err(error) => output.note(&format!(
                "could not create {}: {error}; external providers need it, local models do not",
                dir.display()
            )),
        }
    }

    // Without a preloaded launcher the workers are started directly, so the
    // databases are served now; the restart only makes that persistent.
    let restarting_now =
        activation == Activation::Restart && !args.no_restart && context.cluster.restart.is_some();
    if !config::guc::list_contains_postvec(&snapshot.settings.preload_items()) && !restarting_now {
        if let Some(database) = installed.first() {
            context.db.reload_config(database).await?;
        }
        for database in &installed {
            if context.db.start_worker(database).await? {
                journal.record(format!("started the worker in {database}"));
                output.progress(&format!("postvec: started the worker in {database}"));
            }
        }
    }

    let previous_server = context.server.clone();
    match activation {
        Activation::Restart => {
            if args.no_restart || context.cluster.restart.is_none() {
                journal.restart_deferred = true;
                let result = finish(
                    &context,
                    plan,
                    journal,
                    Vec::new(),
                    started,
                    started_at,
                    Some(next_restart_hint(&context)),
                );
                output.show_result(&result)?;
                let exit = Exit::from_code(result.exit_code);
                context.close().await;
                return Ok(exit);
            }
            output.progress(&format!(
                "postvec: restarting {}",
                context
                    .cluster
                    .restart
                    .as_ref()
                    .map(|restart| restart.label.clone())
                    .unwrap_or_default()
            ));
            match context
                .cluster
                .restart_and_verify(&mut context.db, &previous_server, restart_deadline(mode))
                .await
            {
                Ok(server) => {
                    context.server = server;
                    journal.record("restarted the cluster");
                }
                Err(error) => {
                    return Err(rollback_restart(&mut context, committed, error, output).await);
                }
            }
        }
        Activation::Reload => {
            // Through a database that actually installed: reloading via one
            // whose installation failed would fail after the new configuration
            // is already committed.
            match installed.first() {
                Some(database) => {
                    context.db.reload_config(database).await?;
                    journal.record("reloaded the configuration");
                }
                None => unreachable!("an empty install set returns before this point"),
            }
        }
        Activation::None => {}
    }

    // --- verify -----------------------------------------------------------
    let smoke = smoke_check(&mut context, &installed, mode, &args, output)
        .await
        .map_err(|error| {
            // The changes are already applied at this point; saying only that a
            // check failed would leave the operator guessing about the state.
            CliError::apply(format!(
                "the configuration was applied, but verification failed: {error}"
            ))
            .with_fix(error.remediation().map(str::to_string).unwrap_or_else(|| {
                "run `postvec doctor` for the full report; rerunning setup is safe".to_string()
            }))
        })?;
    let blocking: Vec<&CheckResult> = smoke.iter().filter(|check| check.is_blocking()).collect();
    if !blocking.is_empty() {
        let summary = blocking
            .iter()
            .map(|check| format!("{} ({})", check.id, check.summary))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CliError::apply(format!(
            "the configuration was applied but the installation is not healthy: {summary}"
        ))
        .with_fix("run `postvec doctor` for the full report and per-check remediation"));
    }

    let result = finish(&context, plan, journal, smoke, started, started_at, None);
    output.show_result(&result)?;
    let exit = Exit::from_code(result.exit_code);
    context.close().await;
    Ok(exit)
}

/// What is needed to make the new configuration take effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Activation {
    /// A POSTMASTER-context setting changed.
    Restart,
    /// Only SIGHUP-context settings changed.
    Reload,
    None,
}

/// Decide between a restart and a reload by comparing each expected value with
/// the one currently in effect.
///
/// Restarting for an endpoint change would be a needless outage: the endpoint
/// settings are SIGHUP-context and the worker picks them up on the next wake.
fn activation(desired: &DesiredConfig, snapshot: &SettingsSnapshot) -> Activation {
    const SIGHUP_SETTINGS: [&str; 2] = ["postvec.grpc_endpoints", "postvec.http_endpoints"];
    let mut activation = Activation::None;
    for (name, expected) in desired.expected_settings() {
        let active = snapshot.value(name).unwrap_or("");
        let differs = match name {
            // Compared as parsed lists: the server reports its own rendering.
            "shared_preload_libraries" => {
                config::guc::parse_library_list(active)
                    != config::guc::parse_library_list(&expected)
            }
            // An unset postvec.mode *is* 'embedded' to the extension, so
            // writing that out explicitly must not cost a restart.
            "postvec.mode" => normalized_mode(active) != normalized_mode(&expected),
            _ => active != expected,
        };
        if !differs {
            continue;
        }
        if SIGHUP_SETTINGS.contains(&name) {
            if activation == Activation::None {
                activation = Activation::Reload;
            }
        } else {
            return Activation::Restart;
        }
    }
    activation
}

/// `postvec.mode` unset, empty and `embedded` all mean the same thing to the
/// extension (`postvec/src/gucs.rs::parse_mode`), so writing the default out
/// explicitly is not a change and must not cost a restart.
fn normalized_mode(value: &str) -> &str {
    match value.trim() {
        "" => "embedded",
        other => other,
    }
}

fn restart_deadline(mode: Mode) -> Duration {
    match mode {
        Mode::Grpc => REMOTE_STARTUP_DEADLINE,
        Mode::Embedded => EMBEDDED_STARTUP_DEADLINE,
    }
}

/// Read-only validation before any mutation.
///
/// Everything here is a precondition the operator must fix; nothing that setup
/// itself is about to change (the preload list, the database list) is checked
/// here, because that would refuse to run for the very reason setup exists.
async fn preflight(
    context: &Context,
    snapshot: &SettingsSnapshot,
    ownership: &Ownership,
    validated: &ValidatedSetup,
    args: &SetupArgs,
    output: &Output,
) -> Result<()> {
    // 1. The right package for this server.
    let server_major = context.server.major();
    if !crate::cluster::is_supported_major(server_major) {
        return Err(CliError::precondition(format!(
            "PostgreSQL {server_major} is not supported; postvec builds for 16, 17 and 18"
        )));
    }
    if server_major != context.cluster.binary_major {
        return Err(CliError::precondition(format!(
            "the server is PostgreSQL {server_major} but the selected installation reports {}",
            context.cluster.binary_major
        ))
        .with_fix("select the matching cluster with --cluster, or installation with --pg-config"));
    }

    // 2. Package assets, so `CREATE EXTENSION` cannot fail for a missing file.
    let assets = context.cluster.assets().ok_or_else(|| {
        CliError::precondition(
            "setup needs a local PostgreSQL installation to configure; only a connection URI \
             was supplied",
        )
        .with_fix(
            "run setup on the database host (installing the SQL extension without configuring \
             its launcher would report success for a deployment that cannot work)",
        )
    })?;
    let mut missing = Vec::new();
    if assets.postvec_control.is_none() {
        missing.push("postvec.control".to_string());
    }
    if assets.postvec_sql_versions.is_empty() {
        missing.push("postvec--<version>.sql".to_string());
    }
    if assets.postvec_library.is_none() {
        missing.push("the postvec shared library".to_string());
    }
    if assets.vector_control.is_none() {
        missing.push("vector.control (pgvector)".to_string());
    }
    if !missing.is_empty() {
        return Err(CliError::precondition(format!(
            "missing from the PostgreSQL {server_major} directories: {}",
            missing.join(", ")
        ))
        .with_fix(format!(
            "install the postvec and pgvector packages built for PostgreSQL {server_major}; \
             the CLI never installs or copies package files itself"
        )));
    }

    // 3. Configuration ownership: never overwrite what the CLI does not own.
    let paths = context.cluster.owned_paths()?;
    if !ownership.is_writable() {
        let mut error = CliError::precondition(format!(
            "{}: {}",
            paths.config.display(),
            ownership
                .drift_description()
                .unwrap_or_else(|| "configuration ownership is unclear".to_string())
        ))
        .with_fix(
            "postvec will not overwrite configuration it does not own, even with --yes; \
             reconcile or remove the file, then rerun",
        );
        if let Ownership::Foreign { content } | Ownership::AdoptableMarker { content } = &ownership
        {
            output.note("the existing file is:");
            for line in content.lines().take(20) {
                output.note(&format!("  {line}"));
            }
            error = error.with_fix(
                "the existing file is shown above; postvec will not overwrite configuration it \
                 does not own",
            );
        }
        return Err(error);
    }

    // 4. Mode is cluster-wide, so switching it affects every configured
    //    database, not just the ones named here.
    let requested_mode = validated.target.mode();
    let current_mode = snapshot
        .mode()
        .filter(|_| config::guc::list_contains_postvec(&snapshot.preload_items()))
        .or_else(|| {
            ownership
                .state()
                .and_then(|state| match state.mode.as_str() {
                    "embedded" => Some(Mode::Embedded),
                    "grpc" => Some(Mode::Grpc),
                    _ => None,
                })
        });
    if let Some(current) = current_mode {
        if current != requested_mode {
            if !args.switch_mode {
                return Err(CliError::precondition(format!(
                    "this cluster is configured for {current} inference and the request is \
                     {requested_mode}; the mode is cluster-wide"
                ))
                .with_fix(
                    "rerun with --switch-mode, naming every database the launcher serves, so \
                     no database is left claiming the old mode",
                ));
            }
            let configured = snapshot.configured_databases();
            let unnamed: Vec<&String> = configured
                .iter()
                .filter(|name| !validated.databases.contains(name))
                .collect();
            if !unnamed.is_empty() {
                return Err(CliError::precondition(format!(
                    "switching to {requested_mode} affects every configured database, but {} \
                     is not named in this command",
                    unnamed
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .with_fix("add --database for each of them"));
            }
        }
    }

    // 5. Mode-specific validation of what the operator supplied.
    match &validated.target {
        ModeTarget::Grpc { grpc, http } => {
            let probe =
                engine::remote::probe(grpc, http, TlsPolicy::ExtensionCompatible, context.timeout)
                    .await?;
            let grpc_up = probe.grpc.iter().filter(|node| node.connected).count();
            let http_up = probe
                .http
                .iter()
                .filter(|node| node.config.is_some())
                .count();
            for node in &probe.grpc {
                if !node.connected {
                    output.note(&format!(
                        "note: gRPC {} is not reachable ({})",
                        node.endpoint.authority,
                        node.connect_error
                            .as_deref()
                            .or(node.resolve_error.as_deref())
                            .unwrap_or("no detail")
                    ));
                }
            }
            for node in &probe.http {
                if node.config.is_none() {
                    output.note(&format!(
                        "note: {} did not serve a usable /config ({})",
                        node.endpoint.base,
                        node.error.as_deref().unwrap_or("no detail")
                    ));
                }
            }
            if (grpc_up == 0 || http_up == 0) && !args.allow_unreachable {
                return Err(CliError::precondition(
                    "none of the supplied endpoints are usable (a reachable gRPC endpoint and a \
                     valid GET /config are both needed)"
                        .to_string(),
                )
                .with_fix(
                    "fix the endpoints, or pass --allow-unreachable to configure the cluster \
                     now and verify later with `postvec doctor`",
                ));
            }
        }
        ModeTarget::Embedded {
            path,
            providers_path: _,
            models,
            grpc_listen,
            http_listen,
        } => {
            let probe = engine::embedded::inspect_root(
                Some(path.clone()),
                crate::facts::RootSource::Guc,
                context.cluster.owner.as_ref(),
            );
            if let Some(error) = &probe.root_error {
                return Err(CliError::precondition(format!("--path {error}")));
            }
            if let Some(error) = &probe.models_dir_error {
                return Err(CliError::precondition(format!(
                    "{}/models is unusable: {error}",
                    path.display()
                ))
                .with_fix("the engine loads models from <root>/models/<backend>/<model>/"));
            }
            if engine::embedded::readable_by(path, context.cluster.owner.as_ref(), context.timeout)
                .await
                == Some(false)
            {
                return Err(CliError::precondition(format!(
                    "the PostgreSQL account cannot read {}",
                    path.display()
                ))
                .with_fix("grant it read and traverse permission on the engine root"));
            }
            if probe.ort_libraries.is_empty() {
                let versioned = engine::embedded::find_versioned_ort_libraries(&path.join("libs"));
                return Err(CliError::precondition(format!(
                    "no ONNX Runtime library under {}/libs",
                    path.display()
                ))
                .with_fix(if versioned.is_empty() {
                    "the engine dlopens it at startup; add it to the engine root".to_string()
                } else {
                    format!(
                        "only versioned libraries are present ({}); the engine matches the \
                         filename exactly, so add an unversioned name alongside them",
                        versioned
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }));
            }
            if !probe.descriptor_errors.is_empty() {
                let detail = probe
                    .descriptor_errors
                    .iter()
                    .map(|error| format!("{}: {}", error.path.display(), error.error))
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(CliError::precondition(format!(
                    "{} model descriptor(s) cannot be read: {detail}",
                    probe.descriptor_errors.len()
                )));
            }
            let directories = probe.directory_names();
            let missing: Vec<&String> = models
                .iter()
                .filter(|name| !directories.contains(*name))
                .collect();
            if !missing.is_empty() {
                return Err(CliError::precondition(format!(
                    "requested model(s) not found under {}/models: {}",
                    path.display(),
                    missing
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .with_fix(
                    "--model names a model directory; a convert-only target also needs its \
                     converter and the embed-bridge executor",
                ));
            }
            // A deliberately empty engine root is valid for plumbing tests, but
            // it must be an informed choice rather than a surprise.
            if probe.descriptors.iter().all(|model| !model.enabled) {
                output.progress(
                    "warning: no model under the engine root is enabled; embedding will not work \
                     until one is added (the model set is fixed at engine start — adding \
                     one needs a restart)",
                );
            }
            let settings = EmbeddedSettings::new(
                path.clone(),
                None,
                models.clone(),
                *grpc_listen,
                *http_listen,
            );
            for address in [settings.grpc_listen, settings.http_listen] {
                validate::loopback_listener(address, "the embedded listener")?;
                if engine::embedded::port_is_taken(&address.to_string(), Duration::from_millis(500))
                    .await
                    && snapshot.mode() != Some(Mode::Embedded)
                {
                    // Only a warning: after a restart the engine may well be the
                    // process already holding it.
                    output.progress(&format!(
                        "warning: {address} is already accepting connections; if that is not \
                         a postvec engine, the engine will fail to bind and retry every 30s"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Build the file content, merging rather than clobbering.
fn build_desired(
    snapshot: &SettingsSnapshot,
    ownership: &Ownership,
    target: &ModeTarget,
    databases_to_serve: &[String],
) -> Result<DesiredConfig> {
    // The effective value is about to be read, merged and written back. If it
    // is not something the postmaster would accept, refuse: rewriting it would
    // silently repair syntax the operator got wrong, and they would only find
    // out the next time they hand-edited the file.
    if let Some(raw) = snapshot.value("shared_preload_libraries") {
        if let Err(e) = config::guc::validate_library_list(raw) {
            return Err(CliError::precondition(format!(
                "the cluster's shared_preload_libraries cannot be parsed: {e}\n  value: {raw}"
            ))
            .with_fix(
                "fix the value in the configuration file that sets it, then run setup again \
                 — postvec will not rewrite a list the server would reject",
            ));
        }
    }

    let active_preload = snapshot.preload_items();
    let owned_sets_preload = snapshot
        .get("shared_preload_libraries")
        .and_then(|row| row.sourcefile.clone())
        .zip(ownership.state().map(|state| state.config_path.clone()))
        .map(|(source, owned)| std::path::Path::new(&source) == owned)
        .unwrap_or(false);

    // Where postvec is already preloaded by configuration the CLI does not own,
    // the setting is left alone entirely: restating it would silently take
    // ownership of the operator's value, and uninstall could then not keep its
    // promise to leave it as it found it.
    let preload_already_present =
        config::guc::list_contains_postvec(&active_preload) && !owned_sets_preload;

    let preload = if preload_already_present {
        None
    } else {
        // The base is everything *except* postvec, from whatever source. When
        // the owned file is the current source, the recorded base is the
        // authoritative history; otherwise the active list is.
        let base = ownership
            .state()
            .filter(|_| owned_sets_preload)
            .map(|state| state.preload_base.clone())
            .unwrap_or_else(|| config::guc::remove_postvec(&active_preload));
        Some(config::guc::merge_postvec(&base))
    };

    // Databases: everything already served plus everything requested. Names the
    // CLI did not configure itself are preserved, never claimed.
    let mut databases = databases_to_serve.to_vec();
    if let Some(state) = ownership.state() {
        databases.extend(state.managed_databases.iter().cloned());
        databases.extend(state.preserved_databases.iter().cloned());
    }
    databases.extend(snapshot.configured_databases());
    databases.extend(ownership.configured_databases());
    let databases = validate::database_list(&databases)?;

    let inference = match target {
        ModeTarget::Grpc { grpc, http } => InferenceSettings::Grpc(RemoteSettings {
            grpc: grpc.clone(),
            http: http.clone(),
        }),
        ModeTarget::Embedded {
            path,
            providers_path,
            models,
            grpc_listen,
            http_listen,
        } => InferenceSettings::Embedded(EmbeddedSettings::new(
            path.clone(),
            providers_path.clone(),
            models.clone(),
            *grpc_listen,
            *http_listen,
        )),
    };

    Ok(DesiredConfig {
        preload,
        databases,
        inference,
    })
}

fn build_state(
    context: &Context,
    paths: &crate::config::owned::OwnedPaths,
    desired: &DesiredConfig,
    ownership: &Ownership,
    digest: &str,
    snapshot: &SettingsSnapshot,
    requested: &[String],
) -> ClusterState {
    let previous = ownership.state();
    // Requested databases become managed; anything else that was already
    // configured stays preserved, so uninstall never removes a name the CLI did
    // not add.
    let mut managed: Vec<String> = previous
        .map(|state| state.managed_databases.clone())
        .unwrap_or_default();
    for name in requested {
        if !managed.contains(name) {
            managed.push(name.clone());
        }
    }
    managed.sort();
    managed.dedup();
    let preserved: Vec<String> = desired
        .databases
        .iter()
        .filter(|name| !managed.contains(name))
        .cloned()
        .collect();

    ClusterState {
        schema_version: STATE_SCHEMA_VERSION,
        cluster: context.cluster.identity.id.clone(),
        config_path: paths.config.clone(),
        config_sha256: digest.to_string(),
        managed_databases: managed,
        preserved_databases: preserved,
        preload_was_already_present: desired.preload.is_none(),
        preload_base: desired
            .preload
            .as_ref()
            .map(|items| config::guc::remove_postvec(items))
            .unwrap_or_else(|| snapshot.preload_items()),
        mode: desired.mode().as_guc().to_string(),
        updated_by_cli_version: crate::CLI_VERSION.to_string(),
        updated_at: checks::timestamp_now(),
    }
}

async fn install_database(
    context: &mut Context,
    database: &DatabasePlan,
    expectation: &ExtensionExpectation,
    output: &Output,
) -> Result<Vec<String>> {
    let mut messages = Vec::new();
    if !database.exists {
        context.db.create_database(&database.name).await?;
        output.progress(&format!("postvec: created database {}", database.name));
        messages.push(format!("created database {}", database.name));
    }
    let outcome = context
        .db
        .install_extension(&database.name, expectation.clone())
        .await?;
    if outcome.created {
        output.progress(&format!(
            "postvec: installed postvec {} in {}",
            outcome.catalog_version, database.name
        ));
        messages.push(format!(
            "installed postvec {} in {}",
            outcome.catalog_version, database.name
        ));
    } else {
        messages.push(format!(
            "postvec {} was already installed in {}",
            outcome.catalog_version, database.name
        ));
    }
    Ok(messages)
}

/// Query every setting the plan asserts with `postgres -C` and require the
/// candidate configuration to agree.
async fn validate_offline(context: &Context, desired: &DesiredConfig) -> Result<()> {
    for (name, expected) in desired.expected_settings() {
        match context
            .cluster
            .query_setting_offline(name, context.timeout)
            .await?
        {
            crate::cluster::OfflineSetting::Value(actual) => {
                let matches = if name == "shared_preload_libraries" {
                    config::guc::parse_library_list(&actual)
                        == config::guc::parse_library_list(&expected)
                } else {
                    actual == expected
                };
                if !matches {
                    return Err(CliError::apply(format!(
                        "the written configuration does not take effect: {name} would be \
                         {actual:?}, not {expected:?}"
                    ))
                    .with_fix(
                        "another file wins over the conf.d snippet — most often \
                         postgresql.auto.conf, written by ALTER SYSTEM, which PostgreSQL reads \
                         last. Clear it with `ALTER SYSTEM RESET <setting>` and rerun",
                    ));
                }
            }
            crate::cluster::OfflineSetting::Unset => {
                return Err(CliError::apply(format!(
                    "the written configuration does not take effect: {name} is not set at all"
                ))
                .with_fix(format!(
                    "check that postgresql.conf includes the directory containing {}",
                    config::OWNED_FILE_NAME
                )));
            }
            crate::cluster::OfflineSetting::Error(detail) => {
                return Err(CliError::apply(format!(
                    "the candidate configuration does not parse: {detail}"
                ))
                .with_fix("the cluster would fail to start; the previous file has been restored"));
            }
            // Without the server's paths this validation is impossible; the
            // restart itself then becomes the test, which is why the previous
            // file is kept for rollback.
            crate::cluster::OfflineSetting::NotObservable(_) => return Ok(()),
        }
    }
    Ok(())
}

/// Put the previous configuration back and try once to bring the cluster up on
/// it. Both failures are reported; nothing loops.
async fn rollback_restart(
    context: &mut Context,
    committed: Option<crate::config::owned::CommittedConfig>,
    original: CliError,
    output: &Output,
) -> CliError {
    let Some(committed) = committed else {
        return original;
    };
    output.progress("postvec: restart failed; restoring the previous configuration");
    if let Err(error) = committed.restore() {
        return CliError::apply(format!(
            "the cluster failed to restart ({original}) and the previous configuration could \
             not be restored ({error})"
        ))
        .with_fix(format!(
            "restore {} by hand and restart the cluster",
            committed.config_path.display()
        ));
    }
    let previous = context.server.clone();
    match context
        .cluster
        .restart_and_verify(&mut context.db, &previous, REMOTE_STARTUP_DEADLINE)
        .await
    {
        Ok(_) => CliError::apply(format!(
            "the cluster failed to restart with the new configuration ({original}); the \
             previous configuration was restored and the cluster is running again"
        ))
        .with_fix("read the PostgreSQL log for why the new configuration was rejected"),
        Err(second) => CliError::apply(format!(
            "the cluster failed to restart with the new configuration ({original}) and also \
             with the restored one ({second})"
        ))
        .with_fix("the cluster is down; inspect the PostgreSQL log immediately"),
    }
}

/// Post-restart verification: prove the worker is alive and the selected
/// inference path is usable.
async fn smoke_check(
    context: &mut Context,
    databases: &[String],
    mode: Mode,
    args: &SetupArgs,
    output: &Output,
) -> Result<Vec<CheckResult>> {
    let deadline = restart_deadline(mode);
    for database in databases {
        output.progress(&format!(
            "postvec: waiting for the worker in {database} (up to {})",
            humantime::format_duration(deadline)
        ));
        wait_for_worker(context, database, deadline).await?;
    }

    // The one mutating verification step, and the reason it is here rather than
    // in doctor: it proves discovery works end to end.
    for database in databases {
        match context.db.refresh_models(database).await {
            Ok(count) => output.progress(&format!(
                "postvec: {database}: refresh_models() cached {count} model(s)"
            )),
            Err(error) if args.allow_unreachable => {
                output.progress(&format!(
                    "postvec: {database}: refresh_models() failed ({error}); continuing because \
                     --allow-unreachable was given"
                ));
            }
            Err(error) => return Err(error),
        }
    }

    let snapshot = collect::cluster_snapshot(context).await?;
    let facts = collect::database_facts(
        context,
        databases,
        true,
        collect::poll_interval(&snapshot.settings),
        crate::checks::database::fresh_threshold(&snapshot.settings),
    )
    .await?;
    let inference = collect::inference_probe(
        context,
        &snapshot.settings,
        TlsPolicy::ExtensionCompatible,
        None,
    )
    .await?;
    // Already proven before any mutation; restated here so the smoke report
    // carries the same evidence doctor would.
    let identity = context.prove_database_is_the_selected_cluster().await;
    // `registry: None` — setup's smoke check never performs registry network
    // access; the registry.reachable check reports SKIP.
    let mut results = collect::evaluate(
        context, &snapshot, &facts, &inference, &identity, None, true, false,
    );
    if args.allow_unreachable {
        downgrade_reachability(&mut results);
    }
    Ok(results)
}

/// `--allow-unreachable` downgrades engine reachability and model-cache
/// failures to warnings. It deliberately does not touch preload, database,
/// extension, mode, configuration, restart or heartbeat failures.
fn downgrade_reachability(results: &mut [CheckResult]) {
    const DOWNGRADABLE: [&str; 8] = [
        "remote.grpc.address",
        "remote.grpc.connect",
        "remote.http.config",
        "remote.models.consistency",
        "embedded.grpc-listener",
        "embedded.http-listener",
        "embedded.loaded-models",
        "embedded.cache-consistency",
    ];
    for check in results.iter_mut() {
        let downgradable = DOWNGRADABLE.contains(&check.id)
            || check.id == "models.cache"
            || check.id == "queue.pending";
        if downgradable && check.status == CheckStatus::Fail {
            check.status = CheckStatus::Warn;
            check.required = false;
            check.summary = format!("{} (accepted by --allow-unreachable)", check.summary);
        }
    }
}

/// Wait for a heartbeat to appear and advance.
async fn wait_for_worker(context: &mut Context, database: &str, deadline: Duration) -> Result<()> {
    let started = Instant::now();
    let mut previous: Option<f64> = None;
    while started.elapsed() < deadline {
        let sample = {
            let mut db = context.read_only();
            db.heartbeat_sample(database).await
        };
        if let Ok(sample) = sample {
            if let (Some(_pid), Some(age)) = (sample.pid, sample.age_s) {
                // A reading younger than the one before it means a new beat was
                // written between the two samples. That is the only evidence
                // that proves the worker is running *now*: the heartbeat row
                // survives a dead worker, so both its presence and a single
                // fresh-looking age can be left over from one that has died.
                if previous.is_some_and(|last| age < last) {
                    return Ok(());
                }
                previous = Some(age);
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
    Err(CliError::apply(format!(
        "no advancing worker heartbeat in {database:?} within {}",
        humantime::format_duration(deadline)
    ))
    .with_fix(
        "check that postvec is preloaded and this database is in postvec.database (or run \
         SELECT postvec.start_worker()), then read the PostgreSQL log; in embedded mode the \
         engine loads models before serving",
    ))
}

fn describe_inference(inference: &InferenceSettings) -> String {
    match inference {
        InferenceSettings::Grpc(remote) => format!(
            "gRPC {}, discovery {}",
            remote
                .grpc
                .iter()
                .map(|endpoint| endpoint.authority.clone())
                .collect::<Vec<_>>()
                .join(", "),
            remote
                .http
                .iter()
                .map(|endpoint| endpoint.base.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        InferenceSettings::Embedded(embedded) => format!(
            "engine root {}, {}, listeners {} / {}",
            embedded.path.display(),
            if embedded.models.is_empty() {
                "every enabled model on disk".to_string()
            } else {
                format!("models {}", embedded.models.join(", "))
            },
            embedded.grpc_listen,
            embedded.http_listen
        ),
    }
}

fn next_restart_hint(context: &Context) -> String {
    match &context.cluster.restart {
        Some(restart) => format!(
            "the workers are running, but survive a server restart only once postvec is \
             preloaded; restart with `{}` (or rerun `postvec setup` without --no-restart), \
             then run `postvec doctor`",
            restart.display()
        ),
        None => "the workers are running, but survive a server restart only once postvec is \
                 preloaded; restart the cluster, then run `postvec doctor`"
            .to_string(),
    }
}

fn finish(
    context: &Context,
    plan: Plan,
    journal: ApplyJournal,
    checks: Vec<CheckResult>,
    started: Instant,
    started_at: String,
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
    if journal.restart_deferred {
        messages.push("postvec: the restart was deferred (--no-restart)".to_string());
    }
    CommandResult {
        schema_version: SCHEMA_VERSION,
        command: "setup",
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: context.cluster.identity.id.clone(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        plan,
        applied: journal.applied.clone(),
        messages,
        checks,
        next_step,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::owned::STATE_SCHEMA_VERSION;
    use crate::facts::SettingRow;
    use std::path::PathBuf;

    fn snapshot(pairs: &[(&str, &str)], sourcefile: Option<&str>) -> SettingsSnapshot {
        SettingsSnapshot {
            rows: pairs
                .iter()
                .map(|(name, value)| SettingRow {
                    name: (*name).into(),
                    setting: (*value).into(),
                    context: "postmaster".into(),
                    source: "configuration file".into(),
                    sourcefile: sourcefile.map(str::to_string),
                    sourceline: Some(3),
                    pending_restart: false,
                })
                .collect(),
            file_rows: Vec::new(),
        }
    }

    fn remote_target() -> ModeTarget {
        ModeTarget::Grpc {
            grpc: validate::grpc_endpoints(&["192.0.2.2:33333".into()]).unwrap(),
            http: validate::http_endpoints(&["https://192.0.2.2:22222".into()]).unwrap(),
        }
    }

    /// The common shape in these tests: one requested database, remote mode.
    fn desired_for(
        snapshot: &SettingsSnapshot,
        ownership: &Ownership,
        databases: &[&str],
    ) -> DesiredConfig {
        let databases: Vec<String> = databases.iter().map(|name| name.to_string()).collect();
        build_desired(snapshot, ownership, &remote_target(), &databases).unwrap()
    }

    fn owned_state(managed: &[&str], preload_base: &[&str]) -> ClusterState {
        ClusterState {
            schema_version: STATE_SCHEMA_VERSION,
            cluster: "18/main".into(),
            config_path: PathBuf::from("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
            config_sha256: "digest".into(),
            managed_databases: managed.iter().map(|name| name.to_string()).collect(),
            preserved_databases: vec![],
            preload_was_already_present: false,
            preload_base: preload_base.iter().map(|name| name.to_string()).collect(),
            mode: "grpc".into(),
            updated_by_cli_version: "0.1.0".into(),
            updated_at: "2026-07-30T00:00:00Z".into(),
        }
    }

    #[test]
    fn an_existing_preload_list_is_merged_not_clobbered() {
        let snapshot = snapshot(
            &[(
                "shared_preload_libraries",
                "pg_stat_statements,auto_explain",
            )],
            Some("/etc/postgresql/18/main/postgresql.conf"),
        );
        let desired = desired_for(&snapshot, &Ownership::Unmanaged, &["univec"]);
        assert_eq!(
            desired.preload.as_deref(),
            Some(
                ["pg_stat_statements", "auto_explain", "postvec"]
                    .map(String::from)
                    .as_slice()
            ),
            "existing entries survive in order, with postvec appended once"
        );
    }

    #[test]
    fn a_foreign_preload_of_postvec_is_left_alone() {
        // postvec is already preloaded by a file the CLI does not own: taking
        // over the setting would make uninstall unable to keep its promise.
        let snapshot = snapshot(
            &[("shared_preload_libraries", "pg_stat_statements,postvec")],
            Some("/etc/postgresql/18/main/postgresql.conf"),
        );
        let desired = desired_for(&snapshot, &Ownership::Unmanaged, &["univec"]);
        assert!(desired.preload.is_none());
        let rendered = desired.render("0.1.0").unwrap();
        assert!(!rendered.contains("shared_preload_libraries"));
    }

    #[test]
    fn rerunning_setup_reproduces_the_same_file() {
        let state = owned_state(&["univec"], &["pg_stat_statements"]);
        let snapshot = snapshot(
            &[
                ("shared_preload_libraries", "pg_stat_statements,postvec"),
                ("postvec.database", "univec"),
                ("postvec.mode", "grpc"),
            ],
            Some("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
        );
        let ownership = Ownership::Managed {
            state: state.clone(),
        };
        let first = desired_for(&snapshot, &ownership, &["univec"]);
        let second = desired_for(&snapshot, &ownership, &["univec"]);
        assert_eq!(first, second);
        assert_eq!(
            first.preload.as_deref(),
            Some(
                ["pg_stat_statements", "postvec"]
                    .map(String::from)
                    .as_slice()
            ),
            "the recorded base prevents postvec accumulating in the list"
        );
        assert_eq!(first.databases, ["univec"]);
        // And the same input renders byte-identically, which is what makes the
        // rerun a provable no-op.
        assert_eq!(
            first.render("0.1.0").unwrap(),
            second.render("0.1.0").unwrap()
        );
    }

    #[test]
    fn adding_a_database_keeps_the_existing_ones() {
        let snapshot = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "univec"),
            ],
            Some("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
        );
        let desired = desired_for(
            &snapshot,
            &Ownership::Managed {
                state: owned_state(&["univec"], &["pg_stat_statements"]),
            },
            &["analytics"],
        );
        assert_eq!(
            desired.databases,
            ["analytics", "univec"],
            "the already-served database must not be dropped"
        );
    }

    #[test]
    fn databases_configured_outside_the_cli_are_preserved() {
        let snapshot = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "legacy_db"),
            ],
            Some("/etc/postgresql/18/main/postgresql.conf"),
        );
        let desired = desired_for(&snapshot, &Ownership::Unmanaged, &["univec"]);
        assert_eq!(desired.databases, ["legacy_db", "univec"]);
    }

    #[test]
    fn a_postmaster_setting_needs_a_restart_and_endpoints_only_need_a_reload() {
        let desired = desired_for(&snapshot(&[], None), &Ownership::Unmanaged, &["univec"]);

        // Nothing configured yet: the preload and database list both change.
        assert_eq!(
            activation(&desired, &snapshot(&[], None)),
            Activation::Restart
        );

        // Everything already in effect: nothing to do.
        let in_effect = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "univec"),
                ("postvec.mode", "grpc"),
                ("postvec.grpc_endpoints", "192.0.2.2:33333"),
                ("postvec.http_endpoints", "https://192.0.2.2:22222"),
            ],
            None,
        );
        assert_eq!(activation(&desired, &in_effect), Activation::None);

        // Only the endpoints differ: a reload is enough, and an outage would be
        // gratuitous.
        let endpoints_changed = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "univec"),
                ("postvec.mode", "grpc"),
                ("postvec.grpc_endpoints", "192.0.2.9:33333"),
                ("postvec.http_endpoints", "https://192.0.2.2:22222"),
            ],
            None,
        );
        assert_eq!(activation(&desired, &endpoints_changed), Activation::Reload);

        // A database list change is POSTMASTER context.
        let database_changed = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "other"),
                ("postvec.mode", "grpc"),
                ("postvec.grpc_endpoints", "192.0.2.2:33333"),
                ("postvec.http_endpoints", "https://192.0.2.2:22222"),
            ],
            None,
        );
        assert_eq!(activation(&desired, &database_changed), Activation::Restart);
    }

    /// An unset `postvec.mode` means the extension's default, which is
    /// **embedded**, so rendering that explicitly must not manufacture an
    /// outage — while switching an unconfigured cluster to remote genuinely
    /// is a change and must cost the restart it needs.
    #[test]
    fn writing_the_default_mode_explicitly_does_not_cost_a_restart() {
        let unset = |extra: &[(&str, &str)]| {
            let mut settings: Vec<(&str, &str)> = vec![
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "univec"),
                ("postvec.mode", ""),
            ];
            settings.extend_from_slice(extra);
            snapshot(&settings, None)
        };

        let embedded = build_desired(
            &snapshot(&[], None),
            &Ownership::Unmanaged,
            &ModeTarget::Embedded {
                path: PathBuf::from("/opt/postvec"),
                providers_path: None,
                models: vec![],
                grpc_listen: None,
                http_listen: None,
            },
            &["univec".to_string()],
        )
        .unwrap();
        assert_eq!(
            activation(
                &embedded,
                &unset(&[
                    ("postvec.path", "/opt/postvec"),
                    ("postvec.embedded_models", ""),
                    ("postvec.embedded_listen", "127.0.0.1:33433"),
                    ("postvec.embedded_http_listen", "127.0.0.1:33434"),
                ])
            ),
            Activation::None,
            "an unset mode already *is* embedded"
        );

        let remote = desired_for(&snapshot(&[], None), &Ownership::Unmanaged, &["univec"]);
        assert_eq!(
            activation(
                &remote,
                &unset(&[
                    ("postvec.grpc_endpoints", "192.0.2.2:33333"),
                    ("postvec.http_endpoints", "https://192.0.2.2:22222",),
                ])
            ),
            Activation::Restart,
            "moving an unconfigured cluster to remote inference is a real change"
        );
    }

    #[test]
    fn preload_comparison_ignores_the_servers_own_rendering() {
        let desired = desired_for(&snapshot(&[], None), &Ownership::Unmanaged, &["univec"]);
        // The server may report the same list with different spacing.
        let spaced = snapshot(
            &[
                ("shared_preload_libraries", " postvec "),
                ("postvec.database", "univec"),
                ("postvec.mode", "grpc"),
                ("postvec.grpc_endpoints", "192.0.2.2:33333"),
                ("postvec.http_endpoints", "https://192.0.2.2:22222"),
            ],
            None,
        );
        assert_eq!(activation(&desired, &spaced), Activation::None);
    }

    /// After `setup --no-restart`, a rerun writes no file. Restart is gated
    /// on settings vs the running server, not on whether this run rewrote
    /// the snippet.
    #[test]
    fn a_rerun_after_a_deferred_restart_still_owes_the_restart() {
        let desired = desired_for(&snapshot(&[], None), &Ownership::Unmanaged, &["univec"]);
        // The file on disk already says all this; the running server does not.
        let not_yet_in_effect = snapshot(
            &[
                ("shared_preload_libraries", "pg_stat_statements"),
                ("postvec.database", ""),
            ],
            Some("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
        );
        assert_eq!(
            activation(&desired, &not_yet_in_effect),
            Activation::Restart
        );
    }

    /// A database whose installation failed must never reach
    /// `postvec.database`: its worker would fail to connect and be respawned
    /// every ~15 seconds, forever.
    #[test]
    fn a_failed_database_is_not_activated() {
        let snapshot = snapshot(&[], None);
        // Both were requested; only `univec` installed.
        let requested = desired_for(&snapshot, &Ownership::Unmanaged, &["analytics", "univec"]);
        assert_eq!(requested.databases, ["analytics", "univec"]);

        let installed = desired_for(&snapshot, &Ownership::Unmanaged, &["univec"]);
        assert_eq!(
            installed.databases,
            ["univec"],
            "the database that failed must not be configured"
        );
        assert!(!installed.render("0.1.0").unwrap().contains("analytics"));
    }

    /// A database that was already being served keeps being served even if this
    /// run failed to touch it — the command must not make things worse than it
    /// found them.
    #[test]
    fn an_already_configured_database_survives_a_failed_rerun() {
        let snapshot = snapshot(
            &[
                ("shared_preload_libraries", "postvec"),
                ("postvec.database", "analytics,univec"),
            ],
            Some("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
        );
        let ownership = Ownership::Managed {
            state: owned_state(&["analytics", "univec"], &["pg_stat_statements"]),
        };
        // Only `univec` installed on this run; `analytics` was already managed.
        let desired = desired_for(&snapshot, &ownership, &["univec"]);
        assert_eq!(desired.databases, ["analytics", "univec"]);
    }

    #[test]
    fn allow_unreachable_downgrades_only_reachability_failures() {
        let mut results = vec![
            CheckResult::fail("remote.grpc.connect", "inference", "unreachable").required(),
            CheckResult::fail("models.cache", "database:univec", "empty"),
            CheckResult::fail("cluster.preload", "cluster", "not preloaded"),
            CheckResult::fail("worker.heartbeat", "database:univec", "no heartbeat"),
            CheckResult::fail("extension.installed", "database:univec", "absent"),
        ];
        downgrade_reachability(&mut results);
        assert_eq!(results[0].status, CheckStatus::Warn);
        assert!(!results[0].required);
        assert!(results[0].summary.contains("--allow-unreachable"));
        assert_eq!(results[1].status, CheckStatus::Warn);
        assert_eq!(
            results[2].status,
            CheckStatus::Fail,
            "preload failures are never downgraded"
        );
        assert_eq!(
            results[3].status,
            CheckStatus::Fail,
            "a dead worker is never acceptable"
        );
        assert_eq!(results[4].status, CheckStatus::Fail);
    }

    #[test]
    fn embedded_inference_is_described_for_the_prompt() {
        let described = describe_inference(&InferenceSettings::Embedded(EmbeddedSettings::new(
            PathBuf::from("/opt/engine"),
            None,
            vec!["baai-bge-m3".into()],
            None,
            None,
        )));
        assert!(described.contains("/opt/engine"));
        assert!(described.contains("baai-bge-m3"));
        assert!(described.contains("127.0.0.1:33433"));

        let scan = describe_inference(&InferenceSettings::Embedded(EmbeddedSettings::new(
            PathBuf::from("/opt/engine"),
            None,
            vec![],
            None,
            None,
        )));
        assert!(scan.contains("every enabled model on disk"));
    }
}
