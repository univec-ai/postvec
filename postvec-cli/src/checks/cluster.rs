//! Cluster-scope checks: connection, version, package assets, configuration
//! parsing and ownership, preload, mode, database list, worker slots.

use super::CheckResult;
#[cfg(test)]
use super::CheckStatus;
use crate::cluster::{is_supported_major, OfflineSetting, SUPPORTED_MAJORS};
use crate::config::guc;
use crate::config::owned::Ownership;
use crate::facts::{AssetFacts, ClusterIdentity, ServerFacts, SettingsSnapshot};
use crate::validate;
use serde_json::json;

const SCOPE: &str = "cluster";

/// Everything the cluster checks need, gathered by the caller.
pub struct ClusterInput<'a> {
    pub identity: &'a ClusterIdentity,
    /// The `pg_config` the directories were resolved from. Reported so a
    /// version mismatch names the installation that was inspected.
    pub pg_config: &'a std::path::Path,
    /// `pg_config --version`'s major, for the mismatch check.
    pub binary_major: u32,
    pub server: &'a ServerFacts,
    pub settings: &'a SettingsSnapshot,
    /// `None` when there is no local installation to inspect.
    pub assets: Option<&'a AssetFacts>,
    pub ownership: Option<&'a Ownership>,
    /// The offline `postgres -C shared_preload_libraries` answer, when it could
    /// be obtained.
    pub offline_preload: Option<&'a OfflineSetting>,
    /// Databases whose health is being inspected. Used to check that they are
    /// actually configured.
    pub inspected_databases: &'a [String],
    /// Other background workers the operator declared, if any. The CLI cannot
    /// know every extension's future demand.
    pub other_worker_slots: i64,
    /// Whether the inspected database connection is the instance running out of
    /// this cluster's data directory. Only meaningful when the two were paired
    /// explicitly (`--database-url` with `--cluster`/`--pg-config`).
    pub instance_identity: crate::commands::IdentityProof,
}

/// What can still be observed when the cluster will not accept a connection.
///
/// Every field here comes from the filesystem or from `postgres -C`, so all of
/// it is readable while the server is stopped — which is exactly when an
/// operator most needs it, because a bad preload list or a missing `.so` is a
/// common *reason* a cluster will not start.
pub struct OfflineClusterInput<'a> {
    pub identity: &'a ClusterIdentity,
    /// Why the connection failed, verbatim, and what to do about it.
    pub connection_error: &'a str,
    pub connection_fix: Option<&'a str>,
    pub assets: Option<&'a AssetFacts>,
    pub ownership: Option<&'a Ownership>,
    pub offline_preload: Option<&'a OfflineSetting>,
}

/// The report for a cluster that could not be connected to.
///
/// Two properties are load-bearing and are asserted in the tests below:
///
/// 1. `cluster.reachable` **fails**, so the run can never exit 0. A diagnostic
///    tool that returns green on a stopped server is worse than one that
///    refuses to run.
/// 2. Everything that needed the connection becomes a **required skip**, not a
///    pass. `SettingsSnapshot`'s accessors all return `None`/empty when a
///    setting is absent, so feeding the normal checks an empty snapshot would
///    have quietly reported "mode: not set", "no databases configured" and
///    similar as ordinary findings — indistinguishable from a cluster that is
///    genuinely misconfigured. Those checks are not run at all.
pub fn offline_checks(input: &OfflineClusterInput<'_>) -> Vec<CheckResult> {
    let mut unreachable = CheckResult::fail(
        "cluster.reachable",
        SCOPE,
        format!("cannot connect to {}", input.identity.id),
    )
    .required()
    .with_evidence(json!({ "error": input.connection_error }));
    if let Some(fix) = input.connection_fix {
        unreachable = unreachable.with_fix(fix);
    }

    vec![
        unreachable,
        assets_postvec(input.assets, input.identity),
        assets_vector(input.assets, input.identity),
        offline_config_parse(input.offline_preload),
    ]
    .into_iter()
    .chain(config_ownership(input.ownership))
    .chain(std::iter::once(
        // One honest placeholder for the whole live-only battery. Naming them
        // individually would imply each was examined.
        CheckResult::skip(
            "cluster.live-checks",
            SCOPE,
            "server version, preload state, mode, database list, worker slots, every \
             per-database check and the inference probe all need a connection",
        )
        .required()
        .with_fix("start the cluster and rerun `postvec doctor` for the full report"),
    ))
    .collect()
}

pub fn checks(input: &ClusterInput<'_>) -> Vec<CheckResult> {
    let mut out = vec![connection(input)];
    out.extend(identity(input));
    out.extend([
        version(input),
        assets_postvec(input.assets, input.identity),
        assets_vector(input.assets, input.identity),
        config_parse(input),
    ]);
    out.extend(config_ownership(input.ownership));
    out.push(preload(input));
    out.extend(preload_pending(input));
    out.extend(preload_shadowed(input));
    out.extend(mode(input));
    out.push(database_list(input));
    out.push(worker_slots(input));
    out
}

fn connection(input: &ClusterInput<'_>) -> CheckResult {
    CheckResult::pass(
        "cluster.connection",
        SCOPE,
        format!(
            "PostgreSQL {} on port {}",
            input.server.short_version(),
            input.server.port
        ),
    )
    .with_evidence(json!({
        "version": input.server.version,
        "version_num": input.server.version_num,
        "port": input.server.port,
        "postmaster_start_time": input.server.postmaster_start_time,
    }))
}

/// Whether a paired connection and cluster are actually the same instance.
///
/// `doctor` may pair them (`--database-url` with an explicit `--cluster`), and
/// a report that mixes one server's settings with another's package assets and
/// ownership state is worse than no report. It does not abort — every other
/// check still runs — but it says so, and blocks.
fn identity(input: &ClusterInput<'_>) -> Option<CheckResult> {
    use crate::commands::IdentityProof;
    match &input.instance_identity {
        // Nothing was paired, so there is nothing to disagree about.
        IdentityProof::NotApplicable => None,
        IdentityProof::Proven => Some(CheckResult::pass(
            "cluster.identity",
            SCOPE,
            "the supplied connection is this cluster's running instance",
        )),
        IdentityProof::Mismatch { detail } => Some(
            CheckResult::fail(
                "cluster.identity",
                SCOPE,
                format!("the supplied connection is not this cluster: {detail}"),
            )
            .required()
            .with_fix(
                "the findings below mix this connection's database state with the local \
                 host's files and configuration, and describe no single system; drop \
                 --database-url, or name the cluster that URI actually serves",
            ),
        ),
        IdentityProof::Unprovable { reason, fix } => Some(
            CheckResult::skip(
                "cluster.identity",
                SCOPE,
                format!("cannot tell whether the supplied connection is this cluster: {reason}"),
            )
            .required()
            .with_fix(fix.clone()),
        ),
    }
}

fn version(input: &ClusterInput<'_>) -> CheckResult {
    let major = input.server.major();
    if !is_supported_major(major) {
        return CheckResult::fail(
            "cluster.version",
            SCOPE,
            format!("PostgreSQL {major} is not supported by postvec"),
        )
        .with_evidence(json!({"server_major": major, "supported": SUPPORTED_MAJORS}))
        .with_fix("postvec builds for PostgreSQL 16, 17 and 18");
    }
    if major != input.binary_major {
        return CheckResult::fail(
            "cluster.version",
            SCOPE,
            format!(
                "the server is PostgreSQL {major} but the selected pg_config reports {}",
                input.binary_major
            ),
        )
        .with_evidence(json!({
            "server_major": major,
            "pg_config_major": input.binary_major,
            "pg_config": input.pg_config,
        }))
        .with_fix(
            "select the matching cluster with --cluster <major>/<name>, or the matching \
             installation with --pg-config",
        );
    }
    CheckResult::pass(
        "cluster.version",
        SCOPE,
        format!("PostgreSQL {major} matches the selected installation"),
    )
    .with_evidence(json!({"pg_config": input.pg_config}))
}

fn assets_postvec(assets: Option<&AssetFacts>, identity: &ClusterIdentity) -> CheckResult {
    let Some(assets) = assets else {
        return CheckResult::skip(
            "cluster.assets.postvec",
            SCOPE,
            "no local PostgreSQL installation to inspect for the package assets",
        );
    };
    let mut missing = Vec::new();
    if assets.postvec_control.is_none() {
        missing.push("postvec.control");
    }
    if assets.postvec_sql_versions.is_empty() {
        missing.push("postvec--<version>.sql");
    }
    if assets.postvec_library.is_none() {
        missing.push("the postvec shared library");
    }
    let evidence = json!({
        "sharedir": assets.sharedir,
        "pkglibdir": assets.pkglibdir,
        "sql_versions": assets.postvec_sql_versions,
        "library": assets.postvec_library,
    });
    if missing.is_empty() {
        CheckResult::pass(
            "cluster.assets.postvec",
            SCOPE,
            format!(
                "control file, {} and the library are installed for PostgreSQL {}",
                describe_versions(&assets.postvec_sql_versions),
                identity.major
            ),
        )
        .with_evidence(evidence)
    } else {
        CheckResult::fail(
            "cluster.assets.postvec",
            SCOPE,
            format!(
                "missing for PostgreSQL {}: {}",
                identity.major,
                missing.join(", ")
            ),
        )
        .with_evidence(evidence)
        .with_fix(format!(
            "install the postvec package built for PostgreSQL {} (one artifact per major)",
            identity.major
        ))
    }
}

fn describe_versions(versions: &[String]) -> String {
    match versions {
        [] => "no SQL script".to_string(),
        [one] => format!("SQL {one}"),
        many => format!("SQL {}", many.join("/")),
    }
}

fn assets_vector(assets: Option<&AssetFacts>, identity: &ClusterIdentity) -> CheckResult {
    let Some(assets) = assets else {
        return CheckResult::skip(
            "cluster.assets.vector",
            SCOPE,
            "no local PostgreSQL installation to inspect for pgvector's files",
        );
    };
    match (&assets.vector_control, &assets.vector_library) {
        (Some(_), Some(_)) => CheckResult::pass(
            "cluster.assets.vector",
            SCOPE,
            "pgvector is installed in this cluster",
        ),
        (Some(_), None) => CheckResult::warn(
            "cluster.assets.vector",
            SCOPE,
            "pgvector's control file is present but its library was not found where expected",
        )
        .with_evidence(json!({"pkglibdir": assets.pkglibdir}))
        .with_fix(
            "confirm the pgvector package matches this PostgreSQL major; \
             CREATE EXTENSION will fail if the library cannot be loaded",
        ),
        _ => CheckResult::fail(
            "cluster.assets.vector",
            SCOPE,
            "pgvector is not installed in this cluster",
        )
        .with_fix(format!(
            "install postgresql-{}-pgvector (>= 0.8); postvec declares requires = 'vector'",
            identity.major
        )),
    }
}

/// `cluster.config.parse` without a connection.
///
/// The online version starts from `pg_file_settings`, a live view. With the
/// server down there is only `postgres -C`, which parses the configuration a
/// restart *would* read — often the most valuable single fact when a cluster
/// will not come up.
///
/// The difference that matters: the online version treats "no live errors
/// reported" as a pass. Here, having observed nothing, "nothing was wrong" is
/// not a conclusion the code is entitled to draw, so an unobservable
/// configuration is a skip.
fn offline_config_parse(offline_preload: Option<&OfflineSetting>) -> CheckResult {
    match offline_preload {
        Some(OfflineSetting::Error(detail)) => CheckResult::fail(
            "cluster.config.parse",
            SCOPE,
            "the candidate configuration does not parse offline",
        )
        .with_evidence(json!({ "detail": detail }))
        .with_fix("fix the configuration before starting; the cluster would fail to start"),
        Some(OfflineSetting::NotObservable(why)) => CheckResult::skip(
            "cluster.config.parse",
            SCOPE,
            "the configuration could not be parsed offline",
        )
        .with_evidence(json!({ "reason": why }))
        .with_fix("rerun as root or as the cluster owner for a complete report"),
        Some(_) => CheckResult::pass(
            "cluster.config.parse",
            SCOPE,
            "the candidate configuration parses offline",
        ),
        None => CheckResult::skip(
            "cluster.config.parse",
            SCOPE,
            "the configuration was not parsed offline, so nothing is known about it",
        )
        .with_fix("rerun as root or as the cluster owner for a complete report"),
    }
}

fn config_parse(input: &ClusterInput<'_>) -> CheckResult {
    let errors = input.settings.file_errors();
    if !errors.is_empty() {
        let detail: Vec<String> = errors
            .iter()
            .take(5)
            .map(|row| {
                format!(
                    "{}:{} {}",
                    row.sourcefile,
                    row.sourceline,
                    row.error.as_deref().unwrap_or("error")
                )
            })
            .collect();
        return CheckResult::fail(
            "cluster.config.parse",
            SCOPE,
            format!("{} configuration line(s) have errors", errors.len()),
        )
        .with_evidence(json!({"errors": detail}))
        .with_fix("fix the lines listed above; the cluster will not start with them");
    }
    match input.offline_preload {
        Some(OfflineSetting::Error(detail)) => CheckResult::fail(
            "cluster.config.parse",
            SCOPE,
            "the candidate configuration does not parse offline",
        )
        .with_evidence(json!({"detail": detail}))
        .with_fix("fix the configuration before restarting; the cluster would fail to start"),
        Some(OfflineSetting::NotObservable(why)) => CheckResult::skip(
            "cluster.config.parse",
            SCOPE,
            "the configuration could not be parsed offline",
        )
        .with_evidence(json!({"reason": why}))
        .with_fix("rerun as root or as the cluster owner for a complete report"),
        None => CheckResult::pass(
            "cluster.config.parse",
            SCOPE,
            "the active configuration files have no reported errors",
        ),
        Some(_) => CheckResult::pass(
            "cluster.config.parse",
            SCOPE,
            "the active and candidate configurations both parse",
        ),
    }
}

fn config_ownership(ownership: Option<&Ownership>) -> Option<CheckResult> {
    let ownership = ownership?;
    Some(match ownership {
        Ownership::Unmanaged => CheckResult::pass(
            "cluster.config.ownership",
            SCOPE,
            "no CLI-owned configuration snippet (this cluster is configured by hand)",
        ),
        Ownership::Managed { state } => CheckResult::pass(
            "cluster.config.ownership",
            SCOPE,
            format!(
                "{} matches its recorded digest ({} managed database(s))",
                state.config_path.display(),
                state.managed_databases.len()
            ),
        )
        .with_evidence(json!({
            "config_path": state.config_path,
            "managed_databases": state.managed_databases,
            "preserved_databases": state.preserved_databases,
            "written_by": state.updated_by_cli_version,
            "updated_at": state.updated_at,
        })),
        drifted => CheckResult::warn(
            "cluster.config.ownership",
            SCOPE,
            drifted
                .drift_description()
                .unwrap_or_else(|| "configuration ownership is unclear".to_string()),
        )
        .with_fix(
            "postvec will not overwrite configuration it does not own; reconcile the file by \
             hand (or remove it) before rerunning setup",
        ),
    })
}

fn preload(input: &ClusterInput<'_>) -> CheckResult {
    let items = input.settings.preload_items();
    if guc::list_contains_postvec(&items) {
        CheckResult::pass(
            "cluster.preload",
            SCOPE,
            "postvec is an active shared_preload_libraries item",
        )
        .with_evidence(json!({"shared_preload_libraries": items}))
    } else {
        CheckResult::fail(
            "cluster.preload",
            SCOPE,
            "postvec is not in the active shared_preload_libraries",
        )
        .with_evidence(json!({"shared_preload_libraries": items}))
        .with_fix(
            "without the preload there is no background worker, so nothing fills vectors; \
             run `postvec setup` (it merges the list) and restart",
        )
    }
}

/// Compare the *offline* candidate value with the active one.
///
/// `pending_restart` alone is not enough: after `setup --no-restart` the server
/// has not read the new file at all, so it has nothing to mark pending.
fn preload_pending(input: &ClusterInput<'_>) -> Option<CheckResult> {
    let active = guc::render_library_list(&input.settings.preload_items());
    let pending_flag = input.settings.pending_restart("shared_preload_libraries");
    match input.offline_preload {
        Some(OfflineSetting::Value(candidate)) => {
            let candidate_items = guc::parse_library_list(candidate);
            let candidate_rendered = guc::render_library_list(&candidate_items);
            if candidate_rendered == active && !pending_flag {
                Some(CheckResult::pass(
                    "cluster.preload.pending",
                    SCOPE,
                    "the configured preload list is the one in effect",
                ))
            } else {
                Some(
                    CheckResult::fail(
                        "cluster.preload.pending",
                        SCOPE,
                        "the configured preload list differs from the one in effect",
                    )
                    .with_evidence(json!({
                        "active": active,
                        "configured": candidate_rendered,
                        "pending_restart": pending_flag,
                    }))
                    .with_fix("restart the cluster to apply it"),
                )
            }
        }
        Some(OfflineSetting::Unset) => Some(
            CheckResult::warn(
                "cluster.preload.pending",
                SCOPE,
                "shared_preload_libraries is not set in any configuration file",
            )
            .with_evidence(json!({"active": active})),
        ),
        // The parse failure is already reported by cluster.config.parse.
        Some(OfflineSetting::Error(_)) => None,
        Some(OfflineSetting::NotObservable(_)) | None => {
            if pending_flag {
                Some(
                    CheckResult::fail(
                        "cluster.preload.pending",
                        SCOPE,
                        "shared_preload_libraries has changed and needs a restart",
                    )
                    .with_fix("restart the cluster"),
                )
            } else {
                Some(CheckResult::skip(
                    "cluster.preload.pending",
                    SCOPE,
                    "the configured preload list could not be read offline for comparison",
                ))
            }
        }
    }
}

/// Another configuration file setting `shared_preload_libraries` with items the
/// effective value lacks.
///
/// This is the failure mode of owning the setting in a `conf.d` snippet: a later
/// edit to `postgresql.conf` is silently shadowed. `pg_file_settings` sees the
/// losing rows, so the CLI can name them.
fn preload_shadowed(input: &ClusterInput<'_>) -> Option<CheckResult> {
    let effective: Vec<String> = input.settings.preload_items();
    let shadowed: Vec<&crate::facts::FileSettingRow> = input
        .settings
        .file_rows
        .iter()
        .filter(|row| row.name == "shared_preload_libraries" && !row.applied)
        .collect();
    if shadowed.is_empty() {
        return None;
    }
    let mut lost: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    for row in &shadowed {
        sources.push(format!("{}:{}", row.sourcefile, row.sourceline));
        for item in guc::parse_library_list(&row.setting) {
            if !effective.contains(&item) && !lost.contains(&item) {
                lost.push(item);
            }
        }
    }
    if lost.is_empty() {
        return None;
    }
    Some(
        CheckResult::warn(
            "cluster.preload.shadowed",
            SCOPE,
            format!(
                "another configuration file preloads {} but is overridden",
                lost.join(", ")
            ),
        )
        .with_evidence(json!({"sources": sources, "libraries_lost": lost, "effective": effective}))
        .with_fix(
            "the CLI-owned conf.d snippet wins over postgresql.conf; rerun `postvec setup` so \
             the merged list includes these libraries again",
        ),
    )
}

fn mode(input: &ClusterInput<'_>) -> Option<CheckResult> {
    // Without the preload the postvec GUCs are not even defined, and
    // cluster.preload already says so.
    if !guc::list_contains_postvec(&input.settings.preload_items()) {
        return None;
    }
    match input.settings.mode() {
        Some(mode) => Some(CheckResult::pass(
            "cluster.mode",
            SCOPE,
            format!("inference mode is {mode}"),
        )),
        None => match input.settings.raw_mode() {
            Some(raw) => Some(
                CheckResult::fail(
                    "cluster.mode",
                    SCOPE,
                    format!("postvec.mode is {raw:?}, which the extension does not accept"),
                )
                .with_fix(
                    "set postvec.mode to 'grpc' or 'embedded' (the value is case-sensitive) \
                     and restart; the worker parks on an unknown mode",
                ),
            ),
            // Unset means the default, which is grpc.
            None => Some(CheckResult::pass(
                "cluster.mode",
                SCOPE,
                "inference mode is grpc (postvec.mode is unset, which is the default)",
            )),
        },
    }
}

fn database_list(input: &ClusterInput<'_>) -> CheckResult {
    let configured = input.settings.configured_databases();
    if configured.is_empty() {
        return CheckResult::fail(
            "cluster.database-list",
            SCOPE,
            "postvec.database is empty, so the launcher serves no database",
        )
        .with_fix(
            "set postvec.database to the database(s) to serve and restart; the launcher \
             otherwise idles forever",
        );
    }
    let missing: Vec<&String> = input
        .inspected_databases
        .iter()
        .filter(|name| !configured.contains(name))
        .collect();
    if !missing.is_empty() {
        return CheckResult::fail(
            "cluster.database-list",
            SCOPE,
            format!(
                "{} is not in postvec.database, so no worker serves it",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .with_evidence(json!({"configured": configured, "inspected": input.inspected_databases}))
        .with_fix("add it with `postvec setup --database <name>` and restart");
    }
    let unnameable: Vec<&String> = configured
        .iter()
        .filter(|name| validate::database_name(name).is_err())
        .collect();
    if !unnameable.is_empty() {
        return CheckResult::warn(
            "cluster.database-list",
            SCOPE,
            format!(
                "postvec.database contains {} unusable entry/entries",
                unnameable.len()
            ),
        )
        .with_evidence(json!({"configured": configured}))
        .with_fix("remove the malformed entries; the launcher would fail to connect for them");
    }
    CheckResult::pass(
        "cluster.database-list",
        SCOPE,
        format!(
            "the launcher serves {} database(s): {}",
            configured.len(),
            configured.join(", ")
        ),
    )
    .with_evidence(json!({"configured": configured}))
}

/// The launcher takes one `max_worker_processes` slot and each configured
/// database takes one more.
fn worker_slots(input: &ClusterInput<'_>) -> CheckResult {
    let max = input.settings.int("max_worker_processes", 8);
    let configured = input.settings.configured_databases().len() as i64;
    let needed = 1 + configured;
    let total_claim = needed + input.other_worker_slots;
    let evidence = json!({
        "max_worker_processes": max,
        "postvec_needs": needed,
        "other_declared": input.other_worker_slots,
    });
    if total_claim > max {
        return CheckResult::fail(
            "cluster.worker-slots",
            SCOPE,
            format!(
                "postvec needs {needed} background worker slot(s) (1 launcher + {configured} \
                 database worker(s)) but max_worker_processes is {max}"
            ),
        )
        .with_evidence(evidence)
        .with_fix(format!(
            "raise max_worker_processes to at least {} and restart",
            total_claim.max(needed)
        ));
    }
    // The CLI can prove insufficiency but cannot know every other extension's
    // future demand, so a tight-but-sufficient budget is a warning.
    if max - total_claim < 2 {
        return CheckResult::warn(
            "cluster.worker-slots",
            SCOPE,
            format!(
                "postvec needs {needed} of {max} background worker slots, leaving little room \
                 for other extensions"
            ),
        )
        .with_evidence(evidence)
        .with_fix("consider raising max_worker_processes");
    }
    CheckResult::pass(
        "cluster.worker-slots",
        SCOPE,
        format!("{needed} of {max} background worker slots are needed by postvec"),
    )
    .with_evidence(evidence)
}

#[cfg(test)]
// The fixtures below deliberately start from a healthy default and mutate the
// one field under test, which reads better than restating every field.
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::config::owned::{ClusterState, STATE_SCHEMA_VERSION};
    use crate::facts::{FileSettingRow, SettingRow};
    use std::path::PathBuf;

    fn server(major: u32) -> ServerFacts {
        ServerFacts {
            version_num: (major * 10_000 + 4) as i32,
            version: format!("{major}.4"),
            data_directory: Some("/var/lib/postgresql/x".into()),
            config_file: Some("/etc/postgresql/x/postgresql.conf".into()),
            hba_file: None,
            port: 5432,
            postmaster_start_time: "2026-07-30 10:00:00+00".into(),
            postmaster_start_exact: None,
            system_identifier: None,
        }
    }

    fn setting(name: &str, value: &str) -> SettingRow {
        SettingRow {
            name: name.into(),
            setting: value.into(),
            context: "postmaster".into(),
            source: "configuration file".into(),
            sourcefile: Some("/etc/postgresql/18/main/conf.d/99-postvec.conf".into()),
            sourceline: Some(3),
            pending_restart: false,
        }
    }

    fn snapshot(pairs: &[(&str, &str)]) -> SettingsSnapshot {
        SettingsSnapshot {
            rows: pairs.iter().map(|(n, v)| setting(n, v)).collect(),
            file_rows: Vec::new(),
        }
    }

    fn assets(complete: bool) -> AssetFacts {
        AssetFacts {
            sharedir: PathBuf::from("/usr/share/postgresql/18"),
            pkglibdir: PathBuf::from("/usr/lib/postgresql/18/lib"),
            postvec_control: complete.then(|| PathBuf::from("/x/postvec.control")),
            postvec_sql_versions: if complete {
                vec!["0.1.0".to_string()]
            } else {
                vec![]
            },
            postvec_library: complete.then(|| PathBuf::from("/x/postvec.so")),
            vector_control: Some(PathBuf::from("/x/vector.control")),
            vector_library: Some(PathBuf::from("/x/vector.so")),
        }
    }

    fn identity() -> ClusterIdentity {
        ClusterIdentity {
            id: "18/main".into(),
            major: 18,
            name: "main".into(),
        }
    }

    struct Fixture {
        server: ServerFacts,
        settings: SettingsSnapshot,
        assets: AssetFacts,
        offline: Option<OfflineSetting>,
        ownership: Option<Ownership>,
        databases: Vec<String>,
        binary_major: u32,
    }

    impl Default for Fixture {
        fn default() -> Self {
            Self {
                server: server(18),
                settings: snapshot(&[
                    ("shared_preload_libraries", "pg_stat_statements,postvec"),
                    ("postvec.database", "univec"),
                    ("postvec.mode", "grpc"),
                    ("max_worker_processes", "8"),
                ]),
                assets: assets(true),
                offline: Some(OfflineSetting::Value(
                    "pg_stat_statements,postvec".to_string(),
                )),
                ownership: None,
                databases: vec!["univec".to_string()],
                binary_major: 18,
            }
        }
    }

    impl Fixture {
        fn run(&self) -> Vec<CheckResult> {
            checks(&ClusterInput {
                identity: &identity(),
                pg_config: std::path::Path::new("/usr/lib/postgresql/18/bin/pg_config"),
                binary_major: self.binary_major,
                server: &self.server,
                settings: &self.settings,
                assets: Some(&self.assets),
                ownership: self.ownership.as_ref(),
                offline_preload: self.offline.as_ref(),
                inspected_databases: &self.databases,
                other_worker_slots: 0,
                instance_identity: crate::commands::IdentityProof::NotApplicable,
            })
        }
    }

    fn status_of(checks: &[CheckResult], id: &str) -> Option<CheckStatus> {
        checks.iter().find(|c| c.id == id).map(|c| c.status)
    }

    fn with_identity(proof: crate::commands::IdentityProof) -> Vec<CheckResult> {
        let fixture = Fixture::default();
        checks(&ClusterInput {
            identity: &identity(),
            pg_config: std::path::Path::new("/usr/lib/postgresql/18/bin/pg_config"),
            binary_major: 18,
            server: &fixture.server,
            settings: &fixture.settings,
            assets: Some(&fixture.assets),
            ownership: None,
            offline_preload: fixture.offline.as_ref(),
            inspected_databases: &fixture.databases,
            other_worker_slots: 0,
            instance_identity: proof,
        })
    }

    /// A report that pairs one server's database state with another's files
    /// describes no single system. It still runs every other check — the
    /// findings may be exactly what the operator needs — but it must not stay
    /// silent about the mixture.
    #[test]
    fn a_paired_connection_that_is_a_different_instance_blocks_the_report() {
        let checks = with_identity(crate::commands::IdentityProof::Mismatch {
            detail: "same replication lineage but a different running instance".into(),
        });
        let identity = checks.iter().find(|c| c.id == "cluster.identity").unwrap();
        assert_eq!(identity.status, CheckStatus::Fail);
        assert!(identity.is_blocking());
        assert!(identity.remediation.clone().unwrap().contains("mix"));
        // Everything else still ran.
        assert!(checks.len() > 5);
    }

    #[test]
    fn an_unprovable_pairing_is_a_required_skip() {
        let checks = with_identity(crate::commands::IdentityProof::Unprovable {
            reason: "the cluster is not running".into(),
            fix: "start it, or drop --database-url".into(),
        });
        let identity = checks.iter().find(|c| c.id == "cluster.identity").unwrap();
        assert_eq!(identity.status, CheckStatus::Skip);
        assert!(
            identity.is_blocking(),
            "an unverifiable pairing must not read as healthy"
        );
    }

    #[test]
    fn a_proven_pairing_passes_and_an_unpaired_run_says_nothing() {
        let proven = with_identity(crate::commands::IdentityProof::Proven);
        assert_eq!(
            proven
                .iter()
                .find(|c| c.id == "cluster.identity")
                .map(|c| c.status),
            Some(CheckStatus::Pass)
        );
        let unpaired = with_identity(crate::commands::IdentityProof::NotApplicable);
        assert!(
            !unpaired.iter().any(|c| c.id == "cluster.identity"),
            "with nothing paired there is nothing to report"
        );
    }

    #[test]
    fn a_healthy_cluster_passes_everything() {
        let checks = Fixture::default().run();
        for check in &checks {
            assert_eq!(
                check.status,
                CheckStatus::Pass,
                "{} should pass: {}",
                check.id,
                check.summary
            );
        }
        assert_eq!(
            status_of(&checks, "cluster.preload"),
            Some(CheckStatus::Pass)
        );
        assert_eq!(status_of(&checks, "cluster.mode"), Some(CheckStatus::Pass));
    }

    #[test]
    fn every_emitted_id_is_registered() {
        let fixture = Fixture {
            ownership: Some(Ownership::Unmanaged),
            ..Default::default()
        };
        for check in fixture.run() {
            assert!(
                super::super::CHECK_ORDER.contains(&check.id),
                "{} is not in CHECK_ORDER",
                check.id
            );
        }
    }

    #[test]
    fn a_missing_preload_fails_with_the_consequence_spelled_out() {
        let mut fixture = Fixture::default();
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "pg_stat_statements"),
            ("max_worker_processes", "8"),
        ]);
        fixture.offline = Some(OfflineSetting::Value("pg_stat_statements".to_string()));
        let checks = fixture.run();
        let preload = checks.iter().find(|c| c.id == "cluster.preload").unwrap();
        assert_eq!(preload.status, CheckStatus::Fail);
        assert!(preload
            .remediation
            .clone()
            .unwrap_or_default()
            .contains("setup"));
        // With no preload the postvec GUCs do not exist, so mode is not judged.
        assert_eq!(status_of(&checks, "cluster.mode"), None);
    }

    #[test]
    fn a_similarly_named_library_does_not_count_as_postvec() {
        let mut fixture = Fixture::default();
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "my_postvec_test"),
            ("max_worker_processes", "8"),
        ]);
        fixture.offline = Some(OfflineSetting::Value("my_postvec_test".to_string()));
        assert_eq!(
            status_of(&fixture.run(), "cluster.preload"),
            Some(CheckStatus::Fail)
        );
    }

    #[test]
    fn a_deferred_restart_is_detected_without_relying_on_pending_restart() {
        let mut fixture = Fixture::default();
        // Active value lacks postvec; the file on disk has it. This is exactly
        // the state `setup --no-restart` leaves, where the server has not read
        // the file and so has nothing marked pending.
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "pg_stat_statements"),
            ("max_worker_processes", "8"),
        ]);
        fixture.offline = Some(OfflineSetting::Value(
            "pg_stat_statements,postvec".to_string(),
        ));
        let checks = fixture.run();
        let pending = checks
            .iter()
            .find(|c| c.id == "cluster.preload.pending")
            .unwrap();
        assert_eq!(pending.status, CheckStatus::Fail);
        assert!(pending
            .remediation
            .clone()
            .unwrap_or_default()
            .contains("restart"));
    }

    #[test]
    fn an_unreadable_candidate_configuration_skips_rather_than_lies() {
        let mut fixture = Fixture::default();
        fixture.offline = Some(OfflineSetting::NotObservable("no permission".to_string()));
        let checks = fixture.run();
        assert_eq!(
            status_of(&checks, "cluster.config.parse"),
            Some(CheckStatus::Skip)
        );
        assert_eq!(
            status_of(&checks, "cluster.preload.pending"),
            Some(CheckStatus::Skip)
        );
    }

    #[test]
    fn a_broken_configuration_line_fails_the_parse_check() {
        let mut fixture = Fixture::default();
        fixture.settings.file_rows = vec![FileSettingRow {
            name: "postvec.database".into(),
            setting: "x".into(),
            sourcefile: "/etc/postgresql/18/main/conf.d/99-postvec.conf".into(),
            sourceline: 4,
            applied: false,
            error: Some("syntax error".into()),
        }];
        let checks = fixture.run();
        let parse = checks
            .iter()
            .find(|c| c.id == "cluster.config.parse")
            .unwrap();
        assert_eq!(parse.status, CheckStatus::Fail);
        assert!(format!("{:?}", parse.evidence).contains("syntax error"));
    }

    #[test]
    fn a_shadowed_preload_line_is_reported_with_its_source() {
        let mut fixture = Fixture::default();
        fixture.settings.file_rows = vec![
            FileSettingRow {
                name: "shared_preload_libraries".into(),
                setting: "pg_stat_statements,auto_explain".into(),
                sourcefile: "/etc/postgresql/18/main/postgresql.conf".into(),
                sourceline: 809,
                applied: false,
                error: None,
            },
            FileSettingRow {
                name: "shared_preload_libraries".into(),
                setting: "pg_stat_statements,postvec".into(),
                sourcefile: "/etc/postgresql/18/main/conf.d/99-postvec.conf".into(),
                sourceline: 3,
                applied: true,
                error: None,
            },
        ];
        let checks = fixture.run();
        let shadowed = checks
            .iter()
            .find(|c| c.id == "cluster.preload.shadowed")
            .unwrap();
        assert_eq!(shadowed.status, CheckStatus::Warn);
        let evidence = format!("{:?}", shadowed.evidence);
        assert!(evidence.contains("auto_explain"), "{evidence}");
        assert!(evidence.contains("postgresql.conf:809"), "{evidence}");
        assert!(
            !evidence.contains("\"pg_stat_statements\"]") || evidence.contains("auto_explain"),
            "only the libraries actually lost are named"
        );
    }

    #[test]
    fn a_shadowed_line_that_loses_nothing_is_not_reported() {
        let mut fixture = Fixture::default();
        fixture.settings.file_rows = vec![FileSettingRow {
            name: "shared_preload_libraries".into(),
            setting: "pg_stat_statements".into(),
            sourcefile: "/etc/postgresql/18/main/postgresql.conf".into(),
            sourceline: 809,
            applied: false,
            error: None,
        }];
        assert_eq!(
            status_of(&fixture.run(), "cluster.preload.shadowed"),
            None,
            "the overridden value is a subset of the effective one"
        );
    }

    #[test]
    fn an_uninspectable_database_list_is_a_failure_with_the_fix() {
        let mut fixture = Fixture::default();
        fixture.databases = vec!["analytics".to_string()];
        let checks = fixture.run();
        let list = checks
            .iter()
            .find(|c| c.id == "cluster.database-list")
            .unwrap();
        assert_eq!(list.status, CheckStatus::Fail);
        assert!(list.summary.contains("analytics"));
    }

    #[test]
    fn an_empty_database_list_is_a_failure() {
        let mut fixture = Fixture::default();
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "postvec"),
            ("postvec.database", ""),
            ("max_worker_processes", "8"),
        ]);
        fixture.databases = vec![];
        fixture.offline = Some(OfflineSetting::Value("postvec".to_string()));
        let checks = fixture.run();
        let list = checks
            .iter()
            .find(|c| c.id == "cluster.database-list")
            .unwrap();
        assert_eq!(list.status, CheckStatus::Fail);
        assert!(list.summary.contains("empty"));
    }

    #[test]
    fn an_unknown_mode_value_fails_because_the_worker_would_park() {
        let mut fixture = Fixture::default();
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "postvec"),
            ("postvec.database", "univec"),
            ("postvec.mode", "Embedded"),
            ("max_worker_processes", "8"),
        ]);
        fixture.offline = Some(OfflineSetting::Value("postvec".to_string()));
        let checks = fixture.run();
        let mode = checks.iter().find(|c| c.id == "cluster.mode").unwrap();
        assert_eq!(mode.status, CheckStatus::Fail);
        assert!(mode.summary.contains("Embedded"));
    }

    #[test]
    fn worker_slots_scale_with_the_database_count() {
        let mut fixture = Fixture::default();
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "postvec"),
            ("postvec.database", "a,b,c,d,e,f,g,h,i,j"),
            ("max_worker_processes", "8"),
        ]);
        fixture.offline = Some(OfflineSetting::Value("postvec".to_string()));
        fixture.databases = vec![];
        let checks = fixture.run();
        let slots = checks
            .iter()
            .find(|c| c.id == "cluster.worker-slots")
            .unwrap();
        assert_eq!(slots.status, CheckStatus::Fail);
        assert!(slots.summary.contains("11 background worker slot"));

        // A tight but sufficient budget warns rather than fails.
        fixture.settings = snapshot(&[
            ("shared_preload_libraries", "postvec"),
            ("postvec.database", "a,b,c,d,e,f,g"),
            ("max_worker_processes", "8"),
        ]);
        assert_eq!(
            status_of(&fixture.run(), "cluster.worker-slots"),
            Some(CheckStatus::Warn)
        );
    }

    /// With only a connection URI there is no installation to look at, and
    /// reporting the package as missing would be wrong rather than cautious.
    #[test]
    fn an_unobservable_host_skips_the_asset_checks() {
        let fixture = Fixture::default();
        let checks = checks(&ClusterInput {
            identity: &identity(),
            pg_config: std::path::Path::new(""),
            binary_major: 18,
            server: &fixture.server,
            settings: &fixture.settings,
            assets: None,
            ownership: None,
            offline_preload: Some(&OfflineSetting::NotObservable("no local install".into())),
            inspected_databases: &fixture.databases,
            other_worker_slots: 0,
            instance_identity: crate::commands::IdentityProof::NotApplicable,
        });
        assert_eq!(
            status_of(&checks, "cluster.assets.postvec"),
            Some(CheckStatus::Skip)
        );
        assert_eq!(
            status_of(&checks, "cluster.assets.vector"),
            Some(CheckStatus::Skip)
        );
        // Everything the server itself can answer still runs.
        assert_eq!(
            status_of(&checks, "cluster.preload"),
            Some(CheckStatus::Pass)
        );
        assert_eq!(
            status_of(&checks, "cluster.database-list"),
            Some(CheckStatus::Pass)
        );
        assert!(
            !checks.iter().any(|check| check.is_blocking()),
            "an unobservable host must not manufacture failures"
        );
    }

    #[test]
    fn missing_assets_name_the_package_to_install() {
        let mut fixture = Fixture::default();
        fixture.assets = assets(false);
        let checks = fixture.run();
        let postvec = checks
            .iter()
            .find(|c| c.id == "cluster.assets.postvec")
            .unwrap();
        assert_eq!(postvec.status, CheckStatus::Fail);
        assert!(postvec
            .remediation
            .clone()
            .unwrap()
            .contains("PostgreSQL 18"));
    }

    #[test]
    fn missing_pgvector_fails_and_names_the_package() {
        let mut fixture = Fixture::default();
        fixture.assets.vector_control = None;
        fixture.assets.vector_library = None;
        let checks = fixture.run();
        let vector = checks
            .iter()
            .find(|c| c.id == "cluster.assets.vector")
            .unwrap();
        assert_eq!(vector.status, CheckStatus::Fail);
        assert!(vector
            .remediation
            .clone()
            .unwrap()
            .contains("postgresql-18-pgvector"));
    }

    #[test]
    fn a_version_mismatch_between_server_and_pg_config_fails() {
        let mut fixture = Fixture::default();
        fixture.binary_major = 17;
        let checks = fixture.run();
        let version = checks.iter().find(|c| c.id == "cluster.version").unwrap();
        assert_eq!(version.status, CheckStatus::Fail);
        assert!(version.summary.contains("17"));
    }

    #[test]
    fn an_unsupported_server_major_fails() {
        let mut fixture = Fixture::default();
        fixture.server = server(15);
        fixture.binary_major = 15;
        assert_eq!(
            status_of(&fixture.run(), "cluster.version"),
            Some(CheckStatus::Fail)
        );
    }

    #[test]
    fn ownership_drift_warns_and_refuses_automatic_repair() {
        let state = ClusterState {
            schema_version: STATE_SCHEMA_VERSION,
            cluster: "18/main".into(),
            config_path: PathBuf::from("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
            config_sha256: "aaa".into(),
            managed_databases: vec!["univec".into()],
            preserved_databases: vec![],
            preload_was_already_present: false,
            preload_base: vec!["pg_stat_statements".into()],
            mode: "grpc".into(),
            updated_by_cli_version: "0.1.0".into(),
            updated_at: "2026-07-30T00:00:00Z".into(),
        };
        let mut fixture = Fixture::default();
        fixture.ownership = Some(Ownership::Modified {
            state: state.clone(),
            actual_sha256: "bbb".into(),
        });
        let checks = fixture.run();
        let ownership = checks
            .iter()
            .find(|c| c.id == "cluster.config.ownership")
            .unwrap();
        assert_eq!(ownership.status, CheckStatus::Warn);
        assert!(ownership
            .remediation
            .clone()
            .unwrap()
            .contains("not overwrite"));

        fixture.ownership = Some(Ownership::Managed { state });
        assert_eq!(
            status_of(&fixture.run(), "cluster.config.ownership"),
            Some(CheckStatus::Pass)
        );
    }

    // ---- the offline (unreachable cluster) report -------------------------

    fn offline(assets: Option<&AssetFacts>, preload: Option<&OfflineSetting>) -> Vec<CheckResult> {
        let id = identity();
        offline_checks(&OfflineClusterInput {
            identity: &id,
            connection_error: "cannot connect to database \"template1\"",
            connection_fix: Some("start it with `sudo pg_ctlcluster 18 main start`"),
            assets,
            ownership: None,
            offline_preload: preload,
        })
    }

    /// The property the whole design rests on: a stopped cluster can never
    /// produce a green report.
    #[test]
    fn an_unreachable_cluster_always_fails() {
        let complete = assets(true);
        let ok = OfflineSetting::Value("postvec".into());
        // Even with every observable thing healthy, the run fails.
        let results = offline(Some(&complete), Some(&ok));
        assert_eq!(
            status_of(&results, "cluster.reachable"),
            Some(CheckStatus::Fail)
        );
        let summary = crate::checks::Summary::of(&results);
        assert!(summary.fail >= 1, "{summary:?}");
    }

    /// The unreachable check carries the driver's own error and the fix, so
    /// the report says *why* rather than only *that*.
    #[test]
    fn the_failure_carries_the_cause_and_the_fix() {
        let results = offline(None, None);
        let reachable = results
            .iter()
            .find(|c| c.id == "cluster.reachable")
            .expect("cluster.reachable is always emitted");
        assert!(reachable.required, "it must be able to fail the run");
        assert!(reachable
            .evidence
            .as_ref()
            .unwrap()
            .to_string()
            .contains("template1"));
        assert!(reachable
            .remediation
            .as_ref()
            .unwrap()
            .contains("pg_ctlcluster"));
    }

    /// Everything needing a connection is one honest required skip — never a
    /// pass, and never silently absent.
    #[test]
    fn live_only_checks_become_a_required_skip() {
        let results = offline(None, None);
        let live = results
            .iter()
            .find(|c| c.id == "cluster.live-checks")
            .expect("the live battery must be accounted for");
        assert_eq!(live.status, CheckStatus::Skip);
        assert!(live.required);

        // The live-only checks must not appear at all: naming them would
        // imply each was examined.
        for id in [
            "cluster.connection",
            "cluster.preload",
            "cluster.mode",
            "cluster.databases",
            "cluster.worker-slots",
        ] {
            assert!(
                !results.iter().any(|c| c.id == id),
                "{id} cannot be evaluated without a connection, so it must not be reported"
            );
        }
    }

    /// The offline report still says something useful: package assets are
    /// filesystem facts, and a missing library is a very likely reason the
    /// cluster will not start.
    #[test]
    fn package_assets_are_still_diagnosed() {
        let missing = assets(false);
        let results = offline(Some(&missing), None);
        assert_eq!(
            status_of(&results, "cluster.assets.postvec"),
            Some(CheckStatus::Fail)
        );
        let complete = assets(true);
        assert_eq!(
            status_of(&offline(Some(&complete), None), "cluster.assets.postvec"),
            Some(CheckStatus::Pass)
        );
    }

    /// An unparseable candidate configuration is exactly what an operator
    /// needs to see when the server will not come up.
    #[test]
    fn an_offline_config_error_is_reported() {
        let broken = OfflineSetting::Error("syntax error at line 4".into());
        assert_eq!(
            status_of(&offline(None, Some(&broken)), "cluster.config.parse"),
            Some(CheckStatus::Fail)
        );
    }

    /// Having observed nothing, "nothing was wrong" is not a conclusion the
    /// code may draw — the online version's `None => pass` would have been a
    /// vacuous pass here.
    #[test]
    fn an_unobserved_configuration_is_a_skip_not_a_pass() {
        assert_eq!(
            status_of(&offline(None, None), "cluster.config.parse"),
            Some(CheckStatus::Skip)
        );
    }
}
