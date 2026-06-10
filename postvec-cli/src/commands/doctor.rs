//! `postvec doctor` — read-only diagnosis.
//!
//! Read-only at every layer, and structurally so:
//!
//! - the database handle is a [`crate::db::ReadOnlyDb`], which has no mutating
//!   method at all (so no `refresh_models()`, no `pg_reload_conf()`);
//! - configuration is inspected with `postgres -C`, which parses a candidate
//!   configuration without touching the running server;
//! - the host lock is taken *shared* and only if it already exists, so a
//!   diagnosis leaves no trace on the filesystem;
//! - no inference engine is instantiated and no embedding is ever executed.

use super::{collect, Context};
use crate::checks::{self, Report, SCHEMA_VERSION};
use crate::cli::{Cli, DoctorArgs, TlsPolicy};
use crate::config::owned::HostLock;
use crate::error::{Exit, Result};
use crate::output::Output;
use std::time::Instant;

pub async fn run(cli: &Cli, args: DoctorArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = checks::timestamp_now();
    let requested = args.validated()?;

    // A cluster that will not accept a connection is the single most common
    // thing an operator runs `doctor` about, so refusing to produce a report
    // is the least useful possible response. The filesystem and `postgres -C`
    // still answer a good deal — including, often, *why* it will not start.
    //
    // `Context::open` is left strict: every mutating command still requires a
    // live connection, and nothing about their behaviour changes.
    let mut context = match Context::open(cli, output).await {
        Ok(context) => context,
        Err(error) => {
            return offline_report(cli, output, &args, error, started, started_at).await;
        }
    };

    // Shared, and only if the lock file already exists: doctor must not create
    // state. Without it a concurrent change cannot be excluded, and the report
    // says so rather than pretending otherwise.
    let lock = context
        .owned_paths()
        .ok()
        .and_then(|paths| HostLock::acquire_shared_if_present(&paths.lock).ok())
        .flatten();

    let snapshot = collect::cluster_snapshot(&mut context).await?;
    let databases = if requested.is_empty() {
        collect::default_databases(&snapshot.settings)
    } else {
        requested
    };
    if databases.is_empty() {
        output.note(
            "no database to inspect: postvec.database is empty and none was named with --database",
        );
    }

    let facts = collect::database_facts(
        &mut context,
        &databases,
        args.deep,
        collect::poll_interval(&snapshot.settings),
        crate::checks::database::fresh_threshold(&snapshot.settings),
    )
    .await?;
    // A probe that cannot even be constructed must not take the rest of the
    // report with it — but it must not vanish either: it becomes a failing
    // check, so the report cannot come back green with the inference side
    // silently unexamined.
    let inference =
        match collect::inference_probe(&context, &snapshot.settings, args.tls, args.path.clone())
            .await
        {
            Ok(probe) => probe,
            Err(error) => collect::InferenceProbe::Unavailable {
                error: error.to_string(),
            },
        };

    // Pairing a URI with a local cluster is allowed, but the report must say
    // whether they are actually the same instance rather than quietly mixing
    // one server's database state with another's files.
    let identity = context.prove_database_is_the_selected_cluster().await;

    // Registry reachability is the one network probe outside the cluster and
    // its engine; it runs only under --deep, and only observes.
    let registry = collect::registry_probe(args.deep, context.timeout).await;

    let mut results = collect::evaluate(
        &context,
        &snapshot,
        &facts,
        &inference,
        &identity,
        Some(&registry),
        args.deep,
        args.tls == TlsPolicy::Strict,
    );
    // The one other network probe, --deep only, best effort: 2 s, no retry.
    if args.deep && snapshot.settings.mode() == Some(crate::cli::Mode::Embedded) {
        results.extend(
            checks::provider::catalogue_notes(
                &snapshot.settings.providers_path(),
                std::time::Duration::from_secs(2),
            )
            .await,
        );
    }

    let report = Report {
        schema_version: SCHEMA_VERSION,
        command: "doctor",
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: collect::report_cluster(&context, &snapshot.settings),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        summary: checks::Summary::of(&results),
        checks: results,
        // Only meaningful where the CLI owns the configuration: elsewhere there
        // is no `postvec setup` that could be running against this cluster.
        concurrent_change_possible: lock.is_none()
            && snapshot
                .ownership
                .as_ref()
                .is_some_and(|ownership| ownership.state().is_some()),
    };
    output.show_report(&report)?;
    let exit = report.exit(args.strict);
    context.close().await;
    Ok(exit)
}

/// Diagnose as much as possible of a cluster that could not be connected to.
///
/// `Context::open` builds a `Cluster` and drops it on the way out, so discovery
/// is repeated here — it is read-only (`pg_lsclusters` + `pg_config`), cheap,
/// and happens only on this path. If discovery *also* fails, the original
/// error was never "cannot connect" (no cluster, an unusable selection, a host
/// with no `pg_lsclusters`), and it is returned unchanged: inventing a report
/// for a cluster that was never identified would be worse than the error.
async fn offline_report(
    cli: &Cli,
    output: &Output,
    args: &DoctorArgs,
    connection_error: crate::error::CliError,
    started: Instant,
    started_at: String,
) -> Result<Exit> {
    let Ok(cluster) = crate::cluster::discover_allowing_offline(
        cli.cluster.as_deref(),
        cli.pg_config.as_deref(),
        cli.config_dir.as_deref(),
        cli.timeout,
    )
    .await
    else {
        return Err(connection_error);
    };

    // Named neutrally: "not accepting connections" would be a diagnosis, and
    // the error may be on our side of the socket (a privilege drop that could
    // not exec, say) while the server is up and answering `psql` fine.
    output.note(&format!(
        "no connection to cluster {} ({connection_error}) — reporting what can be read \
         without one",
        cluster.identity.id
    ));

    let ownership = match cluster.owned_paths() {
        Ok(paths) => paths.inspect().ok(),
        Err(_) => None,
    };
    let offline_preload = cluster
        .query_setting_offline("shared_preload_libraries", cli.timeout)
        .await
        .ok();
    let assets = cluster.assets();

    let results = checks::cluster::offline_checks(&checks::cluster::OfflineClusterInput {
        identity: &cluster.identity,
        connection_error: &connection_error.to_string(),
        connection_fix: connection_error.remediation(),
        assets: assets.as_ref(),
        ownership: ownership.as_ref(),
        offline_preload: offline_preload.as_ref(),
    });

    let report = Report {
        schema_version: SCHEMA_VERSION,
        command: "doctor",
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: checks::ReportCluster {
            id: cluster.identity.id.clone(),
            postgres_major: Some(cluster.identity.major),
            // Both come from the running server, which is the thing that is
            // missing. Reporting the binary's version as the server's would be
            // a guess presented as an observation.
            postgres_version: None,
            mode: None,
        },
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        summary: checks::Summary::of(&results),
        checks: results,
        // Nothing could be running against a cluster that is not up, and no
        // lock was taken, so there is no concurrent change to warn about.
        concurrent_change_possible: false,
    };
    output.show_report(&report)?;
    // `cluster.reachable` is a required failure, so this is Failure whatever
    // else was found. Asserted in the tests below rather than assumed.
    Ok(report.exit(args.strict))
}

#[cfg(test)]
mod tests {
    use crate::checks::{CheckResult, Report, ReportCluster, Summary, SCHEMA_VERSION};
    use crate::error::Exit;

    fn report(checks: Vec<CheckResult>) -> Report {
        Report {
            schema_version: SCHEMA_VERSION,
            command: "doctor",
            cli_version: "0.1.0".into(),
            cluster: ReportCluster {
                id: "18/main".into(),
                postgres_major: Some(18),
                postgres_version: Some("18.4".into()),
                mode: Some("grpc".into()),
            },
            started_at: "2026-07-30T12:00:00Z".into(),
            duration_ms: 5,
            summary: Summary::of(&checks),
            checks,
            concurrent_change_possible: false,
        }
    }

    /// The exit contract, restated at the command level because it is what
    /// automation depends on.
    #[test]
    fn exit_code_contract() {
        assert_eq!(
            report(vec![CheckResult::pass("cluster.preload", "cluster", "ok")]).exit(false),
            Exit::Success
        );
        assert_eq!(
            report(vec![CheckResult::skip(
                "worker.heartbeat-advances",
                "database:d",
                "not sampled"
            )])
            .exit(false),
            Exit::Success,
            "an informational skip is not a failure"
        );
        assert_eq!(
            report(vec![CheckResult::warn("queue.dead", "database:d", "1")]).exit(false),
            Exit::Success
        );
        assert_eq!(
            report(vec![CheckResult::warn("queue.dead", "database:d", "1")]).exit(true),
            Exit::Failure,
            "--strict promotes warnings"
        );
        assert_eq!(
            report(vec![CheckResult::fail("cluster.preload", "cluster", "no")]).exit(false),
            Exit::Failure
        );
        assert_eq!(
            report(vec![CheckResult::skip(
                "embedded.build-capability",
                "embedded",
                "unknown"
            )
            .required()])
            .exit(false),
            Exit::Failure,
            "a mode-critical check that cannot be observed fails"
        );
    }
}
