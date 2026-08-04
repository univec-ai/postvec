//! Cursor backfiller for large tables.
//!
//! `enable(backfill_mode => 'cursor')` skips the one-shot enqueue (which
//! bloats `postvec.jobs` on 10M+ row tables). Instead the worker enqueues
//! watermark-ordered chunks whenever the entry's queue is empty, so the queue
//! never holds more than one chunk of backfill work at a time. The enqueued
//! jobs flow through the normal pipeline (retries, dead-lettering,
//! coalescing with trigger-enqueued jobs via the dedup index).
//!
//! The chunk filter is `source IS NOT NULL AND vector IS NULL`, so rows that
//! got their vector through a trigger in the meantime are skipped for free;
//! rows past the watermark that lose their vector later are the triggers'
//! (not the backfiller's) responsibility, exactly like queue-mode backfill.

use crate::registry::RegistryEntryDb as _;
use crate::registry::{quote_ident, RegistryEntry};
use pgrx::prelude::*;

/// Enqueue the next chunk for every cursor-backfilling entry whose queue is
/// empty. Must run inside a transaction. Returns the number of jobs enqueued.
///
/// A recursive entry's cursor feeds `refresh` jobs whose eligibility
/// predicate is simply source-non-NULL — the watermark, not absence of
/// chunks, prevents revisiting successfully materialized empty documents
/// The feeder rule stays "no live job of either op": a feed produces a
/// batch of documents, each of which can expand to 10,000 children, so the
/// coarser rule composes with the refresh phase's finer CHUNK_INFLIGHT_MAX
/// bound [R2-2]. `recursive_only` restricts a pass to recursive entries —
/// the closed-inference-gate loop uses it so column-mode cursors do not start
/// advancing while inference is down [R2-5].
pub fn enqueue_chunks(chunk: i32, recursive_only: bool) -> i64 {
    let ids: Vec<i64> = Spi::connect(|c| {
        let t = c
            .select(
                &format!(
                    "SELECT id FROM postvec.registry
                      WHERE backfill_mode = 'cursor' AND state = 'active'{extra}
                      ORDER BY id",
                    extra = if recursive_only {
                        " AND chunking = 'recursive'"
                    } else {
                        ""
                    }
                ),
                None,
                &[],
            )
            .expect("postvec: cursor-backfill registry scan failed");
        t.into_iter()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
    });

    let mut enqueued = 0i64;
    for id in ids {
        let Some(entry) = RegistryEntry::load(id) else {
            continue;
        };
        if let Some(reason) = entry.missing_dependency_locked(&[]) {
            // Table/column dropped while backfilling: quarantine instead of
            // aborting the worker on the chunk read, forever.
            crate::api::registry::quarantine_entry(&entry, &reason);
            continue;
        }
        // Only feed an empty queue: one in-flight chunk per entry bounds the
        // queue size, and trigger traffic keeps its priority. EXISTS on the
        // jobs_registry_pk index — a count(*) here matched no index and paid a
        // full seq scan of the jobs table per entry per drain cycle.
        let has_pending = Spi::get_one_with_args::<bool>(
            "SELECT EXISTS (SELECT 1 FROM postvec.jobs WHERE registry_id = $1)",
            &[id.into()],
        )
        .unwrap()
        .unwrap_or(false);
        if has_pending {
            continue;
        }

        let wm = entry
            .backfill_watermark
            .as_deref()
            .map(|w| entry.pk_watermark_clause("", w))
            .unwrap_or_default();
        // Column mode: rows still missing their vector. Recursive mode: the
        // vector lives per chunk in the destination, so eligibility is source
        // non-NULL and the watermark alone prevents revisits.
        let vec_pred = if entry.is_recursive() {
            String::new()
        } else {
            format!(" AND {} IS NULL", quote_ident(&entry.vector_column))
        };
        let pks: Vec<String> = Spi::connect(|c| {
            let q = format!(
                "SELECT {pk} FROM {tbl}
                  WHERE {src} IS NOT NULL{vec_pred}{wm}
                  ORDER BY {order}
                  LIMIT {chunk}",
                pk = entry.pk_text_expr(""),
                tbl = entry.qualified_table(),
                src = quote_ident(&entry.source_column),
                order = entry.pk_order_expr(""),
            );
            let t = c
                .select(q.as_str(), None, &[])
                .expect("postvec: cursor-backfill chunk read failed");
            t.into_iter()
                .map(|r| r.get::<String>(1).unwrap().unwrap())
                .collect()
        });

        if pks.is_empty() {
            Spi::run_with_args(
                "UPDATE postvec.registry
                    SET backfill_mode = 'done', backfill_watermark = NULL
                  WHERE id = $1",
                &[id.into()],
            )
            .unwrap();
            log!(
                "postvec: cursor backfill of {}.{}.{} complete",
                entry.table_schema,
                entry.table_name,
                entry.source_column
            );
            continue;
        }

        let last = pks.last().cloned();
        let op = if entry.is_recursive() {
            "refresh"
        } else {
            "embed"
        };
        Spi::connect_mut(|c| {
            c.update(
                "INSERT INTO postvec.jobs (registry_id, pk_value, op)
                 SELECT $1, unnest($2::text[]), $3
                 ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING",
                None,
                &[id.into(), pks.clone().into(), op.into()],
            )
            .unwrap();
        });
        Spi::run_with_args(
            "UPDATE postvec.registry SET backfill_watermark = $2 WHERE id = $1",
            &[id.into(), last.as_deref().into()],
        )
        .unwrap();
        enqueued += pks.len() as i64;
    }
    enqueued
}

/// Worker-side wrapper: one chunk-feeding pass in its own transaction.
/// Chunk size follows the embed batch size so a chunk drains in a few cycles.
/// A failed pass (e.g. lock_timeout on a blocked user table) is logged and
/// retried on the next cycle instead of crash-looping the worker.
pub fn step() -> i64 {
    step_inner(false)
}

/// The closed-inference-gate variant: recursive entries only [R2-5].
pub fn step_recursive_only() -> i64 {
    step_inner(true)
}

fn step_inner(recursive_only: bool) -> i64 {
    let chunk = crate::gucs::BATCH_SIZE.get().max(1) * 4;
    match crate::worker::try_transaction(move || enqueue_chunks(chunk, recursive_only)) {
        Ok(n) => n,
        Err(e) => {
            warning!("postvec: cursor-backfill pass failed (will retry): {e}");
            0
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use crate::jobs::mock::MockClient;
    use crate::jobs::{apply_group, claim_and_read, embed_with_bisection, split_items};

    fn seed_docs(rows: i32) -> i64 {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO docs (body) SELECT 'row ' || g FROM generate_series(1, $1) g",
            &[rows.into()],
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill_mode => 'cursor')")
            .unwrap()
            .unwrap()
    }

    /// Drain whatever is currently pending through the mock pipeline.
    fn drain_queue() {
        let client = MockClient::new(3);
        loop {
            let groups = claim_and_read(64, 300.0);
            if groups.is_empty() {
                return;
            }
            for g in groups {
                let routing = g.routing.clone();
                let (nj, ej) = split_items(g.items);
                let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
                let outcomes =
                    embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
                apply_group(g.entry.id, &routing, &nj, &ej, &outcomes, 5, 5000);
            }
        }
    }

    #[pg_test]
    fn cursor_backfill_feeds_chunks_until_done() {
        let id = seed_docs(10);

        // First pass: exactly one chunk lands in the queue.
        assert_eq!(enqueue_chunks(4, false), 4);
        // A second pass while the queue holds work feeds nothing.
        assert_eq!(enqueue_chunks(4, false), 0, "waits for the queue to drain");

        // Drain / feed until completion.
        let mut fed = 4i64;
        for _ in 0..10 {
            drain_queue();
            let n = enqueue_chunks(4, false);
            fed += n;
            if n == 0 {
                break;
            }
        }
        assert_eq!(fed, 10, "every row was fed exactly once");
        drain_queue();

        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(10),
            "cursor backfill filled every row"
        );
        let mode = Spi::get_one_with_args::<String>(
            "SELECT backfill_mode FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        assert_eq!(mode.as_deref(), Some("done"));
        // A further pass is a no-op.
        assert_eq!(enqueue_chunks(4, false), 0);
    }

    /// Rows that already have vectors (e.g. filled by triggers while the
    /// cursor was behind) are skipped by the `vec IS NULL` filter.
    #[pg_test]
    fn cursor_backfill_skips_already_filled_rows() {
        seed_docs(6);
        Spi::run("UPDATE docs SET body_semantic = '[1,2,3]'::vector WHERE id <= 4").unwrap();
        assert_eq!(
            enqueue_chunks(100, false),
            2,
            "only the two unfilled rows enqueue"
        );
    }
}
