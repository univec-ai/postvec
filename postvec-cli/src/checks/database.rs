//! Per-database checks: extension presence and versions, worker liveness,
//! queue health, model cache, registry integrity, migrations.
//!
//! Two judgement rules run through all of it:
//!
//! - **Queue depth is meaningless on its own.** Pending jobs with a live worker
//!   and a reachable engine are normal operation; the same depth with a dead
//!   worker, or with no reachable inference path, is a failure.
//! - **`worker_last_error` is prose, not a code.** It is reported as evidence
//!   and can raise a warning, but only structured checks (liveness,
//!   reachability, queue, catalog) decide FAIL.

use super::CheckResult;
#[cfg(test)]
use super::CheckStatus;
use crate::cli::Mode;
use crate::facts::{DatabaseFacts, SettingsSnapshot, SUPPORTED_DIAGNOSTICS_API};
use crate::validate;
use serde_json::json;
use std::time::Duration;

/// pgvector floor. `postvec.control` declares `requires = 'vector'` and the
/// extension's SQL uses 0.8 features.
pub const MINIMUM_VECTOR_VERSION: semver::Version = semver::Version::new(0, 8, 0);

pub struct DatabaseInput<'a> {
    pub facts: &'a DatabaseFacts,
    pub settings: &'a SettingsSnapshot,
    pub mode: Option<Mode>,
    /// Whether *some* inference discovery path is currently usable.
    /// `None` when it was not determined (nothing configured to probe, or
    /// probing was skipped).
    pub inference_reachable: Option<bool>,
    /// Whether `--deep` sampling was performed.
    pub deep: bool,
}

pub fn checks(input: &DatabaseInput<'_>) -> Vec<CheckResult> {
    let scope = input.facts.scope();
    let mut out = vec![exists(input, &scope)];
    if !input.facts.exists || input.facts.unreachable.is_some() {
        return out;
    }
    out.push(available(input, &scope));
    out.push(installed(input, &scope));
    if input.facts.postvec.is_none() {
        // Nothing else is observable, and that is a legitimate state: the
        // package can be installed long before any database uses it.
        return out;
    }
    out.push(version(input, &scope));
    out.push(build_info(input, &scope));
    out.push(vector_version(input, &scope));
    out.extend(worker_pid(input, &scope));
    out.push(heartbeat(input, &scope));
    out.extend(heartbeat_advances(input, &scope));
    out.push(worker_enabled(input, &scope));
    out.extend(worker_last_error(input, &scope));
    out.push(queue_pending(input, &scope));
    out.push(queue_dead(input, &scope));
    out.push(models_cache(input, &scope));
    out.extend(models_freshness(input, &scope));
    out.extend(registry_dependencies(input, &scope));
    out.extend(registry_indexes(input, &scope));
    out.extend(migrations_state(input, &scope));
    out
}

fn exists(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    if !input.facts.exists {
        return CheckResult::fail(
            "database.exists",
            scope,
            format!("database {:?} does not exist", input.facts.name),
        )
        .with_fix(
            "create it (or remove it from postvec.database): a configured but missing \
             database makes its worker fail to connect and respawn every ~15 seconds",
        );
    }
    match &input.facts.unreachable {
        Some(detail) => CheckResult::fail(
            "database.exists",
            scope,
            format!(
                "database {:?} exists but could not be inspected",
                input.facts.name
            ),
        )
        .with_evidence(json!({"detail": detail}))
        .with_fix("check connection privileges for this database"),
        None => CheckResult::pass(
            "database.exists",
            scope,
            format!(
                "database {:?} exists and accepts connections",
                input.facts.name
            ),
        ),
    }
}

fn available(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    match (
        input.facts.availability("postvec"),
        input.facts.availability("vector"),
    ) {
        (Some(postvec), Some(vector)) => CheckResult::pass(
            "extension.available",
            scope,
            format!(
                "postvec {} and vector {} are available to the server",
                postvec.default_version, vector.default_version
            ),
        ),
        (None, _) => CheckResult::fail(
            "extension.available",
            scope,
            "postvec is not available to this server",
        )
        .with_fix("install the postvec package for this PostgreSQL major"),
        (_, None) => CheckResult::fail(
            "extension.available",
            scope,
            "pgvector is not available to this server",
        )
        .with_fix("install pgvector >= 0.8 for this PostgreSQL major"),
    }
}

fn installed(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    match &input.facts.postvec {
        Some(extension) => CheckResult::pass(
            "extension.installed",
            scope,
            format!("postvec {} is installed", extension.catalog_version),
        ),
        None => CheckResult::fail(
            "extension.installed",
            scope,
            "the postvec extension is not installed in this database",
        )
        .with_fix(format!(
            "run `postvec setup --database {}`, or `CREATE EXTENSION postvec CASCADE` as a \
             superuser",
            input.facts.name
        )),
    }
}

fn version(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let extension = input.facts.postvec.as_ref().expect("checked by caller");
    if extension.catalog_version != extension.library_version {
        return CheckResult::fail(
            "extension.version",
            scope,
            format!(
                "installed SQL is {} but the loaded library is {}",
                extension.catalog_version, extension.library_version
            ),
        )
        .with_fix(
            "restart the cluster so the new library is loaded, then run \
             `ALTER EXTENSION postvec UPDATE` in this database",
        );
    }
    let api = extension
        .build_info
        .as_ref()
        .map(|info| info.diagnostics_api);
    match api {
        Some(api) if api > SUPPORTED_DIAGNOSTICS_API => CheckResult::warn(
            "extension.version",
            scope,
            format!(
                "postvec {} reports diagnostics API {api}, newer than this CLI understands ({})",
                extension.catalog_version, SUPPORTED_DIAGNOSTICS_API
            ),
        )
        .with_fix("upgrade postvec-cli for a complete report"),
        Some(api) => CheckResult::pass(
            "extension.version",
            scope,
            format!(
                "SQL {}, library {}, diagnostics API {api}",
                extension.catalog_version, extension.library_version
            ),
        ),
        None => CheckResult::pass(
            "extension.version",
            scope,
            format!(
                "SQL {}, library {}",
                extension.catalog_version, extension.library_version
            ),
        ),
    }
}

fn build_info(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let extension = input.facts.postvec.as_ref().expect("checked by caller");
    match &extension.build_info {
        Some(info) => CheckResult::pass(
            "extension.build-info",
            scope,
            format!(
                "build metadata present (embedded feature: {})",
                if info.embedded { "yes" } else { "no" }
            ),
        )
        .with_evidence(json!(info)),
        None => {
            let check = CheckResult::skip(
                "extension.build-info",
                scope,
                "this postvec version does not report build metadata",
            )
            .with_fix(
                "upgrade postvec to a version providing postvec.build_info(); until then the \
                 CLI cannot prove which build features are compiled in",
            );
            // Required only when embedded mode is selected: without build
            // metadata the CLI cannot prove the library can host an engine.
            if input.mode == Some(Mode::Embedded) {
                check.required()
            } else {
                check
            }
        }
    }
}

fn vector_version(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let Some(raw) = &input.facts.vector_version else {
        return CheckResult::fail(
            "extension.vector-version",
            scope,
            "pgvector is not installed in this database",
        )
        .with_fix("`CREATE EXTENSION vector` (postvec's CASCADE normally does this)");
    };
    match validate::parse_extension_version(raw) {
        Some(version) if version >= MINIMUM_VECTOR_VERSION => {
            CheckResult::pass("extension.vector-version", scope, format!("pgvector {raw}"))
        }
        Some(_) => CheckResult::fail(
            "extension.vector-version",
            scope,
            format!("pgvector {raw} is older than the required {MINIMUM_VECTOR_VERSION}"),
        )
        .with_fix("upgrade pgvector, then `ALTER EXTENSION vector UPDATE`"),
        None => CheckResult::warn(
            "extension.vector-version",
            scope,
            format!("pgvector reports version {raw:?}, which could not be compared"),
        ),
    }
}

/// Heartbeat freshness relative to the configured cadences. The worker writes
/// the heartbeat when its counters change and otherwise at most once per
/// `postvec.heartbeat_interval_ms`, so an idle worker's newest beat can be a
/// full liveness interval old and still be healthy; the poll multiple and
/// slack ride on top.
pub(crate) fn fresh_threshold(settings: &SettingsSnapshot) -> Duration {
    let poll = Duration::from_millis(settings.int("postvec.poll_interval_ms", 5000).max(0) as u64);
    let liveness =
        Duration::from_millis(settings.int("postvec.heartbeat_interval_ms", 30_000).max(0) as u64);
    (liveness + poll * 3 + Duration::from_secs(2)).max(Duration::from_secs(5))
}

fn worker_pid(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    let worker = input.facts.worker.as_ref()?;
    Some(match worker.pid {
        None => CheckResult::warn(
            "worker.pid",
            scope,
            "no worker has ever written a heartbeat in this database",
        )
        .with_fix(
            "check that postvec is preloaded, that this database is in postvec.database, and \
             that the cluster was restarted after those changes",
        ),
        Some(pid) if worker.pid_is_live => {
            CheckResult::pass("worker.pid", scope, format!("worker pid {pid} is running"))
        }
        Some(pid) if worker.predates_restart => CheckResult::warn(
            "worker.pid",
            scope,
            format!("the heartbeat (pid {pid}) predates the last PostgreSQL restart"),
        )
        .with_fix(
            "a preloaded worker beats within seconds of startup: rerun doctor, then check \
             shared_preload_libraries and the PostgreSQL log if this persists",
        ),
        Some(pid) => CheckResult::fail(
            "worker.pid",
            scope,
            format!("the recorded worker pid {pid} is not a running backend"),
        )
        .with_evidence(json!({"pid": pid}))
        .with_fix(
            "the heartbeat row is not cleared when a worker dies; check the PostgreSQL log for \
             why the worker exited",
        ),
    })
}

fn heartbeat(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let Some(worker) = &input.facts.worker else {
        return CheckResult::skip("worker.heartbeat", scope, "no heartbeat table to read");
    };
    let threshold = fresh_threshold(input.settings);
    match worker.last_beat_age_s {
        None => CheckResult::fail(
            "worker.heartbeat",
            scope,
            "no worker heartbeat: nothing is filling vectors in this database",
        )
        .with_evidence(json!({"threshold_s": threshold.as_secs_f64()}))
        .with_fix(
            "add postvec to shared_preload_libraries and this database to postvec.database, \
             then restart",
        ),
        Some(age) if age <= threshold.as_secs_f64() => CheckResult::pass(
            "worker.heartbeat",
            scope,
            format!(
                "last beat {age:.1}s ago{}",
                worker
                    .pid
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default()
            ),
        ),
        Some(age) => CheckResult::fail(
            "worker.heartbeat",
            scope,
            format!(
                "the last heartbeat is {age:.1}s old, more than the {:.0}s expected for a \
                 {}ms liveness interval and {}ms poll interval",
                threshold.as_secs_f64(),
                input.settings.int("postvec.heartbeat_interval_ms", 30_000),
                input.settings.int("postvec.poll_interval_ms", 5000)
            ),
        )
        .with_evidence(json!({
            "age_s": age,
            "threshold_s": threshold.as_secs_f64(),
            "pid": worker.pid,
        }))
        .with_fix(
            "the worker is stalled or gone; check the PostgreSQL log. The heartbeat row \
             persists after a worker dies, so its presence alone proves nothing",
        ),
    }
}

fn heartbeat_advances(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    if !input.deep {
        return None;
    }
    let worker = input.facts.worker.as_ref()?;
    Some(match worker.advanced {
        Some(true) => CheckResult::pass(
            "worker.heartbeat-advances",
            scope,
            "the heartbeat is current (heartbeats are written on change or once per \
             postvec.heartbeat_interval_ms)",
        ),
        Some(false) => CheckResult::fail(
            "worker.heartbeat-advances",
            scope,
            "the heartbeat is stale and did not advance between two samples",
        )
        .with_evidence(json!({
            "first_age_s": worker.last_beat_age_s,
            "second_age_s": worker.second_beat_age_s,
        }))
        .with_fix(
            "the worker process exists but is not making progress; check the PostgreSQL log \
             for a stuck transaction or lock wait",
        ),
        None => CheckResult::skip(
            "worker.heartbeat-advances",
            scope,
            "advancement was not sampled (the poll interval is longer than the sampling cap)",
        ),
    })
}

fn worker_enabled(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    match input.settings.bool("postvec.worker_enabled") {
        Some(false) => CheckResult::warn(
            "worker.enabled",
            scope,
            "postvec.worker_enabled is off: draining is intentionally paused",
        )
        .with_fix(
            "set postvec.worker_enabled = on and run SELECT pg_reload_conf() when you want \
             processing to resume",
        ),
        _ => CheckResult::pass("worker.enabled", scope, "job processing is enabled"),
    }
}

fn worker_last_error(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    let worker = input.facts.worker.as_ref()?;
    let error = worker
        .last_error
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    if error.is_empty() {
        return Some(CheckResult::pass(
            "worker.last-error",
            scope,
            "the worker reports no error",
        ));
    }
    // Deliberately not classified by parsing the text: the heartbeat stores
    // prose, not a stable code. Structured checks decide severity.
    Some(
        CheckResult::warn(
            "worker.last-error",
            scope,
            format!(
                "the worker's last recorded error: {}",
                truncate(&error, 160)
            ),
        )
        .with_evidence(json!({"errors": worker.errors, "last_error": error}))
        .with_fix(
            "this is the most recent error, not necessarily a current one; read it together \
             with the heartbeat, queue and endpoint checks",
        ),
    )
}

fn queue_pending(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let queue = input.facts.queue.clone().unwrap_or_default();
    let worker = input.facts.worker.clone().unwrap_or_default();
    let threshold = fresh_threshold(input.settings).as_secs_f64();
    let worker_alive = worker
        .last_beat_age_s
        .is_some_and(|age| age <= threshold && worker.pid_is_live);
    let evidence = json!({
        "pending": queue.pending,
        "claimed": queue.claimed,
        "oldest_pending_s": queue.oldest_pending_s,
    });

    if queue.pending == 0 && queue.claimed == 0 {
        return CheckResult::pass("queue.pending", scope, "the job queue is empty")
            .with_evidence(evidence);
    }
    let depth = format!(
        "{} pending, {} in flight{}",
        queue.pending,
        queue.claimed,
        queue
            .oldest_pending_s
            .map(|age| format!(
                ", oldest {}",
                humantime::format_duration(Duration::from_secs(age.max(0.0) as u64))
            ))
            .unwrap_or_default()
    );

    if input.settings.bool("postvec.worker_enabled") == Some(false) {
        return CheckResult::warn(
            "queue.pending",
            scope,
            format!("{depth}; draining is paused by postvec.worker_enabled"),
        )
        .with_evidence(evidence);
    }
    if !worker_alive {
        return CheckResult::fail(
            "queue.pending",
            scope,
            format!("{depth}, and no live worker is draining them"),
        )
        .with_evidence(evidence)
        .with_fix("fix the worker first (see the heartbeat check)");
    }
    if input.inference_reachable == Some(false) {
        return CheckResult::fail(
            "queue.pending",
            scope,
            format!("{depth}, and no inference endpoint is reachable"),
        )
        .with_evidence(evidence)
        .with_fix(
            "jobs accumulate without burning retries while inference is unreachable; fix the \
             endpoint or engine",
        );
    }
    CheckResult::pass(
        "queue.pending",
        scope,
        format!("{depth}; a live worker is draining"),
    )
    .with_evidence(evidence)
}

fn queue_dead(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let queue = input.facts.queue.clone().unwrap_or_default();
    if queue.dead == 0 {
        return CheckResult::pass("queue.dead", scope, "no dead-lettered jobs");
    }
    CheckResult::warn(
        "queue.dead",
        scope,
        format!(
            "{} dead-lettered job(s){}",
            queue.dead,
            queue
                .dead_reasons
                .first()
                .map(|reason| format!("; most recent reason: {}", truncate(reason, 120)))
                .unwrap_or_default()
        ),
    )
    .with_evidence(json!({"dead": queue.dead, "reasons": queue.dead_reasons}))
    .with_fix(
        "inspect postvec.jobs_dead; a permanent failure (for example TARGET_RESTRICTED) needs \
         a configuration change first, then re-drive with SELECT \
         postvec.retry_dead('schema.table', 'column')",
    )
}

fn models_cache(input: &DatabaseInput<'_>, scope: &str) -> CheckResult {
    let models = &input.facts.models;
    if models.count > 0 {
        return CheckResult::pass(
            "models.cache",
            scope,
            format!("{} model(s) cached", models.count),
        )
        .with_evidence(json!({"names": models.names}));
    }
    match input.inference_reachable {
        // The engine advertises models but none reached the cache: a real
        // problem, and the reason enable()/migrate() would refuse a model.
        Some(true) => CheckResult::fail(
            "models.cache",
            scope,
            "the model cache is empty although inference advertises models",
        )
        .with_fix(
            "run SELECT postvec.refresh_models() in this database; the worker also refreshes \
             on its own cadence once it is running",
        ),
        Some(false) => CheckResult::warn(
            "models.cache",
            scope,
            "the model cache is empty and no inference endpoint is reachable",
        )
        .with_fix("fix inference reachability; the cache fills from GET /config"),
        None => CheckResult::warn(
            "models.cache",
            scope,
            "the model cache is empty, so enable() and migrate() cannot resolve a model",
        )
        .with_fix("run SELECT postvec.refresh_models() once inference is reachable"),
    }
}

/// Cache freshness allows for a whole refresh cycle plus one discovery timeout.
fn models_freshness(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    let age = input.facts.models.newest_last_seen_s?;
    let refresh = Duration::from_millis(
        input
            .settings
            .int("postvec.model_refresh_interval_ms", 60_000) as u64,
    );
    let discovery =
        Duration::from_millis(input.settings.int("postvec.discovery_timeout_ms", 5_000) as u64);
    let threshold = (refresh * 2 + discovery).max(Duration::from_secs(120));
    Some(if age <= threshold.as_secs_f64() {
        CheckResult::pass(
            "models.cache-freshness",
            scope,
            format!("the model cache was refreshed {age:.0}s ago"),
        )
    } else {
        CheckResult::warn(
            "models.cache-freshness",
            scope,
            format!(
                "the model cache has not been refreshed for {age:.0}s (expected within {:.0}s)",
                threshold.as_secs_f64()
            ),
        )
        .with_evidence(json!({"age_s": age, "threshold_s": threshold.as_secs_f64()}))
        .with_fix(
            "the worker refreshes the cache on postvec.model_refresh_interval_ms; a stale \
             cache usually means the worker is not running or discovery is failing",
        )
    })
}

fn registry_dependencies(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    // An empty registry is healthy: the extension can be installed long before
    // postvec.enable() is called.
    if input.facts.registry.is_empty() {
        return Some(CheckResult::pass(
            "registry.dependencies",
            scope,
            "no columns are enabled yet",
        ));
    }
    let mut broken = Vec::new();
    for entry in &input.facts.registry {
        let mut problems: Vec<String> = Vec::new();
        if !entry.relation_exists {
            problems.push("the table is gone".to_string());
        } else {
            if !entry.source_column_exists {
                problems.push(format!("source column {} is gone", entry.source_column));
            }
            if !entry.vector_column_exists {
                problems.push(format!("vector column {} is gone", entry.vector_column));
            }
            if entry.trigger_count == 0 && entry.state == "active" {
                problems.push("its enqueue triggers are missing".to_string());
            }
        }
        if !problems.is_empty() {
            broken.push(json!({
                "relation": entry.relation,
                "source_column": entry.source_column,
                "problems": problems,
            }));
        }
    }
    Some(if broken.is_empty() {
        CheckResult::pass(
            "registry.dependencies",
            scope,
            format!(
                "{} enabled column(s), all with their table, columns and triggers intact",
                input.facts.registry.len()
            ),
        )
    } else {
        CheckResult::fail(
            "registry.dependencies",
            scope,
            format!(
                "{} registry entry/entries no longer match the schema",
                broken.len()
            ),
        )
        .with_evidence(json!({"entries": broken}))
        .with_fix(
            "run SELECT postvec.disable(<relation>, <column>) for entries whose objects were \
             dropped outside postvec",
        )
    })
}

fn registry_indexes(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    let active: Vec<&crate::facts::RegistryEntryFacts> = input
        .facts
        .registry
        .iter()
        .filter(|entry| entry.state == "active" && entry.relation_exists)
        .collect();
    if active.is_empty() {
        return None;
    }
    // Index classification, in descending severity. Readiness may come
    // through a user-built or a postvec-built index. The diagnosis never
    // implies that readiness transfers ownership of a user index to postvec.
    let mut parked: Vec<String> = Vec::new(); // auto build failed, retries stopped
    let mut wrong_opclass: Vec<String> = Vec::new(); // ANN index exists, none usable
    let mut manual_unindexed: Vec<String> = Vec::new(); // manual, nothing built
    let mut auto_pending: Vec<String> = Vec::new(); // auto, waiting for work to drain
    for entry in &active {
        let name = format!("{}.{}", entry.relation, entry.vector_column);
        if let Some(err) = &entry.index_error {
            parked.push(format!("{name} ({err})"));
        } else if entry.has_vector_index && !entry.has_expected_opclass_index {
            wrong_opclass.push(name);
        } else if !entry.has_vector_index && entry.index_mode == "auto" {
            auto_pending.push(name);
        } else if !entry.has_vector_index {
            manual_unindexed.push(name);
        }
    }
    let evidence = json!({
        "parked_auto_failures": parked,
        "wrong_opclass": wrong_opclass,
        "manual_unindexed": manual_unindexed,
        "auto_pending": auto_pending,
    });
    Some(if !parked.is_empty() {
        CheckResult::warn(
            "registry.indexes",
            scope,
            format!(
                "automatic index build failed and is parked for {}",
                parked.join(", ")
            ),
        )
        .with_evidence(evidence)
        .with_fix(
            "fix the recorded cause, then SELECT postvec.create_vector_index(<relation>, \
             <column>) — its readiness check clears the parked error; an equivalent index \
             you build yourself (postvec never claims it) satisfies it too",
        )
    } else if !wrong_opclass.is_empty() {
        CheckResult::warn(
            "registry.indexes",
            scope,
            format!(
                "{} has an ANN index, but no valid index uses the operator class search() \
                 expects for the entry's distance/dimension (vector_*_ops up to 2000 dims, \
                 halfvec_*_ops above)",
                wrong_opclass.join(", ")
            ),
        )
        .with_evidence(evidence)
        .with_fix(
            "SELECT postvec.create_vector_index(<relation>, <column>), or build an index \
             with the expected operator class yourself",
        )
    } else if !manual_unindexed.is_empty() {
        CheckResult::warn(
            "registry.indexes",
            scope,
            format!(
                "{} has no ANN index, so search() scans sequentially",
                manual_unindexed.join(", ")
            ),
        )
        .with_evidence(evidence)
        .with_fix(
            "SELECT postvec.create_vector_index(<relation>, <column>) once the backfill has \
             drained (index-after-load is faster), or declare index_mode => 'auto'",
        )
    } else if !auto_pending.is_empty() {
        CheckResult::pass(
            "registry.indexes",
            scope,
            format!(
                "{} auto entry/entries await their build (the worker builds after the \
                 entry's queue/migration/backfill work drains); the rest are indexed",
                auto_pending.len()
            ),
        )
        .with_evidence(evidence)
    } else {
        CheckResult::pass(
            "registry.indexes",
            scope,
            format!(
                "all {} active entry/entries are served by a usable ANN index \
                 (postvec- or operator-built)",
                active.len()
            ),
        )
    })
}

fn migrations_state(input: &DatabaseInput<'_>, scope: &str) -> Option<CheckResult> {
    if input.facts.migrations.is_empty() {
        return None;
    }
    let mut failed = Vec::new();
    let mut awaiting = Vec::new();
    let mut running = Vec::new();
    for migration in &input.facts.migrations {
        let entry = json!({
            "id": migration.id,
            "state": migration.state,
            "rows_done": migration.rows_done,
            "rows_total": migration.rows_total,
            "retry_failures": migration.retry_failures,
            "error": migration.error,
        });
        match migration.state.as_str() {
            "failed" => failed.push(entry),
            "awaiting_finalize" | "awaiting_index" => awaiting.push(entry),
            _ => running.push(entry),
        }
    }
    if !failed.is_empty() {
        return Some(
            CheckResult::fail(
                "migrations.state",
                scope,
                format!("{} migration(s) failed", failed.len()),
            )
            .with_evidence(json!({"failed": failed}))
            .with_fix(
                "read the error in postvec.migration_status(); \
                 postvec.migration_abort(<id>) leaves the old data untouched",
            ),
        );
    }
    if !awaiting.is_empty() {
        return Some(
            CheckResult::warn(
                "migrations.state",
                scope,
                format!(
                    "{} migration(s) are waiting for an operator step",
                    awaiting.len()
                ),
            )
            .with_evidence(json!({"awaiting": awaiting, "running": running}))
            .with_fix(
                "these are not stuck: run postvec.migration_status(<id>) and then \
                 postvec.migration_finalize(<id>) (for awaiting_index, build the suggested \
                 index first)",
            ),
        );
    }
    let stalled = running
        .iter()
        .filter(|entry| entry["retry_failures"].as_i64().unwrap_or(0) >= 3)
        .count();
    Some(if stalled == 0 {
        CheckResult::pass(
            "migrations.state",
            scope,
            format!("{} migration(s) in progress", running.len()),
        )
        .with_evidence(json!({"running": running}))
    } else {
        CheckResult::warn(
            "migrations.state",
            scope,
            format!("{stalled} migration(s) are retrying after repeated failures"),
        )
        .with_evidence(json!({"running": running}))
        .with_fix("check postvec.migration_status() for the recorded error")
    })
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let kept: String = value.chars().take(limit).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::*;

    fn settings(pairs: &[(&str, &str)]) -> SettingsSnapshot {
        SettingsSnapshot {
            rows: pairs
                .iter()
                .map(|(name, value)| SettingRow {
                    name: (*name).into(),
                    setting: (*value).into(),
                    context: "sighup".into(),
                    source: "configuration file".into(),
                    sourcefile: None,
                    sourceline: None,
                    pending_restart: false,
                })
                .collect(),
            file_rows: Vec::new(),
        }
    }

    fn healthy() -> DatabaseFacts {
        DatabaseFacts {
            name: "univec".into(),
            exists: true,
            unreachable: None,
            available: vec![
                ExtensionAvailability {
                    name: "postvec".into(),
                    default_version: "0.1.0".into(),
                    installed_version: Some("0.1.0".into()),
                },
                ExtensionAvailability {
                    name: "vector".into(),
                    default_version: "0.8.5".into(),
                    installed_version: Some("0.8.5".into()),
                },
            ],
            postvec: Some(ExtensionFacts {
                catalog_version: "0.1.0".into(),
                library_version: "0.1.0".into(),
                build_info: Some(BuildInfo {
                    version: "0.1.0".into(),
                    diagnostics_api: 1,
                    embedded: false,
                    model_backends: None,
                }),
            }),
            vector_version: Some("0.8.5".into()),
            worker: Some(WorkerFacts {
                pid: Some(18422),
                last_beat_age_s: Some(0.4),
                started_at: Some("2026-07-30 10:00:00+00".into()),
                last_error: None,
                errors: 0,
                jobs_embedded: 12,
                jobs_dead_lettered: 0,
                model_refreshes: 3,
                pid_is_live: true,
                predates_restart: false,
                second_beat_age_s: None,
                advanced: None,
            }),
            queue: Some(QueueFacts::default()),
            registry: Vec::new(),
            models: ModelCacheFacts {
                count: 4,
                names: vec!["baai-bge-m3".into()],
                newest_last_seen_s: Some(12.0),
            },
            migrations: Vec::new(),
        }
    }

    fn entry(active: bool, indexed: bool) -> RegistryEntryFacts {
        RegistryEntryFacts {
            registry_id: 1,
            relation: "public.docs".into(),
            source_column: "body".into(),
            vector_column: "body_semantic".into(),
            model: "baai-bge-m3".into(),
            space: None,
            dim: 1024,
            state: if active { "active" } else { "disabled" }.into(),
            pending_jobs: 0,
            dead_jobs: 0,
            has_vector_index: indexed,
            index_mode: "manual".into(),
            index_error: None,
            has_expected_opclass_index: indexed,
            last_error: None,
            relation_exists: true,
            source_column_exists: true,
            vector_column_exists: true,
            trigger_count: 3,
            destination: None,
        }
    }

    fn default_settings() -> SettingsSnapshot {
        settings(&[
            ("postvec.poll_interval_ms", "1000"),
            ("postvec.worker_enabled", "on"),
            ("postvec.model_refresh_interval_ms", "60000"),
            ("postvec.discovery_timeout_ms", "5000"),
        ])
    }

    fn run(facts: &DatabaseFacts, reachable: Option<bool>) -> Vec<CheckResult> {
        checks(&DatabaseInput {
            facts,
            settings: &default_settings(),
            mode: Some(Mode::Grpc),
            inference_reachable: reachable,
            deep: false,
        })
    }

    fn status(checks: &[CheckResult], id: &str) -> Option<CheckStatus> {
        checks.iter().find(|c| c.id == id).map(|c| c.status)
    }

    #[test]
    fn a_healthy_database_passes_everything() {
        let checks = run(&healthy(), Some(true));
        for check in &checks {
            assert_eq!(
                check.status,
                CheckStatus::Pass,
                "{} should pass: {}",
                check.id,
                check.summary
            );
        }
    }

    #[test]
    fn every_emitted_id_is_registered() {
        let mut facts = healthy();
        facts.registry = vec![entry(true, false)];
        facts.migrations = vec![MigrationFacts {
            id: 1,
            registry_id: 1,
            state: "running".into(),
            rows_done: 1,
            rows_total: 2,
            error: None,
            age_s: Some(1.0),
            retry_failures: 0,
        }];
        for check in run(&facts, Some(true)) {
            assert!(
                super::super::CHECK_ORDER.contains(&check.id),
                "{} is not in CHECK_ORDER",
                check.id
            );
        }
    }

    #[test]
    fn a_missing_database_stops_further_checks() {
        let checks = run(&DatabaseFacts::absent("gone"), None);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Fail);
        assert!(checks[0].remediation.clone().unwrap().contains("respawn"));
    }

    #[test]
    fn an_uninstalled_extension_is_a_failure_but_not_a_cascade_of_them() {
        let mut facts = healthy();
        facts.postvec = None;
        facts.worker = None;
        facts.queue = None;
        let checks = run(&facts, None);
        assert_eq!(
            status(&checks, "extension.installed"),
            Some(CheckStatus::Fail)
        );
        assert_eq!(status(&checks, "worker.heartbeat"), None);
        assert_eq!(status(&checks, "queue.pending"), None);
    }

    #[test]
    fn version_skew_between_sql_and_library_fails() {
        let mut facts = healthy();
        facts.postvec.as_mut().unwrap().library_version = "0.2.0".into();
        let checks = run(&facts, Some(true));
        let version = checks.iter().find(|c| c.id == "extension.version").unwrap();
        assert_eq!(version.status, CheckStatus::Fail);
        assert!(version
            .remediation
            .clone()
            .unwrap()
            .contains("ALTER EXTENSION"));
    }

    #[test]
    fn a_newer_diagnostics_api_warns_instead_of_failing() {
        let mut facts = healthy();
        facts.postvec.as_mut().unwrap().build_info = Some(BuildInfo {
            version: "0.9.0".into(),
            diagnostics_api: 99,
            embedded: false,
            model_backends: None,
        });
        assert_eq!(
            status(&run(&facts, Some(true)), "extension.version"),
            Some(CheckStatus::Warn)
        );
    }

    #[test]
    fn missing_build_info_is_informational_but_required_for_embedded_mode() {
        let mut facts = healthy();
        facts.postvec.as_mut().unwrap().build_info = None;

        let grpc = checks(&DatabaseInput {
            facts: &facts,
            settings: &default_settings(),
            mode: Some(Mode::Grpc),
            inference_reachable: Some(true),
            deep: false,
        });
        let info = grpc
            .iter()
            .find(|c| c.id == "extension.build-info")
            .unwrap();
        assert_eq!(info.status, CheckStatus::Skip);
        assert!(!info.required, "irrelevant in remote mode");
        assert!(!info.is_blocking());

        let embedded = checks(&DatabaseInput {
            facts: &facts,
            settings: &default_settings(),
            mode: Some(Mode::Embedded),
            inference_reachable: Some(true),
            deep: false,
        });
        let info = embedded
            .iter()
            .find(|c| c.id == "extension.build-info")
            .unwrap();
        assert!(info.required);
        assert!(
            info.is_blocking(),
            "embedded mode cannot be verified without build metadata"
        );
    }

    #[test]
    fn an_old_pgvector_fails_the_floor_check() {
        let mut facts = healthy();
        facts.vector_version = Some("0.7.4".into());
        assert_eq!(
            status(&run(&facts, Some(true)), "extension.vector-version"),
            Some(CheckStatus::Fail)
        );
        facts.vector_version = Some("0.8.0-1.pgdg22.04+1".into());
        assert_eq!(
            status(&run(&facts, Some(true)), "extension.vector-version"),
            Some(CheckStatus::Pass)
        );
    }

    #[test]
    fn a_stale_heartbeat_fails_and_a_dead_pid_fails_separately() {
        let mut facts = healthy();
        facts.worker.as_mut().unwrap().last_beat_age_s = Some(600.0);
        facts.worker.as_mut().unwrap().pid_is_live = false;
        let checks = run(&facts, Some(true));
        assert_eq!(status(&checks, "worker.heartbeat"), Some(CheckStatus::Fail));
        assert_eq!(status(&checks, "worker.pid"), Some(CheckStatus::Fail));
        let beat = checks.iter().find(|c| c.id == "worker.heartbeat").unwrap();
        assert!(
            beat.remediation.clone().unwrap().contains("persists"),
            "the message must explain that the row outlives the worker"
        );
    }

    #[test]
    fn heartbeat_freshness_scales_with_the_configured_cadences() {
        let mut facts = healthy();
        // Within the default 30s liveness interval: healthy even though it is
        // many poll ticks old — idle heartbeats are change-gated.
        facts.worker.as_mut().unwrap().last_beat_age_s = Some(20.0);
        assert_eq!(
            status(&run(&facts, Some(true)), "worker.heartbeat"),
            Some(CheckStatus::Pass)
        );
        // Past liveness + 3×poll + slack (default 30 + 15 + 2 = 47s): stale.
        facts.worker.as_mut().unwrap().last_beat_age_s = Some(60.0);
        assert_eq!(
            status(&run(&facts, Some(true)), "worker.heartbeat"),
            Some(CheckStatus::Fail)
        );
        // A larger liveness interval widens the budget.
        let slow = checks(&DatabaseInput {
            facts: &facts,
            settings: &settings(&[("postvec.heartbeat_interval_ms", "120000")]),
            mode: Some(Mode::Grpc),
            inference_reachable: Some(true),
            deep: false,
        });
        assert_eq!(status(&slow, "worker.heartbeat"), Some(CheckStatus::Pass));
    }

    #[test]
    fn advancement_is_only_reported_under_deep() {
        let mut facts = healthy();
        facts.worker.as_mut().unwrap().advanced = Some(false);
        assert_eq!(
            status(&run(&facts, Some(true)), "worker.heartbeat-advances"),
            None
        );

        let deep = checks(&DatabaseInput {
            facts: &facts,
            settings: &default_settings(),
            mode: Some(Mode::Grpc),
            inference_reachable: Some(true),
            deep: true,
        });
        assert_eq!(
            status(&deep, "worker.heartbeat-advances"),
            Some(CheckStatus::Fail)
        );
    }

    #[test]
    fn queue_depth_is_judged_in_context() {
        let mut facts = healthy();
        facts.queue = Some(QueueFacts {
            pending: 500,
            claimed: 64,
            dead: 0,
            oldest_pending_s: Some(30.0),
            dead_reasons: vec![],
        });

        // Live worker, reachable inference: normal operation.
        assert_eq!(
            status(&run(&facts, Some(true)), "queue.pending"),
            Some(CheckStatus::Pass)
        );

        // Unreachable inference: jobs will never drain.
        assert_eq!(
            status(&run(&facts, Some(false)), "queue.pending"),
            Some(CheckStatus::Fail)
        );

        // Paused on purpose: a warning, not a failure.
        let paused = checks(&DatabaseInput {
            facts: &facts,
            settings: &settings(&[("postvec.worker_enabled", "off")]),
            mode: Some(Mode::Grpc),
            inference_reachable: Some(true),
            deep: false,
        });
        assert_eq!(status(&paused, "queue.pending"), Some(CheckStatus::Warn));

        // Dead worker: failure regardless of inference.
        let mut dead_worker = facts.clone();
        dead_worker.worker.as_mut().unwrap().pid_is_live = false;
        assert_eq!(
            status(&run(&dead_worker, Some(true)), "queue.pending"),
            Some(CheckStatus::Fail)
        );
    }

    #[test]
    fn dead_letters_warn_even_when_the_live_queue_is_healthy() {
        let mut facts = healthy();
        facts.queue = Some(QueueFacts {
            pending: 0,
            claimed: 0,
            dead: 3,
            oldest_pending_s: None,
            dead_reasons: vec!["TARGET_RESTRICTED: cohere-embed-v4.0".into()],
        });
        let checks = run(&facts, Some(true));
        let dead = checks.iter().find(|c| c.id == "queue.dead").unwrap();
        assert_eq!(dead.status, CheckStatus::Warn);
        assert!(dead.summary.contains("TARGET_RESTRICTED"));
        assert_eq!(status(&checks, "queue.pending"), Some(CheckStatus::Pass));
    }

    #[test]
    fn a_worker_error_is_evidence_not_a_verdict() {
        let mut facts = healthy();
        facts.worker.as_mut().unwrap().last_error =
            Some("transport error: connection refused".into());
        let checks = run(&facts, Some(true));
        assert_eq!(
            status(&checks, "worker.last-error"),
            Some(CheckStatus::Warn),
            "prose is never parsed into a FAIL"
        );
        assert_eq!(status(&checks, "queue.pending"), Some(CheckStatus::Pass));
    }

    #[test]
    fn an_empty_cache_is_judged_against_what_inference_advertises() {
        let mut facts = healthy();
        facts.models = ModelCacheFacts::default();
        assert_eq!(
            status(&run(&facts, Some(true)), "models.cache"),
            Some(CheckStatus::Fail)
        );
        assert_eq!(
            status(&run(&facts, Some(false)), "models.cache"),
            Some(CheckStatus::Warn)
        );
        assert_eq!(
            status(&run(&facts, None), "models.cache"),
            Some(CheckStatus::Warn)
        );
    }

    #[test]
    fn cache_freshness_uses_the_refresh_cadence() {
        let mut facts = healthy();
        facts.models.newest_last_seen_s = Some(600.0);
        assert_eq!(
            status(&run(&facts, Some(true)), "models.cache-freshness"),
            Some(CheckStatus::Warn)
        );
        // A long refresh interval makes the same age acceptable.
        let slow = checks(&DatabaseInput {
            facts: &facts,
            settings: &settings(&[("postvec.model_refresh_interval_ms", "600000")]),
            mode: Some(Mode::Grpc),
            inference_reachable: Some(true),
            deep: false,
        });
        assert_eq!(
            status(&slow, "models.cache-freshness"),
            Some(CheckStatus::Pass)
        );
    }

    /// Doctor classification: parked auto failures, wrong-opclass
    /// indexes, manual missing indexes and auto-pending entries each get
    /// their own diagnosis. Readiness through a user index passes without
    /// implying ownership.
    #[test]
    fn index_check_classifies_p8_states() {
        // Parked auto failure outranks everything.
        let mut facts = healthy();
        let mut parked = entry(true, false);
        parked.index_mode = "auto".into();
        parked.index_error = Some("name collision".into());
        facts.registry = vec![parked];
        let checks = run(&facts, Some(true));
        let check = checks.iter().find(|c| c.id == "registry.indexes").unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(check.summary.contains("parked"), "{}", check.summary);

        // An ANN index exists, but none carries the expected opclass.
        let mut facts = healthy();
        let mut wrong = entry(true, true);
        wrong.has_expected_opclass_index = false;
        facts.registry = vec![wrong];
        let checks = run(&facts, Some(true));
        let check = checks.iter().find(|c| c.id == "registry.indexes").unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.summary.contains("operator class"),
            "{}",
            check.summary
        );

        // Auto entries still waiting for their build are informational.
        let mut facts = healthy();
        let mut pending = entry(true, false);
        pending.index_mode = "auto".into();
        facts.registry = vec![pending];
        let checks = run(&facts, Some(true));
        let check = checks.iter().find(|c| c.id == "registry.indexes").unwrap();
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.summary.contains("await"), "{}", check.summary);

        // Ready through any usable index (user- or postvec-built) passes.
        let mut facts = healthy();
        facts.registry = vec![entry(true, true)];
        let checks = run(&facts, Some(true));
        assert_eq!(status(&checks, "registry.indexes"), Some(CheckStatus::Pass));
    }

    #[test]
    fn an_empty_registry_is_healthy() {
        let checks = run(&healthy(), Some(true));
        assert_eq!(
            status(&checks, "registry.dependencies"),
            Some(CheckStatus::Pass)
        );
        assert_eq!(
            status(&checks, "registry.indexes"),
            None,
            "there is nothing to index"
        );
    }

    #[test]
    fn a_dropped_table_or_column_fails_the_dependency_check() {
        let mut facts = healthy();
        let mut broken = entry(true, true);
        broken.relation_exists = false;
        facts.registry = vec![broken];
        let checks = run(&facts, Some(true));
        let dependencies = checks
            .iter()
            .find(|c| c.id == "registry.dependencies")
            .unwrap();
        assert_eq!(dependencies.status, CheckStatus::Fail);
        assert!(format!("{:?}", dependencies.evidence).contains("table is gone"));

        let mut facts = healthy();
        let mut broken = entry(true, true);
        broken.vector_column_exists = false;
        broken.trigger_count = 0;
        facts.registry = vec![broken];
        let checks = run(&facts, Some(true));
        let evidence = format!(
            "{:?}",
            checks
                .iter()
                .find(|c| c.id == "registry.dependencies")
                .unwrap()
                .evidence
        );
        assert!(evidence.contains("body_semantic"));
        assert!(evidence.contains("triggers"));
    }

    #[test]
    fn a_missing_ann_index_warns_only_for_active_entries() {
        let mut facts = healthy();
        facts.registry = vec![entry(true, false)];
        let checks = run(&facts, Some(true));
        let indexes = checks.iter().find(|c| c.id == "registry.indexes").unwrap();
        assert_eq!(indexes.status, CheckStatus::Warn);
        assert!(indexes.summary.contains("public.docs.body_semantic"));

        facts.registry = vec![entry(false, false)];
        assert_eq!(
            status(&run(&facts, Some(true)), "registry.indexes"),
            None,
            "a disabled entry needs no index"
        );
    }

    #[test]
    fn migration_states_are_distinguished() {
        let migration = |state: &str, retries: i32| MigrationFacts {
            id: 7,
            registry_id: 1,
            state: state.into(),
            rows_done: 10,
            rows_total: 100,
            error: None,
            age_s: Some(30.0),
            retry_failures: retries,
        };
        let mut facts = healthy();

        facts.migrations = vec![migration("awaiting_index", 0)];
        let checks = run(&facts, Some(true));
        let state = checks.iter().find(|c| c.id == "migrations.state").unwrap();
        assert_eq!(state.status, CheckStatus::Warn);
        assert!(
            state.remediation.clone().unwrap().contains("not stuck"),
            "awaiting_index is expected, not broken"
        );

        facts.migrations = vec![migration("failed", 0)];
        assert_eq!(
            status(&run(&facts, Some(true)), "migrations.state"),
            Some(CheckStatus::Fail)
        );

        facts.migrations = vec![migration("running", 0)];
        assert_eq!(
            status(&run(&facts, Some(true)), "migrations.state"),
            Some(CheckStatus::Pass)
        );

        facts.migrations = vec![migration("running", 5)];
        assert_eq!(
            status(&run(&facts, Some(true)), "migrations.state"),
            Some(CheckStatus::Warn)
        );
    }

    #[test]
    fn long_messages_are_truncated_without_splitting_characters() {
        let long = "é".repeat(300);
        let truncated = truncate(&long, 10);
        assert_eq!(truncated.chars().count(), 11, "10 kept plus the ellipsis");
        assert_eq!(truncate("short", 10), "short");
    }
}
