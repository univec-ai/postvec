//! Local refresh phase: split one changed document and replace its chunk
//! set atomically. No network I/O in this module. A refresh claims, splits
//! and writes in one transaction, so chunk text and lexical search still
//! converge while ninference is down.
//!
//! One refresh per transaction is deliberate. Multi-document batches would
//! interleave source `FOR SHARE` row locks with arbitrary application
//! UPDATE order (a lock-order deadlock farm) and unbound the transaction's
//! write size. Throughput comes from the worker drain loop calling
//! [`step`] until it reports no work.
//!
//! Backpressure is a bound, not a barrier: a refresh is eligible while
//! its entry's live child embed jobs (pending or claimed) number fewer
//! than [`chunk_inflight_max`]. Many documents' children batch together,
//! the queue stays bounded and a freshly edited document becomes
//! searchable as soon as the backlog dips below the bound, not when it
//! empties. The 10,000-chunk cap means one refresh can overshoot the
//! bound in one step; that is intended. The bound gates starting an
//! expansion.

use crate::registry::{quote_ident, RegistryEntry};
use crate::worker::{try_transaction, Counters};
use pgrx::prelude::*;

/// `max(batch_size * 8, 1000)`, derived from the operator's inference
/// batch size instead of another GUC.
pub fn chunk_inflight_max() -> i64 {
    (crate::gucs::BATCH_SIZE.get() as i64 * 8).max(1000)
}

/// One claimed refresh job.
struct RefreshClaim {
    job_id: i64,
    registry_id: i64,
    pk_value: String,
    attempts: i32,
}

/// Identify and pin the oldest due refresh entry before touching its job row.
/// The actual claim is restricted to this ID, so a concurrent enqueue cannot
/// make the worker claim an unpinned relation. Missing/disabled entries remain
/// eligible for the normal cleanup path; broken active entries are
/// quarantined in source -> destination -> registry -> jobs order.
fn prepare_refresh_entry(bound: i64) -> Result<Option<i64>, ()> {
    let registry_id = Spi::connect(|c| {
        let table = c
            .select(
                "SELECT cand.registry_id FROM postvec.jobs cand
                  WHERE cand.claimed_at IS NULL AND cand.not_before <= now()
                    AND cand.op = 'refresh'
                    AND (SELECT count(*) FROM (
                             SELECT 1 FROM postvec.jobs live
                              WHERE live.registry_id = cand.registry_id
                                AND live.op = 'embed'
                              LIMIT $1) x) < $1
                  ORDER BY cand.not_before, cand.id
                  LIMIT 1",
                Some(1),
                &[bound.into()],
            )
            .expect("postvec: refresh candidate scan failed");
        table
            .into_iter()
            .next()
            .and_then(|r| r.get::<i64>(1).unwrap())
    });
    let Some(registry_id) = registry_id else {
        return Ok(None);
    };

    let Some(entry) = RegistryEntry::load(registry_id) else {
        return Ok(Some(registry_id));
    };
    if entry.state == "disabled" {
        return Ok(Some(registry_id));
    }
    if let Some(reason) = entry.missing_dependency_locked(&[]) {
        crate::api::registry::quarantine_entry(&entry, &reason);
        return Err(());
    }
    Ok(Some(registry_id))
}

/// Claim the oldest due refresh for the already-pinned registry entry.
/// The backpressure probe is bounded (`LIMIT bound` inside a count) so its
/// cost is capped no matter how deep the queue is, and it rides the partial
/// `jobs_live_embed_registry` index, skipping the refresh backlog entirely.
fn claim_refresh(bound: i64, registry_id: i64) -> Option<RefreshClaim> {
    Spi::connect_mut(|c| {
        let t = c
            .update(
                "UPDATE postvec.jobs j
                    SET claimed_at = now(), attempts = attempts + 1
                  WHERE j.id = (
                      SELECT id FROM postvec.jobs cand
                       WHERE cand.claimed_at IS NULL AND cand.not_before <= now()
                         AND cand.op = 'refresh'
                         AND cand.registry_id = $2
                         AND (SELECT count(*) FROM (
                                  SELECT 1 FROM postvec.jobs live
                                   WHERE live.registry_id = cand.registry_id
                                     AND live.op = 'embed'
                                   LIMIT $1) x) < $1
                       ORDER BY cand.not_before, cand.id
                       LIMIT 1
                       FOR UPDATE SKIP LOCKED)
                RETURNING id, registry_id, pk_value, attempts",
                None,
                &[bound.into(), registry_id.into()],
            )
            .expect("postvec: refresh claim failed");
        t.into_iter().next().map(|r| RefreshClaim {
            job_id: r.get::<i64>(1).unwrap().unwrap(),
            registry_id: r.get::<i64>(2).unwrap().unwrap(),
            pk_value: r.get::<String>(3).unwrap().unwrap(),
            attempts: r.get::<i32>(4).unwrap().unwrap(),
        })
    })
}

/// Purge a document's derived state: destination chunks (native-typed key),
/// its child embed jobs, and its dead rows. The refresh job itself is the
/// caller's to finish.
fn purge_document(entry: &RegistryEntry, pk_value: &str) {
    let qdest = entry.qualified_vector_table();
    let pk_type = &entry.pk_types[0];
    Spi::run_with_args(
        &format!("DELETE FROM {qdest} WHERE postvec_source_pk = $1::{pk_type}"),
        &[pk_value.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "DELETE FROM postvec.jobs
          WHERE registry_id = $1 AND pk_value = $2 AND op = 'embed'",
        &[entry.id.into(), pk_value.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "DELETE FROM postvec.jobs_dead WHERE registry_id = $1 AND pk_value = $2",
        &[entry.id.into(), pk_value.into()],
    )
    .unwrap();
}

/// The outcome of one refresh transaction, for the heartbeat counters.
#[derive(Default, Clone, Copy)]
pub struct Refreshed {
    pub processed: bool,
    pub chunks_created: i64,
}

/// Process at most one refresh job in the ambient transaction. Returns what
/// happened; SPI errors propagate to the caller's `try_transaction`.
pub fn process_one_refresh(max_retries: i32) -> Refreshed {
    let none = Refreshed::default();
    let done = Refreshed {
        processed: true,
        chunks_created: 0,
    };
    let bound = chunk_inflight_max();
    let registry_id = match prepare_refresh_entry(bound) {
        Ok(Some(id)) => id,
        Ok(None) => return none,
        // Quarantine consumed the broken entry's jobs. Report progress so the
        // drain loop immediately rescans and continues with healthy work.
        Err(()) => return done,
    };
    let Some(claim) = claim_refresh(bound, registry_id) else {
        return none;
    };

    let Some(entry) = RegistryEntry::load(claim.registry_id) else {
        crate::jobs::delete_jobs(&[claim.job_id]);
        return done;
    };
    if entry.state == "disabled" {
        crate::jobs::delete_jobs(&[claim.job_id]);
        return done;
    }
    if !entry.is_recursive() {
        // A refresh job on a column-mode entry cannot come from postvec's
        // own paths; classify instead of looping on it.
        crate::jobs::move_to_dead(
            &[claim.job_id],
            "malformed queue row: op='refresh' on a non-chunked entry",
        );
        return done;
    }
    if let Some(reason) = entry.missing_dependency(&[]) {
        // `prepare_refresh_entry` already pinned a healthy entry before the
        // job claim; this defensive re-read handles cleanup-only races.
        crate::api::registry::quarantine_entry(&entry, &reason);
        return done;
    }
    // PUBLIC can INSERT (registry_id, pk_value); a key that does not cast to
    // the PK type would abort every SQL below, re-delivering forever.
    let castable = Spi::get_one_with_args::<bool>(
        "SELECT pg_catalog.pg_input_is_valid($1, $2)",
        &[
            claim.pk_value.as_str().into(),
            entry.pk_types[0].as_str().into(),
        ],
    )
    .unwrap()
    .unwrap_or(false);
    if !castable {
        crate::jobs::move_to_dead(
            &[claim.job_id],
            &format!(
                "pk_value is not valid input for PK type {} (direct queue insert?)",
                entry.pk_types[0]
            ),
        );
        return done;
    }

    // Lock the source row FOR SHARE, the load-bearing lock: it
    // conflicts with source UPDATE/DELETE, so either this refresh
    // materializes first and the later trigger removes its output, or the
    // source change commits first and this reads the new row. There is no
    // window to commit chunks of an older row version after a newer
    // version's invalidation. Raw source text, never the format template.
    // The template is rendered per chunk at child claim time.
    let qtable = entry.qualified_table();
    let pk = quote_ident(&entry.pk_columns[0]);
    let src = quote_ident(&entry.source_column);
    let pk_type = &entry.pk_types[0];
    // The splitter's 32 MiB input bound is enforced in the same guarded
    // FOR SHARE read. An over-limit document is measured with
    // octet_length (cheap on toasted text) and never copied into Rust at
    // all. Copying the whole value first and rejecting it afterwards
    // would let a multi-gigabyte cell OOM the worker before the check
    // ran.
    let splitter_cap = crate::chunking::MAX_DOCUMENT_BYTES as i64;
    let row: Option<(Option<String>, Option<i64>)> = Spi::connect_mut(|c| {
        let t = c
            .update(
                &format!(
                    "SELECT CASE WHEN octet_length({src}::text) <= $2
                                 THEN {src}::text END,
                            octet_length({src}::text)
                       FROM {qtable}
                      WHERE {pk} = $1::{pk_type} FOR SHARE"
                ),
                Some(1),
                &[claim.pk_value.as_str().into(), splitter_cap.into()],
            )
            .expect("postvec: refresh source read failed");
        t.into_iter()
            .next()
            .map(|r| (r.get::<String>(1).unwrap(), r.get::<i64>(2).unwrap()))
    });

    let text = match row {
        None | Some((None, None)) => {
            // Row gone or source NULL: everything derived from it is
            // obsolete. Materializing zero chunks is success.
            purge_document(&entry, &claim.pk_value);
            crate::jobs::delete_jobs(&[claim.job_id]);
            return done;
        }
        Some((None, Some(len))) => {
            // Over the splitter bound: dead-letter with the measured size,
            // exactly like the splitter itself would — minus the copy.
            crate::jobs::move_to_dead(
                &[claim.job_id],
                &format!(
                    "document is {len} bytes; the recursive splitter accepts at most \
                     {splitter_cap} bytes of UTF-8 — store oversized payloads outside \
                     the semantic column or split them upstream"
                ),
            );
            return done;
        }
        Some((Some(text), _)) => text,
    };

    // Catalog drift (a hand-edited registry row can NULL these) must
    // quarantine the entry, not panic: the panic would surface as a logged
    // phase failure that repeats every wake without ever removing the entry
    // from the work set.
    let (size, overlap) = match (entry.chunk_size, entry.chunk_overlap) {
        (Some(size), Some(overlap)) => (size, overlap),
        _ => {
            crate::api::registry::quarantine_entry(
                &entry,
                "recursive entry is missing chunk_size/chunk_overlap (registry row edited?)",
            );
            return done;
        }
    };
    let chunks = match crate::chunking::split_recursive(&text, size, overlap) {
        Ok(chunks) => chunks,
        Err(e) => {
            // Permanent per-document failure (the 10,000-chunk cap or a
            // corrupted geometry): dead-letter the refresh with the full
            // remediation; no destination row was touched, so nothing
            // partial can commit.
            let _ = claim.attempts; // budget is irrelevant: this never heals by retry
            crate::jobs::move_to_dead(
                &[claim.job_id],
                &format!(
                    "refresh of {}.{}.{} row {:?} failed: {e}",
                    entry.table_schema, entry.table_name, entry.source_column, claim.pk_value
                ),
            );
            return done;
        }
    };
    let _ = max_retries;

    // Replace the document's chunk set and child work atomically.
    purge_document(&entry, &claim.pk_value);
    let n_chunks = chunks.len() as i64;
    if !chunks.is_empty() {
        let qdest = entry.qualified_vector_table();
        let seqs: Vec<i32> = chunks.iter().map(|c| c.seq).collect();
        let starts: Vec<i64> = chunks.iter().map(|c| c.char_start).collect();
        let ends: Vec<i64> = chunks.iter().map(|c| c.char_end).collect();
        let texts: Vec<String> = chunks.into_iter().map(|c| c.text).collect();
        let ids: Vec<i64> = Spi::connect_mut(|c| {
            let t = c
                .update(
                    &format!(
                        "INSERT INTO {qdest}
                             (postvec_source_pk, postvec_chunk_seq,
                              postvec_char_start, postvec_char_end, chunk_text)
                         SELECT $1::{pk_type}, s.seq, s.cs, s.ce, s.txt
                           FROM unnest($2::int4[], $3::int8[], $4::int8[], $5::text[])
                                AS s(seq, cs, ce, txt)
                         RETURNING postvec_chunk_id"
                    ),
                    None,
                    &[
                        claim.pk_value.as_str().into(),
                        seqs.into(),
                        starts.into(),
                        ends.into(),
                        texts.into(),
                    ],
                )
                .expect("postvec: chunk insert failed");
            t.into_iter()
                .map(|r| r.get::<i64>(1).unwrap().unwrap())
                .collect()
        });
        Spi::connect_mut(|c| {
            c.update(
                "INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
                 SELECT $1, $2, 'embed', unnest($3::int8[])
                 ON CONFLICT (registry_id, op, pk_value, chunk_id)
                 WHERE claimed_at IS NULL DO NOTHING",
                None,
                &[entry.id.into(), claim.pk_value.as_str().into(), ids.into()],
            )
            .expect("postvec: child job insert failed");
        });
    }
    crate::jobs::delete_jobs(&[claim.job_id]);
    Refreshed {
        processed: true,
        chunks_created: n_chunks,
    }
}

/// Test-only SQL entry point for [`process_one_refresh`], so the
/// cross-session suite can run the real refresh — including its source-row
/// `FOR SHARE` lock — inside a transaction it holds open and can commit on
/// cue (`pg_test` builds only — never present in a shipped extension).
#[cfg(feature = "pg_test_concurrency")]
#[pg_extern]
fn __test_process_one_refresh() -> bool {
    process_one_refresh(crate::gucs::MAX_RETRIES.get()).processed
}

/// Worker-side wrapper: one refresh in its own guarded transaction. A failed
/// transaction (lock timeout on a blocked source table, a raising policy)
/// rolls the claim back with it — nothing is lost; the job re-selects on the
/// next pass. Returns whether a refresh was processed (the drain loop's
/// "did anything happen" input).
pub fn step(counters: &mut Counters) -> bool {
    let max_retries = crate::gucs::MAX_RETRIES.get();
    match try_transaction(move || process_one_refresh(max_retries)) {
        Ok(r) => {
            if r.processed {
                counters.documents_chunked += 1;
                counters.chunks_created += r.chunks_created;
            }
            r.processed
        }
        Err(e) => {
            counters.error(format!("chunk refresh: {e}"));
            warning!("postvec: chunk refresh transaction failed (will retry): {e}");
            false
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

    fn enable_chunked(size: i32, overlap: i32) -> i64 {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one_with_args::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks',
                                   chunk_size => $1, chunk_overlap => $2)",
            &[size.into(), overlap.into()],
        )
        .unwrap()
        .unwrap()
    }

    /// Drain every due refresh in the ambient transaction.
    pub(crate) fn drain_refreshes() -> i64 {
        let mut n = 0;
        while process_one_refresh(5).processed {
            n += 1;
        }
        n
    }

    #[pg_test]
    fn refresh_materializes_chunks_and_child_jobs() {
        let id = enable_chunked(64, 8);
        Spi::run("INSERT INTO docs (body) VALUES (repeat('word ', 40))").unwrap();

        assert_eq!(drain_refreshes(), 1);
        let chunks = Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks")
            .unwrap()
            .unwrap();
        assert!(chunks > 1, "a 200-char doc at size 64 splits: {chunks}");
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'embed' AND chunk_id IS NOT NULL",
                &[id.into()],
            )
            .unwrap(),
            Some(chunks),
            "one child embed job per chunk"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1 AND op = 'refresh'",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "the refresh job is consumed"
        );
        // Chunk rows carry offsets that slice back to the text, seq from 0.
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT bool_and(
                            chunk_text = substr(d.body, postvec_char_start::int + 1,
                                                (postvec_char_end - postvec_char_start)::int))
                   FROM docs_chunks c JOIN docs d ON d.id = c.postvec_source_pk"
            )
            .unwrap(),
            Some(true),
            "offsets are exact character offsets into the source"
        );
    }

    #[pg_test]
    fn refresh_of_null_or_deleted_source_purges() {
        let id = enable_chunked(64, 0);
        Spi::run("INSERT INTO docs (body) VALUES ('short doc')").unwrap();
        assert_eq!(drain_refreshes(), 1);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(1)
        );

        // NULL the source: the trigger purges chunks inline and enqueues no
        // refresh (source NULL) — chunks must be gone at commit already.
        Spi::run("UPDATE docs SET body = NULL").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "invalidation deleted the chunks in the writer's transaction"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "no refresh for a NULL source"
        );
    }

    #[pg_test]
    fn oversized_document_dead_letters_atomically() {
        let id = enable_chunked(64, 0);
        // 40-char paragraphs cannot merge at size 64: one chunk each, so
        // 10_001 paragraphs exceed the cap.
        Spi::run(
            "INSERT INTO docs (body)
             SELECT string_agg(repeat('p', 40), E'\\n\\n') FROM generate_series(1, 10001)",
        )
        .unwrap();
        assert_eq!(drain_refreshes(), 1, "the refresh is consumed (dead)");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "no partial chunk set commits"
        );
        let err = Spi::get_one_with_args::<String>(
            "SELECT last_error FROM postvec.jobs_dead WHERE registry_id = $1",
            &[id.into()],
        )
        .unwrap()
        .unwrap_or_default();
        assert!(
            err.contains("10000") && err.contains("chunk_size"),
            "dead letter names the cap and the remediation: {err}"
        );
    }

    #[pg_test]
    fn update_purges_old_chunks_and_child_jobs_inline() {
        let id = enable_chunked(64, 0);
        Spi::run("INSERT INTO docs (body) VALUES (repeat('word ', 40))").unwrap();
        assert_eq!(drain_refreshes(), 1);
        let old_ids: Vec<i64> = Spi::connect(|c| {
            c.select(
                "SELECT postvec_chunk_id FROM docs_chunks ORDER BY 1",
                None,
                &[],
            )
            .unwrap()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
        });
        assert!(!old_ids.is_empty());

        Spi::run("UPDATE docs SET body = repeat('fresh ', 30)").unwrap();
        // Old chunks and child jobs are gone in the writer's transaction;
        // exactly one refresh is pending.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1 AND op = 'embed'",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "old child jobs purged inline"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1 AND op = 'refresh'",
                &[id.into()],
            )
            .unwrap(),
            Some(1)
        );

        assert_eq!(drain_refreshes(), 1);
        let new_ids: Vec<i64> = Spi::connect(|c| {
            c.select(
                "SELECT postvec_chunk_id FROM docs_chunks ORDER BY 1",
                None,
                &[],
            )
            .unwrap()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
        });
        assert!(
            new_ids.iter().all(|id| !old_ids.contains(id)),
            "chunk identities are never reused across refreshes"
        );
    }

    /// [R2-2]: refreshes keep flowing while a child backlog exists, up to the
    /// bound — and stop above it.
    #[pg_test]
    fn refresh_backpressure_is_a_bound_not_a_barrier() {
        let id = enable_chunked(64, 0);
        Spi::run("INSERT INTO docs (body) VALUES ('doc one'), ('doc two')").unwrap();

        // Saturate the entry with fake live child jobs beyond the bound.
        let bound = chunk_inflight_max();
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
             SELECT $1, '999', 'embed', g FROM generate_series(1, $2::bigint + 1) g",
            &[id.into(), bound.into()],
        )
        .unwrap();
        assert!(
            !process_one_refresh(5).processed,
            "no refresh starts while the entry is at the child-job bound"
        );

        // Dip below the bound: refreshes flow again.
        Spi::run("DELETE FROM postvec.jobs WHERE op = 'embed' AND chunk_id > 500").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "refreshes resume once the backlog dips below the bound"
        );
    }

    #[pg_test]
    fn malformed_refresh_key_dead_letters() {
        let id = enable_chunked(64, 0);
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value, op)
             VALUES ($1, 'not-a-bigint', 'refresh')",
            &[id.into()],
        )
        .unwrap();
        assert_eq!(drain_refreshes(), 1);
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .unwrap_or_default();
        assert!(err.contains("not valid input"), "{err}");
    }

    /// Every structural drift on the destination is a quarantine, never a
    /// repeated worker failure. An altered vector dimension, an altered
    /// source-key type, a dropped join view (while the source lives), a
    /// dropped unique key and an edited marker each take the entry out of
    /// service and retain the destination.
    #[pg_test]
    fn destination_structural_drift_quarantines() {
        for (i, drift_sql) in [
            // vector dimension changed
            "ALTER TABLE docs_chunks DROP COLUMN body_semantic;
             ALTER TABLE docs_chunks ADD COLUMN body_semantic vector(5)",
            // source-key type changed (the policy and view pin the column,
            // so a real drift would have removed them first; the source-key
            // check outranks the missing-view check in the validator)
            "DROP POLICY postvec_source_visible ON docs_chunks;
             DROP VIEW docs_chunks_view;
             ALTER TABLE docs_chunks ALTER COLUMN postvec_source_pk TYPE text
                 USING postvec_source_pk::text",
            // view gone while the source lives
            "DROP VIEW docs_chunks_view",
            // unique key gone
            "ALTER TABLE docs_chunks
                 DROP CONSTRAINT docs_chunks_postvec_source_pk_postvec_chunk_seq_key",
            // marker edited
            "COMMENT ON TABLE docs_chunks IS 'edited'",
            // row security no longer forced (owner reads escape the policy)
            "ALTER TABLE docs_chunks NO FORCE ROW LEVEL SECURITY",
            // the source-visibility policy is gone while the source lives
            "DROP POLICY postvec_source_visible ON docs_chunks",
            // the view silently became definer-rights (caller RLS bypassed)
            "ALTER VIEW docs_chunks_view SET (security_invoker = false)",
            // the policy keeps its source dependency but allows everything —
            // the qual proof, not the pg_depend edge, must catch this
            "DROP POLICY postvec_source_visible ON docs_chunks;
             CREATE POLICY postvec_source_visible ON docs_chunks FOR SELECT
                 USING (true OR EXISTS (SELECT 1 FROM docs s
                                         WHERE s.id = postvec_source_pk))",
            // a second permissive SELECT policy ORs the canonical one open
            "CREATE POLICY wide_open ON docs_chunks FOR SELECT USING (true)",
        ]
        .iter()
        .enumerate()
        {
            let id = enable_chunked(64, 0);
            Spi::run("INSERT INTO docs (body) VALUES ('a doc')").unwrap();
            Spi::run(drift_sql).unwrap();
            assert!(
                process_one_refresh(5).processed,
                "case {i}: the refresh is consumed"
            );
            assert_eq!(
                Spi::get_one_with_args::<String>(
                    "SELECT state FROM postvec.registry WHERE id = $1",
                    &[id.into()],
                )
                .unwrap()
                .as_deref(),
                Some("disabled"),
                "case {i} ({drift_sql:?}): drift quarantines the entry"
            );
            assert_eq!(
                Spi::get_one::<bool>("SELECT to_regclass('docs_chunks') IS NOT NULL").unwrap(),
                Some(true),
                "case {i}: the destination is retained for inspection"
            );
            Spi::run("DROP TABLE docs CASCADE; DROP TABLE docs_chunks CASCADE").unwrap();
            Spi::run_with_args(
                "DELETE FROM postvec.jobs WHERE registry_id = $1;",
                &[id.into()],
            )
            .unwrap();
            Spi::run_with_args("DELETE FROM postvec.registry WHERE id = $1", &[id.into()]).unwrap();
        }
    }

    /// The complement of the policy drift cases: RESTRICTIVE policies only
    /// narrow chunk visibility, so an operator adding one is legitimate
    /// hardening, not drift — the entry must keep processing.
    #[pg_test]
    fn restrictive_policies_do_not_quarantine() {
        let id = enable_chunked(64, 0);
        Spi::run("INSERT INTO docs (body) VALUES ('a doc')").unwrap();
        Spi::run(
            "CREATE POLICY narrow ON docs_chunks AS RESTRICTIVE FOR SELECT
                 USING (postvec_chunk_seq >= 0)",
        )
        .unwrap();
        assert!(process_one_refresh(5).processed, "the refresh is consumed");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "a restrictive policy is not drift"
        );
        assert!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks")
                .unwrap()
                .unwrap()
                > 0,
            "chunks were produced"
        );
        Spi::run("DROP TABLE docs CASCADE; DROP TABLE docs_chunks CASCADE").unwrap();
        Spi::run_with_args(
            "DELETE FROM postvec.jobs WHERE registry_id = $1;
             DELETE FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
    }

    /// A PK whose type is a domain in a custom schema works end to end
    /// even when the processing session's search path cannot see that
    /// schema. The registry stores the catalog-qualified spelling.
    #[pg_test]
    fn custom_schema_domain_pk_survives_worker_search_path() {
        seed_model(3);
        Spi::run("CREATE SCHEMA pkdom").unwrap();
        Spi::run("CREATE DOMAIN pkdom.docid AS bigint").unwrap();
        Spi::run("CREATE TABLE ddocs (id pkdom.docid PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('ddocs','body','m', chunking => 'recursive',
                                   destination => 'ddocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        // The stored cast target is schema-qualified, so worker SQL resolves
        // it under ANY search path.
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT pk_types[1] FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("pkdom.docid")
        );
        Spi::run("INSERT INTO ddocs VALUES (7, 'a domain-keyed document')").unwrap();
        // Simulate the background worker's environment: no application
        // search path at all.
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh resolves the domain"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM ddocs_chunks WHERE postvec_source_pk = 7")
                .unwrap(),
            Some(1),
            "chunks materialized under the domain key"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE op = 'embed' AND chunk_id IS NOT NULL"
            )
            .unwrap(),
            Some(1)
        );
    }

    /// A `citext` PK: its equality operator lives in `public`, not
    /// `pg_catalog`, so the policy qual deparses with `OPERATOR(public.=)`
    /// syntax. The canonical-policy proof derives its accepted operators from
    /// the PK index's own btree operator family, so the entry must enable,
    /// refresh (the validator runs under the worker's bare search path), and
    /// tear down as healthy — never quarantine.
    #[pg_test]
    fn citext_pk_enables_refreshes_and_tears_down() {
        seed_model(3);
        Spi::run("CREATE EXTENSION IF NOT EXISTS citext").unwrap();
        Spi::run("CREATE TABLE cdocs (id public.citext PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('cdocs','body','m', chunking => 'recursive',
                                   destination => 'cdocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO cdocs VALUES ('Doc-One', 'a citext-keyed document')").unwrap();
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh is consumed, not quarantined"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "an OPERATOR()-deparsed policy qual is canonical, not drift"
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM cdocs_chunks WHERE postvec_source_pk = 'doc-one'"
            )
            .unwrap()
            .unwrap()
                > 0,
            "chunks materialized under the citext key"
        );
        // Destructive teardown reruns the same proof under locks.
        Spi::run("SELECT postvec.disable('cdocs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('cdocs_chunks') IS NULL").unwrap(),
            Some(true),
            "the proven destination is dropped"
        );
    }

    /// An enum PK: `enum_ops` carries its equality at `anyenum`, which no
    /// concrete enum type reaches through the domain chain or `pg_cast` — the
    /// polymorphic acceptance in the canonical-policy proof is what keeps
    /// this healthy (the qual itself deparses as a plain cast-free `=`).
    #[pg_test]
    fn enum_pk_enables_refreshes_and_tears_down() {
        seed_model(3);
        Spi::run("CREATE TYPE mood AS ENUM ('happy','sad')").unwrap();
        Spi::run("CREATE TABLE edocs (id mood PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('edocs','body','m', chunking => 'recursive',
                                   destination => 'edocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO edocs VALUES ('happy', 'an enum-keyed document')").unwrap();
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh is consumed, not quarantined"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "a polymorphic-equality policy qual is canonical, not drift"
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM edocs_chunks WHERE postvec_source_pk = 'happy'"
            )
            .unwrap()
            .unwrap()
                > 0,
            "chunks materialized under the enum key"
        );
        Spi::run("SELECT postvec.disable('edocs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('edocs_chunks') IS NULL").unwrap(),
            Some(true),
            "the proven destination is dropped"
        );
    }

    /// An array PK: `array_ops` equality lives at `anyarray` — the same
    /// polymorphic acceptance path as the enum case.
    #[pg_test]
    fn array_pk_enables_refreshes_and_tears_down() {
        seed_model(3);
        Spi::run("CREATE TABLE adocs (id bigint[] PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('adocs','body','m', chunking => 'recursive',
                                   destination => 'adocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO adocs VALUES (ARRAY[1,2]::bigint[], 'an array-keyed document')")
            .unwrap();
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh is consumed, not quarantined"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "the anyarray equality qual is canonical, not drift"
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM adocs_chunks
                  WHERE postvec_source_pk = ARRAY[1,2]::bigint[]"
            )
            .unwrap()
            .unwrap()
                > 0,
            "chunks materialized under the array key"
        );
        Spi::run("SELECT postvec.disable('adocs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('adocs_chunks') IS NULL").unwrap(),
            Some(true),
            "the proven destination is dropped"
        );
    }

    /// A domain over an array: the parser resolves `=` through `anyarray`
    /// but deparses casts to the **terminal base** on both sides
    /// (`(s.id)::bigint[] = …::bigint[]`) — the polymorphic acceptance must
    /// pair the pseudo-type match with that base cast, not bare equality.
    #[pg_test]
    fn domain_over_array_pk_enables_refreshes_and_tears_down() {
        seed_model(3);
        Spi::run("CREATE DOMAIN arr_id AS bigint[]").unwrap();
        Spi::run("CREATE TABLE dadocs (id arr_id PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('dadocs','body','m', chunking => 'recursive',
                                   destination => 'dadocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO dadocs VALUES (ARRAY[3]::arr_id, 'a domain-array document')")
            .unwrap();
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh is consumed, not quarantined"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "the base-cast polymorphic qual is canonical, not drift"
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM dadocs_chunks
                  WHERE postvec_source_pk = ARRAY[3]::arr_id"
            )
            .unwrap()
            .unwrap()
                > 0,
            "chunks materialized under the domain-array key"
        );
        Spi::run("SELECT postvec.disable('dadocs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('dadocs_chunks') IS NULL").unwrap(),
            Some(true),
            "the proven destination is dropped"
        );
    }

    /// A domain over a composite: `record` is the one polymorphic family the
    /// parser deparses **bare** even through a domain — no terminal-base
    /// casts, unlike domain-over-array/range. The acceptance must expect
    /// plain equality here or the entry quarantines.
    #[pg_test]
    fn domain_over_composite_pk_enables_refreshes_and_tears_down() {
        seed_model(3);
        Spi::run("CREATE TYPE pairkey AS (a int, b int)").unwrap();
        Spi::run("CREATE DOMAIN pair_id AS pairkey").unwrap();
        Spi::run("CREATE TABLE pdocs (id pair_id PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('pdocs','body','m', chunking => 'recursive',
                                   destination => 'pdocs_chunks',
                                   chunk_size => 64, chunk_overlap => 0)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO pdocs VALUES ((1,2)::pair_id, 'a composite-keyed document')")
            .unwrap();
        Spi::run("SET LOCAL search_path TO pg_catalog").unwrap();
        assert!(
            process_one_refresh(5).processed,
            "the refresh is consumed, not quarantined"
        );
        Spi::run("RESET search_path").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("active"),
            "the bare record-equality qual is canonical, not drift"
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pdocs_chunks
                  WHERE postvec_source_pk = (1,2)::pair_id"
            )
            .unwrap()
            .unwrap()
                > 0,
            "chunks materialized under the composite key"
        );
        Spi::run("SELECT postvec.disable('pdocs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('pdocs_chunks') IS NULL").unwrap(),
            Some(true),
            "the proven destination is dropped"
        );
    }

    /// A `timestamptz`-keyed entry: chunks materialised under one TimeZone
    /// are fully invalidated from a session with another ([R2-1] — the typed
    /// destination key is what makes this hold).
    #[pg_test]
    fn session_rendered_pk_invalidation_leaves_no_stale_chunks() {
        seed_model(3);
        Spi::run("CREATE TABLE ts (at timestamptz PRIMARY KEY, body text)").unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('ts','body','m', chunking => 'recursive',
                                   destination => 'ts_chunks',
                                   chunk_size => 64, chunk_overlap => 8)",
        )
        .unwrap();
        Spi::run("SET TIME ZONE '+02'").unwrap();
        Spi::run("INSERT INTO ts VALUES ('2026-07-04 14:00:00+02', repeat('word ', 40))").unwrap();
        assert_eq!(drain_refreshes(), 1);
        assert!(
            Spi::get_one::<i64>("SELECT count(*) FROM ts_chunks")
                .unwrap()
                .unwrap()
                > 0
        );

        // Invalidate from a session rendering the PK differently.
        Spi::run("SET TIME ZONE 'UTC'").unwrap();
        Spi::run("UPDATE ts SET body = NULL").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM ts_chunks").unwrap(),
            Some(0),
            "no stale chunks survive a differently-rendering session's invalidation"
        );
    }
}
