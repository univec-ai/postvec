//! Opt-in automatic vector-index creation after entry work drains.
//!
//! One bounded reconciliation step around the existing ownership-safe index
//! builder — no new worker, no daemon, no autonomous connection. The worker
//! calls [`step`] at most once per wake, after the normal jobs/migration/
//! backfill pass yields no work, and **outside** the inference-endpoint gate:
//! an observed entry with no queue work may still be indexed.
//!
//! The build is an ordinary (write-blocking) `CREATE INDEX` — PostgreSQL
//! forbids `CREATE INDEX CONCURRENTLY` inside the worker's SPI transaction,
//! which is exactly why `auto` is opt-in and never the default, and why the
//! documented manual `CREATE INDEX CONCURRENTLY` path remains the choice for
//! large or write-heavy tables. The build also occupies the database's one
//! postvec worker for its duration.
//!
//! Failure policy: the failed build's transaction aborts (nothing half-done);
//! a **fresh** guarded transaction reloads the entry and rechecks readiness —
//! a concurrent manual build may have won, in which case no error is stored —
//! and otherwise parks the entry by setting `registry.index_error`. A parked
//! entry is never retried automatically; the operator's repair path is
//! `create_vector_index()`, whose readiness-first wrapper clears the error.

use crate::registry::RegistryEntry;
use crate::worker::{try_transaction, Counters};
use pgrx::prelude::*;

/// Registry ids whose entries are candidates for one automatic build, in id
/// order. An entry is eligible only when it is active with `index_mode =
/// 'auto'`, has no parked `index_error`, no live migration, is not in cursor
/// backfill, has **no pending or claimed job** (queue drain has no separate
/// terminal marker — "no job for this entry" is the drain condition), and its
/// dimension permits a plain HNSW build. Readiness is checked separately, per
/// entry, through the shared opclass-aware predicate.
pub fn eligible_entries() -> Vec<i64> {
    Spi::connect(|c| {
        let t = c
            .select(
                "SELECT r.id FROM postvec.registry r
                  WHERE r.state = 'active'
                    AND r.index_mode = 'auto'
                    AND r.index_error IS NULL
                    AND r.dim <= 2000
                    AND r.backfill_mode <> 'cursor'
                    AND NOT EXISTS (SELECT 1 FROM postvec.jobs j
                                     WHERE j.registry_id = r.id)
                    -- every NON-terminal migration state gates the build:
                    -- 'awaiting_index' in particular has an active registry
                    -- row but explicitly awaits the manual reindex workflow,
                    -- and 'failed' awaits an operator abort — an implicit
                    -- blocking build must not preempt either.
                    AND NOT EXISTS (SELECT 1 FROM postvec.migrations m
                                     WHERE m.registry_id = r.id
                                       AND m.state NOT IN ('done','aborted'))
                  ORDER BY r.id",
                None,
                &[],
            )
            .expect("postvec: auto-index registry scan failed");
        t.into_iter()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
    })
}

/// Read-only, catalog-only relation-identity check. `missing_dependency()` alone only
/// proves the TRUNCATE sentinel for *observed* entries — a synced table
/// dropped and recreated under the same name (with matching column names,
/// vector column included) passes it while its `postvec_ins/upd/trunc_<id>`
/// triggers are gone, and building there would extension-stamp an index onto
/// an unrelated replacement relation. `triggers_missing()` is the identity
/// check for both modes.
fn identity_broken(entry: &RegistryEntry) -> Option<String> {
    if let Some(reason) = entry.missing_dependency(&[]) {
        return Some(reason);
    }
    if entry.triggers_missing() {
        return Some("its generated triggers are gone (table recreated?)".to_string());
    }
    None
}

/// The same check, taking the entry out of service when it fails. Only the
/// **candidate scan** does this: catalog validation retains no relation lock,
/// so quarantine can take its lifecycle locks in the normal order. The build path deliberately
/// only declines (see [`build_candidate`]) — quarantining there would need an
/// ACCESS EXCLUSIVE upgrade while already holding the much stronger SHARE
/// build lock, which is its own deadlock source.
fn quarantine_if_broken(entry: &RegistryEntry) -> bool {
    match identity_broken(entry) {
        Some(reason) => {
            crate::api::registry::quarantine_entry(entry, &reason);
            true
        }
        None => false,
    }
}

/// Identity reconciliation for **every** active auto entry, deliberately
/// independent of build eligibility — it ignores `index_error`, queue depth,
/// migrations, cursor backfill and dimension.
///
/// The independence is the point. A parked entry leaves
/// [`eligible_entries`], so if identity were only checked there, a wrongly
/// parked entry could never be reconciled. That is reachable: an *uncommitted*
/// `DROP TABLE` blocks the build until its `lock_timeout` fires, and the fresh
/// failure transaction still sees the pre-DROP catalog — so it parks a
/// perfectly healthy-looking entry moments before the DROP commits. Checking
/// identity first means the next scan quarantines it anyway.
///
/// Runs before any filtering, in the scan transaction, before any registry
/// row lock; the validation/quarantine path therefore preserves the
/// source -> destination -> registry order [`build_candidate`] documents.
fn quarantine_broken_entries() {
    let ids: Vec<i64> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT id FROM postvec.registry
                  WHERE state = 'active' AND index_mode = 'auto'
                  ORDER BY id",
                None,
                &[],
            )
            .expect("postvec: auto-index identity scan failed");
        t.into_iter()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
    });
    for id in ids {
        if let Some(entry) = RegistryEntry::load(id) {
            quarantine_if_broken(&entry);
        }
    }
}

/// Pick at most one entry that needs a build. Reconciles identity across all
/// active auto entries first (above), then takes the first eligible entry
/// **not** already served by a valid expected-opclass ANN index (a user index
/// satisfies readiness without postvec claiming it). Must run inside a
/// transaction.
pub fn pick_candidate() -> Option<i64> {
    // Broken entries are quarantined here, so they have already left the
    // eligible set by the time it is computed.
    quarantine_broken_entries();
    for id in eligible_entries() {
        let Some(entry) = RegistryEntry::load(id) else {
            continue;
        };
        if crate::api::registry::ann_index_ready(&entry) {
            continue;
        }
        return Some(id);
    }
    None
}

/// Run the build for one candidate inside the ambient transaction.
///
/// **Lock order: the managed table first, then the registry row.** That is
/// the order every lifecycle verb already uses — `set_format()` takes SHARE
/// ROW EXCLUSIVE before updating the registry, `disable()` runs its trigger
/// teardown DDL before marking the row disabled, `migrate()` adds the shadow
/// column before flipping the row to `migrating`, and `quarantine_entry()`
/// tears objects down before disabling. Taking the registry row first and
/// reaching the table later (through `CREATE INDEX`, which needs SHARE) would
/// invert that order and let a builder and a lifecycle verb deadlock:
/// builder holds the row and waits for the table while the verb holds the
/// table and waits for the row. PostgreSQL would break the cycle by aborting
/// one of them — a failed management command, or a spuriously parked
/// `index_error`.
///
/// The SHARE lock taken here is exactly the one the later `CREATE INDEX`
/// needs, so nothing is upgraded. With both locks held, the re-read below
/// sees any lifecycle change that committed first (and declines), while a
/// change starting later queues behind us on the table and then acts on the
/// finished index normally — `disable()` drops it with the entry.
///
/// New DML racing this is safe without extra locking: ordinary
/// `CREATE INDEX` includes rows visible to its command, and later writes
/// maintain the completed index normally.
pub fn build_candidate(id: i64) {
    // Name the relation before holding anything.
    let Some(entry) = RegistryEntry::load(id) else {
        return;
    };
    // Pin and validate identity BEFORE the stronger build lock. For a
    // recursive entry this takes source ACCESS SHARE then destination ACCESS
    // SHARE; skipping it here and asking `identity_broken` only after locking
    // the destination would invert the global source -> destination -> row
    // order and deadlock against lifecycle DDL.
    if entry.missing_dependency_locked(&[]).is_some() || entry.triggers_missing() {
        return;
    }
    // The build target is the vector table: the managed destination for a
    // recursive entry. Its ACCESS SHARE lock is already held by the
    // identity proof; upgrade it to the SHARE mode CREATE INDEX itself
    // needs.
    let qtable = entry.qualified_vector_table();
    let exists = Spi::get_one_with_args::<bool>(
        "SELECT to_regclass($1) IS NOT NULL",
        &[qtable.as_str().into()],
    )
    .unwrap_or(Some(false))
    .unwrap_or(false);
    if !exists {
        return; // the candidate scan quarantines it; nothing to build on
    }

    // (1) the destination/source table, in the mode CREATE INDEX will need …
    Spi::run(&format!("LOCK TABLE {qtable} IN SHARE MODE")).unwrap_or_else(|e| {
        error!("postvec: locking {qtable} for the automatic index build failed: {e}")
    });
    // (2) … then the registry row.
    let locked = Spi::get_one_with_args::<i64>(
        "SELECT id FROM postvec.registry WHERE id = $1 FOR UPDATE",
        &[id.into()],
    )
    .unwrap_or(None);
    if locked.is_none() {
        return; // the row vanished (uninstall/manual delete)
    }

    // Everything below re-reads under source + destination + registry locks.
    // A broken entry is declined rather than quarantined here (quarantine's
    // DDL would need an ACCESS EXCLUSIVE upgrade while we hold SHARE — the
    // candidate scan, which holds no table lock, does it instead).
    let Some(entry) = RegistryEntry::load(id) else {
        return;
    };
    if !eligible_entries().contains(&id)
        || identity_broken(&entry).is_some()
        || crate::api::registry::ann_index_ready(&entry)
    {
        return;
    }
    crate::api::registry::ensure_vector_index(&entry);
    log!(
        "postvec: automatic vector index built for {}.{}.{} (index_mode stays 'auto'; a \
         dropped index will be rebuilt)",
        entry.table_schema,
        entry.table_name,
        entry.source_column
    );
}

/// Test-only SQL entry point for [`build_candidate`], so the cross-session
/// suite can run the real build path in a session it can watch and cancel
/// (`pg_test` builds only — never present in a shipped extension).
#[cfg(feature = "pg_test_concurrency")]
#[pg_extern]
fn __test_build_candidate(id: i64) {
    build_candidate(id);
}

/// Test-only SQL entry point for [`record_build_failure`] (same rationale).
#[cfg(feature = "pg_test_concurrency")]
#[pg_extern]
fn __test_record_build_failure(id: i64, err: &str) {
    record_build_failure(id, err);
}

/// Test-only SQL entry point for [`pick_candidate`] — deliberately the whole
/// scan, not [`quarantine_broken_entries`] directly, so a cross-session test
/// proves reconciliation actually runs *inside* the production scan (calling
/// the helper would stay green even if the scan stopped invoking it). Running
/// it in a session also commits the reconciliation, which is what such a test
/// needs to observe.
#[cfg(feature = "pg_test_concurrency")]
#[pg_extern]
fn __test_pick_candidate() -> Option<i64> {
    pick_candidate()
}

/// Record a failed automatic build — called in a **fresh** transaction after
/// the build's own transaction aborted. Readiness is rechecked first: a
/// concurrent manual build winning the race means the entry is ready and no
/// error is stored. Otherwise the error parks the entry (only while it is
/// still active/auto), stopping automatic retries until the operator
/// intervenes.
pub fn record_build_failure(id: i64, err: &str) {
    // Take the registry row FIRST, so the readiness recheck and the update
    // are one atomic decision: a concurrent repair (`create_vector_index()`)
    // clears `index_error` under this same row lock, and without it a winner
    // committing between the check and the update would leave a stale parked
    // error behind. This path never touches the managed table (readiness is
    // a catalog read), so it cannot participate in the table→row ordering
    // that [`build_candidate`] documents.
    let locked = Spi::get_one_with_args::<i64>(
        "SELECT id FROM postvec.registry WHERE id = $1 FOR UPDATE",
        &[id.into()],
    )
    .unwrap_or(None);
    if locked.is_none() {
        return;
    }
    let Some(entry) = RegistryEntry::load(id) else {
        return;
    };
    // Fast path for the common shape: a build that failed because the
    // relation went away — dropped (or dropped and recreated) between the
    // existence probe and the LOCK TABLE in `build_candidate`, which then
    // errors. Declining to park keeps the entry in the scan's eligible set
    // so nothing has to notice it later.
    //
    // This is an optimisation, NOT the safety net: an *uncommitted* DROP is
    // invisible here, so a healthy-looking park can still happen moments
    // before the DROP commits. `quarantine_broken_entries()` is what makes
    // that recoverable — it reconciles identity for every active auto entry
    // regardless of `index_error`.
    if let Some(reason) = identity_broken(&entry) {
        log!(
            "postvec: automatic index build for registry id {id} failed because {reason}; \
             not parking the entry — the next worker pass quarantines it"
        );
        return;
    }
    if crate::api::registry::ann_index_ready(&entry) {
        log!(
            "postvec: automatic index build for registry id {id} failed but a usable index \
             now exists (concurrent manual build?); no error recorded"
        );
        return;
    }
    if entry.state != "active" || entry.index_mode != "auto" {
        return;
    }
    Spi::run_with_args(
        "UPDATE postvec.registry SET index_error = $2
          WHERE id = $1 AND state = 'active' AND index_mode = 'auto'",
        &[id.into(), err.into()],
    )
    .unwrap();
    warning!(
        "postvec: automatic vector index build for {}.{}.{} failed and is parked: {err}; \
         fix the cause, then run SELECT postvec.create_vector_index({tbl}, {col}) — its \
         readiness check clears the parked error (a suitable index you build yourself \
         counts too)",
        entry.table_schema,
        entry.table_name,
        entry.source_column,
        tbl =
            crate::registry::quote_literal(&format!("{}.{}", entry.table_schema, entry.table_name)),
        col = crate::registry::quote_literal(&entry.source_column),
    );
}

/// The worker-side step: at most one blocking build attempt per wake.
pub fn step(counters: &mut Counters) {
    let candidate = match try_transaction(pick_candidate) {
        Ok(c) => c,
        Err(e) => {
            counters.error(format!("auto-index scan: {e}"));
            warning!("postvec: auto-index candidate scan failed (will retry): {e}");
            return;
        }
    };
    let Some(id) = candidate else {
        return;
    };
    match try_transaction(move || build_candidate(id)) {
        Ok(()) => {}
        Err(e) => {
            // The build transaction aborted and rolled back. Park the entry
            // (or discover a concurrent winner) in a fresh transaction —
            // never in the aborted one.
            counters.error(format!("auto-index build: {e}"));
            let msg = e.clone();
            if let Err(e2) = try_transaction(move || record_build_failure(id, &msg)) {
                warning!("postvec: recording auto-index failure for {id} failed too: {e2}");
            }
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;

    fn seed_model(dim: i32) {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',$1,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[dim.into()],
        )
        .unwrap();
    }

    fn index_error(id: i64) -> Option<String> {
        Spi::get_one_with_args::<String>(
            "SELECT index_error FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap()
    }

    fn has_conventional_index(id: i64) -> bool {
        Spi::get_one_with_args::<bool>(
            "SELECT to_regclass($1) IS NOT NULL",
            &[format!("postvec_vec_{id}").into()],
        )
        .unwrap()
        .unwrap()
    }

    /// An auto-mode recursive entry builds its ANN index on the destination
    /// once refresh/child work drains. Destination drift quarantines
    /// instead of building on a foreign table.
    #[pg_test]
    fn auto_index_targets_the_destination_for_recursive_entries() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('a doc')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks', index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        // Work is pending (the backfill refresh): not eligible yet.
        assert_eq!(pick_candidate(), None, "waits for refresh/child drain");
        while crate::worker::chunk::process_one_refresh(5).processed {}
        Spi::run("DELETE FROM postvec.jobs").unwrap(); // children (no inference here)
        assert_eq!(pick_candidate(), Some(id));
        build_candidate(id);
        assert!(has_conventional_index(id));
        let def = Spi::get_one::<String>(&format!(
            "SELECT pg_get_indexdef(('postvec_vec_{id}')::regclass)"
        ))
        .unwrap()
        .unwrap_or_default();
        assert!(
            def.contains("docs_chunks") && def.contains("body_semantic"),
            "the ANN index is on the destination vector column: {def}"
        );

        // Destination drift (dropped and recreated without the token) is a
        // quarantine event at the next scan — never a build on the foreign
        // table.
        Spi::run(&format!("DROP INDEX IF EXISTS postvec_vec_{id}")).unwrap();
        Spi::run("DROP VIEW docs_chunks_view").unwrap();
        Spi::run("DROP TABLE docs_chunks").unwrap();
        Spi::run(
            "CREATE TABLE docs_chunks (postvec_chunk_id bigint, postvec_source_pk bigint,
                  postvec_chunk_seq int, postvec_char_start bigint, postvec_char_end bigint,
                  chunk_text text, body_semantic vector(3))",
        )
        .unwrap();
        assert_eq!(
            pick_candidate(),
            None,
            "no candidate on a foreign destination"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("disabled"),
            "the drifted entry was quarantined"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_index WHERE indrelid = 'docs_chunks'::regclass"
            )
            .unwrap(),
            Some(0),
            "the foreign replacement table was never touched"
        );
    }

    /// Auto waits for queue drain (and cursor completion), then builds once;
    /// success leaves the mode 'auto' so a later drop reconciles (test 21/28).
    #[pg_test]
    fn auto_waits_for_drain_builds_once_and_rebuilds_after_drop() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        let id =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', index_mode => 'auto')")
                .unwrap()
                .unwrap();

        // Backfill jobs pending: not eligible yet.
        assert!(eligible_entries().is_empty(), "pending jobs gate the build");
        assert_eq!(pick_candidate(), None);

        // Drain the queue, then the entry becomes the one candidate.
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        assert_eq!(pick_candidate(), Some(id));
        build_candidate(id);
        assert!(has_conventional_index(id), "the build happened");
        assert_eq!(index_error(id), None);
        assert_eq!(
            pick_candidate(),
            None,
            "a ready entry is no longer a candidate"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT index_mode FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("auto"),
            "success leaves the mode 'auto'"
        );

        // Dropping the (postvec-owned) index makes reconciliation rebuild it.
        Spi::run(&format!("DROP INDEX postvec_vec_{id}")).unwrap();
        assert_eq!(pick_candidate(), Some(id), "a dropped index reconciles");
        build_candidate(id);
        assert!(has_conventional_index(id));

        // Cursor backfill gates eligibility.
        Spi::run(&format!("DROP INDEX postvec_vec_{id}")).unwrap();
        Spi::run_with_args(
            "UPDATE postvec.registry SET backfill_mode = 'cursor' WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        assert!(eligible_entries().is_empty(), "cursor backfill gates auto");
        Spi::run_with_args(
            "UPDATE postvec.registry SET backfill_mode = 'done' WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        assert_eq!(pick_candidate(), Some(id), "a finished cursor un-gates");
    }

    /// Relation identity, not just column presence: a **synced** table
    /// dropped and recreated with matching column names passes
    /// `missing_dependency()` (which only checks the TRUNCATE sentinel for
    /// *observed* entries), so without the `triggers_missing()` guard the
    /// worker would build and extension-stamp an index on the unrelated
    /// replacement relation. It must quarantine instead.
    #[pg_test]
    fn auto_refuses_recreated_table_and_quarantines() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(pick_candidate(), Some(id), "a healthy entry is a candidate");

        // Same name, same columns — including a same-named vector column, so
        // every column-level check still passes — but a different relation:
        // the generated triggers (the entry's identity) died with the old
        // table. This is the shape an application recreating its own table
        // produces.
        Spi::run("DROP TABLE docs").unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, body_semantic vector(3))",
        )
        .unwrap();
        let entry = RegistryEntry::load(id).unwrap();
        assert!(
            entry.missing_dependency(&[]).is_none(),
            "the column-level check alone cannot see the swap — this is why \
             the trigger-identity guard exists"
        );

        assert_eq!(pick_candidate(), None, "no build on a recreated table");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("disabled"),
            "the stale entry was quarantined"
        );
        assert!(
            !has_conventional_index(id),
            "no index was created on the replacement relation"
        );
    }

    /// A build that failed because the relation vanished must NOT park the
    /// entry. `build_candidate()` probes existence and then locks the table;
    /// a DROP landing in that window makes the lock error, and parking the
    /// resulting failure would strand the entry forever — the candidate scan
    /// skips anything carrying an `index_error`, so it could never reach
    /// quarantine. The failure recorder therefore declines, and the next scan
    /// quarantines the entry (it holds no table lock, so its DDL is safe).
    #[pg_test]
    fn build_failure_on_a_vanished_relation_does_not_park() {
        for recreate in [false, true] {
            seed_model(3);
            Spi::run(
                "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                    body text)",
            )
            .unwrap();
            let id = Spi::get_one::<i64>(
                "SELECT postvec.enable('docs','body','m', backfill => false,
                                       index_mode => 'auto')",
            )
            .unwrap()
            .unwrap();

            // The window: the table is gone (or replaced) by the time the
            // build's LOCK TABLE runs, so the build transaction errored.
            Spi::run("DROP TABLE docs").unwrap();
            if recreate {
                Spi::run(
                    "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                        body text, body_semantic vector(3))",
                )
                .unwrap();
            }
            record_build_failure(id, "relation \"docs\" does not exist");

            assert_eq!(
                index_error(id),
                None,
                "a vanished relation must not park the entry (recreate={recreate})"
            );
            assert_eq!(
                Spi::get_one_with_args::<String>(
                    "SELECT state FROM postvec.registry WHERE id = $1",
                    &[id.into()],
                )
                .unwrap()
                .as_deref(),
                Some("active"),
                "the failure recorder does not quarantine either (recreate={recreate})"
            );
            // Unparked, it is still reachable by the scan — which quarantines.
            assert_eq!(pick_candidate(), None);
            assert_eq!(
                Spi::get_one_with_args::<String>(
                    "SELECT state FROM postvec.registry WHERE id = $1",
                    &[id.into()],
                )
                .unwrap()
                .as_deref(),
                Some("disabled"),
                "the next candidate scan quarantines it (recreate={recreate})"
            );

            // Reset for the second shape.
            Spi::run("DROP TABLE IF EXISTS docs").unwrap();
            Spi::run("DELETE FROM postvec.registry").unwrap();
        }
    }

    /// Every non-terminal migration state gates the build — `awaiting_index`
    /// in particular, which has an active registry row but is explicitly
    /// waiting for the operator's manual reindex.
    #[pg_test]
    fn auto_refuses_every_nonterminal_migration_state() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.migrations
                 (registry_id, old_model, new_model, old_dim, new_dim, strategy,
                  new_column, rows_total, state)
             VALUES ($1, 'm', 'm2', 3, 4, 'convert', 'body_semantic_new', 0, 'running')",
            &[id.into()],
        )
        .unwrap();
        for state in ["running", "awaiting_finalize", "awaiting_index", "failed"] {
            Spi::run_with_args(
                "UPDATE postvec.migrations SET state = $2 WHERE registry_id = $1",
                &[id.into(), state.into()],
            )
            .unwrap();
            assert!(
                eligible_entries().is_empty(),
                "migration state {state:?} must gate the automatic build"
            );
        }
        for state in ["done", "aborted"] {
            Spi::run_with_args(
                "UPDATE postvec.migrations SET state = $2 WHERE registry_id = $1",
                &[id.into(), state.into()],
            )
            .unwrap();
            assert_eq!(
                eligible_entries(),
                vec![id],
                "terminal state {state:?} must not gate it"
            );
        }
    }

    /// An observed entry (no embed route, nothing to drain) is still indexed —
    /// the step must not sit behind inference reachability (test 22).
    #[pg_test]
    fn auto_indexes_observed_entry_without_embed_route() {
        // 'dead' exists only as a converter target with no embeddable source.
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('conv-x-dead', 'convert', 'x', 'dead', 3, '{}'::jsonb)",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(3))",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body, embedding) VALUES ('a', '[1,2,3]')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'dead',
                                  sync => false, backfill => 'none', index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(pick_candidate(), Some(id));
        build_candidate(id);
        assert!(has_conventional_index(id), "observed entries are indexed");
    }

    /// A valid expected-opclass user index satisfies readiness without ever
    /// acquiring an extension dependency; create_vector_index() recognises it
    /// (no duplicate) and clears a parked error (test 23). Wrong-opclass and
    /// invalid indexes do not satisfy readiness (test 24).
    #[pg_test]
    fn user_index_satisfies_readiness_without_ownership() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();

        // A wrong-opclass user index does not satisfy readiness…
        Spi::run("CREATE INDEX user_l2 ON docs USING hnsw (body_semantic vector_l2_ops)").unwrap();
        assert_eq!(pick_candidate(), Some(id), "wrong opclass is not ready");
        Spi::run("DROP INDEX user_l2").unwrap();

        // …an expected-opclass one does.
        Spi::run("CREATE INDEX user_cos ON docs USING hnsw (body_semantic vector_cosine_ops)")
            .unwrap();
        assert_eq!(pick_candidate(), None, "the user index satisfies auto");

        // A parked error is cleared by create_vector_index() recognising the
        // user index — with no duplicate conventional index and no extension
        // dependency stamped onto the user object.
        Spi::run_with_args(
            "UPDATE postvec.registry SET index_error = 'seeded failure' WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        Spi::run("SELECT postvec.create_vector_index('docs','body')").unwrap();
        assert_eq!(index_error(id), None, "the parked error was cleared");
        assert!(
            !has_conventional_index(id),
            "no duplicate postvec-named index was created"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_depend d
                   JOIN pg_extension e ON e.oid = d.refobjid
                  WHERE d.classid = 'pg_class'::regclass
                    AND d.objid = 'user_cos'::regclass
                    AND d.refclassid = 'pg_extension'::regclass
                    AND d.deptype = 'x' AND e.extname = 'postvec'"
            )
            .unwrap(),
            Some(0),
            "readiness never transfers ownership"
        );

        // An invalidated index stops satisfying readiness.
        Spi::run("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'user_cos'::regclass")
            .unwrap();
        assert_eq!(pick_candidate(), Some(id), "an invalid index is not ready");
    }

    /// A same-named unrelated index parks the entry with index_error; it is
    /// never stamped, dropped, or treated as ready (test 25) — and a build
    /// failure is recorded only while no usable index exists; a concurrent
    /// manual winner suppresses the stale error (test 26).
    #[pg_test]
    fn collision_parks_and_manual_winner_suppresses_error() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        // A user object squatting on postvec's conventional name, on an
        // unrelated table.
        Spi::run("CREATE TABLE other (x vector(3))").unwrap();
        Spi::run(&format!(
            "CREATE INDEX postvec_vec_{id} ON other USING hnsw (x vector_cosine_ops)"
        ))
        .unwrap();

        assert_eq!(pick_candidate(), Some(id));
        let r = std::panic::catch_unwind(|| build_candidate(id));
        assert!(r.is_err(), "the name collision refuses the build");
        // The worker's failure policy runs in a fresh transaction; emulate it.
        record_build_failure(id, "name collision (seeded by test)");
        assert!(
            index_error(id).is_some(),
            "the failed build parks the entry"
        );
        assert_eq!(
            pick_candidate(),
            None,
            "a parked entry is not retried every wake"
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_depend d
                   JOIN pg_extension e ON e.oid = d.refobjid
                  WHERE d.classid = 'pg_class'::regclass
                    AND d.objid = 'postvec_vec_{id}'::regclass
                    AND d.refclassid = 'pg_extension'::regclass
                    AND d.deptype = 'x' AND e.extname = 'postvec'"
            ))
            .unwrap(),
            Some(0),
            "the foreign index is never stamped"
        );

        // Manual repair: the operator builds a correct custom index, then
        // create_vector_index() verifies readiness and clears the error.
        Spi::run("CREATE INDEX docs_fix ON docs USING hnsw (body_semantic vector_cosine_ops)")
            .unwrap();
        Spi::run("SELECT postvec.create_vector_index('docs','body')").unwrap();
        assert_eq!(index_error(id), None, "manual repair clears the error");

        // The concurrent-winner path: a failure recorded after a usable index
        // appeared stores no error.
        record_build_failure(id, "stale failure from a lost race");
        assert_eq!(
            index_error(id),
            None,
            "a concurrent manual winner suppresses the stale error"
        );
    }

    /// Live migrations gate eligibility; dimensions above 2000 refuse
    /// immediate/auto up front with the halfvec suggestion (test 27).
    #[pg_test]
    fn auto_refuses_migration_and_high_dimensions() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m',  'embed',   NULL, 'm',  3, '{}'::jsonb),
                    ('m2', 'embed',   NULL, 'm2', 4, '{}'::jsonb),
                    ('mhi','embed',   NULL, 'mhi', 2100, '{}'::jsonb),
                    ('conv-m-m2', 'convert', 'm', 'm2', 4, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, index_mode => 'auto')",
        )
        .unwrap()
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        assert!(
            eligible_entries().is_empty(),
            "a live migration gates eligibility"
        );
        let mid = Spi::get_one::<i64>("SELECT id FROM postvec.migrations")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();
        assert_eq!(pick_candidate(), Some(id), "the aborted migration un-gates");

        // > 2000 dims: both non-manual modes refuse at declaration time.
        Spi::run(
            "CREATE TABLE hdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        for mode in ["immediate", "auto"] {
            let q = format!(
                "SELECT postvec.enable('hdocs','body','mhi', backfill => false, \
                 index_mode => '{mode}')"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{mode} above 2000 dims must refuse");
        }
        let r =
            Spi::get_one::<i64>("SELECT postvec.enable('hdocs','body','mhi', backfill => false)")
                .unwrap();
        assert!(r.is_some(), "manual mode still accepts high dimensions");
    }

    /// index_mode => 'immediate' on enable()/adopt() builds synchronously and
    /// records intent without later reconciliation.
    #[pg_test]
    fn immediate_builds_synchronously_and_does_not_reconcile() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false,
                                   index_mode => 'immediate')",
        )
        .unwrap()
        .unwrap();
        assert!(has_conventional_index(id), "immediate builds in-call");
        Spi::run(&format!("DROP INDEX postvec_vec_{id}")).unwrap();
        assert_eq!(
            pick_candidate(),
            None,
            "immediate records intent only; it never reconciles later"
        );
    }
}
