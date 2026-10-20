//! `postvec doctor` is read-only.
//!
//! The database handle cannot mutate. `postgres -C` does not touch the running
//! server. The host lock is shared and only if it already exists. No engine
//! is started.

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

    // A cluster that will not connect is still worth a report. Filesystem
    // reads and `postgres -C` often show why. `Context::open` stays strict
    // for every mutating command.
    let mut context = match Context::open(cli, output).await {
        Ok(context) => context,
        Err(error) => {
            return offline_report(cli, output, &args, error, started, started_at).await;
        }
    };

    // Shared, and only if the lock already exists. Doctor must not create
    // state. Without the lock the report says a concurrent change is possible.
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
    // A probe that cannot be built is a failing check, not a missing one
    // and not a reason to drop the rest of the report.
    let inference =
        match collect::inference_probe(&context, &snapshot.settings, args.tls, args.path.clone())
            .await
        {
            Ok(probe) => probe,
            Err(error) => collect::InferenceProbe::Unavailable {
                error: error.to_string(),
            },
        };

    // A URI may be paired with a local cluster. The report says whether they
    // are the same instance.
    let identity = context.prove_database_is_the_selected_cluster().await;

    // Registry reachability runs only under --deep and only observes.
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
        // Only where the CLI owns the configuration. Elsewhere no setup can
        // be running against this cluster.
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

/// Report what can be read without a connection.
///
/// Discovery is repeated because `Context::open` drops the cluster. It is
/// read-only. If discovery fails too, the original error is returned: there
/// is no identified cluster to report on.
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

    // Neutral wording. The failure may be on our side of the socket while
    // the server is up.
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
            // Both come from the running server, which is down. The binary
            // version is not a substitute.
            postgres_version: None,
            mode: None,
        },
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        summary: checks::Summary::of(&results),
        checks: results,
        // The cluster is down and no lock was taken, so no concurrent change.
        concurrent_change_possible: false,
    };
    output.show_report(&report)?;
    // `cluster.reachable` is a required failure, so this exits Failure.
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

    /// Exit codes automation depends on.
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
