//! Job-queue engine.
//!
//! Pure SQL step functions over `postvec.jobs`, plus the network-side
//! embed-with-bisection helper. Everything here runs in the ambient
//! transaction. The worker (`crate::worker`) provides the transaction
//! boundaries so the claim commits before the network call (no row locks
//! held during inference) and the write-back commits after it. Tests drive
//! these directly inside the `#[pg_test]` transaction with a mock client.

use crate::client::{EmbedRoute, ErrorClass, InferenceClient, PvError};
use crate::registry::RegistryEntryDb as _;
use crate::registry::{quote_ident, serialize_vector, RegistryEntry};
use crate::runtime;
use pgrx::prelude::*;
use std::collections::BTreeMap;

/// One claimed job with its source text resolved.
#[derive(Debug, Clone)]
pub struct JobItem {
    pub job_id: i64,
    pub pk_value: String,
    /// Destination chunk identity for a recursive child embed job; `None`
    /// for column-mode jobs.
    pub chunk_id: Option<i64>,
    pub attempts: i32,
    /// `None` => the source row is NULL or has since been deleted; no
    /// inference is performed and the shadow value is set NULL.
    pub text: Option<String>,
}

/// Where a group's embeddings must go. Normally the entry's own model and
/// shadow column; while a migration is live (registry `state = 'migrating'`),
/// fresh writes are embedded with the **new** model into the **new** column so
/// they always win over conversions of stale vectors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedRouting {
    pub migration_id: Option<i64>,
    /// Public model name (embed-resolution key).
    pub model: String,
    /// Target vector column.
    pub vector_column: String,
    pub dim: i32,
}

/// Compute the routing for an entry. Must be called inside a transaction.
pub fn embed_routing(entry: &RegistryEntry) -> EmbedRouting {
    if entry.state == "migrating" {
        let live = Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT id, new_model, new_column, new_dim
                      FROM postvec.migrations
                     WHERE registry_id = $1
                        AND state IN ('running','awaiting_finalize')
                      ORDER BY id DESC LIMIT 1",
                    Some(1),
                    &[entry.id.into()],
                )
                .ok()?;
            t.into_iter().next().map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap().unwrap(),
                    r.get::<String>(3).unwrap().unwrap(),
                    r.get::<i32>(4).unwrap().unwrap(),
                )
            })
        });
        if let Some((id, model, column, dim)) = live {
            return EmbedRouting {
                migration_id: Some(id),
                model,
                vector_column: column,
                dim,
            };
        }
    }
    EmbedRouting {
        migration_id: None,
        model: entry.model.clone(),
        vector_column: entry.vector_column.clone(),
        dim: entry.dim,
    }
}

/// Claimed jobs for one registry entry, texts already read and the embed
/// routing captured in the same (claim) transaction.
pub struct GroupWork {
    pub entry: RegistryEntry,
    pub routing: EmbedRouting,
    pub items: Vec<JobItem>,
}

/// Per-text result after (bisecting) inference.
#[derive(Debug, Clone)]
pub enum ItemOutcome {
    /// Embedded successfully.
    Ok(Vec<f32>),
    /// Permanent / poison failure → move to `postvec.jobs_dead`.
    Dead(String),
    /// Retryable failure → exponential backoff (or dead once retries exhausted).
    Retry(String),
}

/// Outcome counts for one processed group (returned for the heartbeat/logs).
#[derive(Debug, Default, Clone, Copy)]
pub struct Applied {
    pub done: i64,
    pub nulled: i64,
    pub retried: i64,
    pub dead: i64,
}

struct Claimed {
    job_id: i64,
    registry_id: i64,
    pk_value: String,
    /// Destination chunk identity for a recursive child embed job.
    chunk_id: Option<i64>,
    attempts: i32,
}

/// Return claims whose visibility timeout expired (worker crash between claim
/// and write-back) to the pending pool. Kept separate from [`claim_batch`] so
/// the hot claim query stays a pure partial-index scan; this one rides the
/// tiny `jobs_reclaim` index (only in-flight jobs are claimed).
///
/// A probe-gated two-step: stale claims only ever exist after a worker crash,
/// so the common case is one O(log n) EXISTS probe and nothing else. When the
/// probe fires: claims whose attempts are already exhausted dead-letter (a
/// job that only ever dies mid-batch — worker crash loop — must not be
/// redelivered forever); the rest go back to pending through [`unclaim_jobs`],
/// which owns the dedup-collision handling, attempts preserved (the next
/// claim bumps them).
fn reclaim_stale(vis_secs: f64, max_retries: i32, registry_ids: &[i64]) {
    if registry_ids.is_empty() {
        return;
    }
    let stale_pred = "claimed_at < now() - make_interval(secs => $1)";
    let any_stale = Spi::get_one_with_args::<bool>(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM postvec.jobs
              WHERE {stale_pred} AND registry_id = ANY($2))"
        ),
        &[vis_secs.into(), registry_ids.to_vec().into()],
    )
    .unwrap()
    .unwrap_or(false);
    if !any_stale {
        return;
    }
    let select_ids = |extra_pred: &str| -> Vec<i64> {
        Spi::connect(|c| {
            let t = c
                .select(
                    &format!(
                        "SELECT id FROM postvec.jobs
                          WHERE {stale_pred} AND registry_id = ANY($3){extra_pred}"
                    ),
                    None,
                    &[
                        vis_secs.into(),
                        max_retries.into(),
                        registry_ids.to_vec().into(),
                    ],
                )
                .expect("postvec: stale-claim scan failed");
            t.into_iter()
                .map(|r| r.get::<i64>(1).unwrap().unwrap())
                .collect()
        })
    };
    move_to_dead(
        &select_ids(" AND attempts > $2"),
        "visibility timeout expired with attempts exhausted (worker crash mid-batch?)",
    );
    unclaim_jobs(
        &select_ids(""),
        0.0,
        "visibility timeout expired; reclaimed",
        true,
    );
}

/// Resolve and pin every relation whose jobs this transaction may touch.
///
/// Lifecycle verbs take relation locks before registry/job rows. Claiming a
/// job and only then letting the generated SELECT acquire ACCESS SHARE would
/// invert that order against `disable()`/quarantine (job -> table versus
/// table -> job), leaving PostgreSQL's deadlock detector to abort one side.
/// This lock-free candidate peek may be stale, which is harmless: the actual
/// claim is restricted to the returned IDs, and a new candidate waits for the
/// next cycle. Invalid entries are quarantined while the correct relation
/// locks are held, before any job row is claimed.
fn prepare_claim_entries(batch: i32, vis_secs: f64) -> Vec<i64> {
    let ids: Vec<i64> = Spi::connect(|c| {
        let table = c
            .select(
                "SELECT DISTINCT registry_id
                   FROM (
                         (SELECT registry_id FROM postvec.jobs
                           WHERE claimed_at IS NULL AND not_before <= now()
                             AND op = 'embed'
                           ORDER BY not_before, id LIMIT $1)
                         UNION ALL
                         (SELECT registry_id FROM postvec.jobs
                           WHERE claimed_at < now() - make_interval(secs => $2)
                           ORDER BY claimed_at, id LIMIT $1)
                        ) candidates
                  ORDER BY registry_id",
                None,
                &[(batch as i64).into(), vis_secs.into()],
            )
            .expect("postvec: claim candidate scan failed");
        table
            .into_iter()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
    });

    let mut prepared = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(entry) = RegistryEntry::load(id) else {
            // Keep orphan IDs eligible so the normal claim path can delete
            // their jobs; no user relation exists to pin.
            prepared.push(id);
            continue;
        };
        if entry.state == "disabled" {
            // Disabled jobs are cleanup-only and never lead to generated SQL.
            prepared.push(id);
            continue;
        }
        if let Some(reason) = entry.missing_dependency_locked(&[]) {
            crate::api::registry::quarantine_entry(&entry, &reason);
            continue;
        }
        prepared.push(id);
    }
    prepared
}

/// Claim up to `batch` due jobs with `FOR UPDATE SKIP LOCKED` and mark them
/// claimed (bumping `attempts`). The claim transaction is expected to commit
/// before any network I/O; a crash between claim and write-back re-delivers
/// after `vis_secs` (the visibility timeout, via [`reclaim_stale`]).
///
/// Ordered by `(not_before, id)` — FIFO by due time, matching the
/// `jobs_embed_claim_order` partial index exactly, so a claim is an ordered index
/// scan that stops at the LIMIT (no per-claim sort of the whole backlog) and
/// no entry can starve another.
fn claim_batch(batch: i32, vis_secs: f64) -> Vec<Claimed> {
    let registry_ids = prepare_claim_entries(batch, vis_secs);
    if registry_ids.is_empty() {
        return Vec::new();
    }
    reclaim_stale(vis_secs, crate::gucs::MAX_RETRIES.get(), &registry_ids);
    Spi::connect_mut(|client| {
        // This claimant takes only `embed` jobs (column-mode source rows
        // and recursive chunk children). The op predicate is what makes
        // `jobs_embed_claim_order` exact. Refresh jobs are claimed one at
        // a time by the local refresh phase (`crate::worker::chunk`).
        let table = client
            .update(
                "UPDATE postvec.jobs j
                    SET claimed_at = now(), attempts = attempts + 1
                  WHERE j.id IN (
                      SELECT id FROM postvec.jobs
                       WHERE claimed_at IS NULL AND not_before <= now()
                         AND op = 'embed'
                         AND registry_id = ANY($2)
                       ORDER BY not_before, id
                       LIMIT $1
                       FOR UPDATE SKIP LOCKED)
                RETURNING id, registry_id, pk_value, chunk_id, attempts",
                None,
                &[(batch as i64).into(), registry_ids.into()],
            )
            .expect("postvec: claim query failed");
        table
            .into_iter()
            .map(|r| Claimed {
                job_id: r.get::<i64>(1).unwrap().unwrap(),
                registry_id: r.get::<i64>(2).unwrap().unwrap(),
                pk_value: r.get::<String>(3).unwrap().unwrap(),
                chunk_id: r.get::<i64>(4).unwrap(),
                attempts: r.get::<i32>(5).unwrap().unwrap(),
            })
            .collect()
    })
}

/// Dead-letter claimed jobs whose `pk_value` cannot be cast back to the
/// (single-column) PK's native type — `pg_input_is_valid` applies the same
/// input conversion (including the typmod) as the staging-side casts in
/// [`RegistryEntry::pk_staging_join_clause`] / `pk_any_clause`, so anything
/// that survives this filter cannot abort the read or write-back SQL.
/// Returns the castable remainder. Runs in the claim transaction, so the
/// dead-letter commits with the claim.
fn dead_letter_uncastable_keys(entry: &RegistryEntry, claims: Vec<Claimed>) -> Vec<Claimed> {
    let pks: Vec<String> = claims.iter().map(|c| c.pk_value.clone()).collect();
    let invalid: Vec<String> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT d.pk FROM unnest($1::text[]) AS d(pk)
                  WHERE NOT pg_input_is_valid(d.pk, $2)",
                None,
                &[pks.into(), entry.pk_types[0].as_str().into()],
            )
            .expect("postvec: pk validity scan failed");
        t.into_iter()
            .map(|r| r.get::<String>(1).unwrap().unwrap())
            .collect()
    });
    if invalid.is_empty() {
        return claims;
    }
    let (bad, good): (Vec<Claimed>, Vec<Claimed>) = claims
        .into_iter()
        .partition(|c| invalid.contains(&c.pk_value));
    let bad_ids: Vec<i64> = bad.iter().map(|c| c.job_id).collect();
    warning!(
        "postvec: dead-lettering {} job(s) for registry id {} whose pk_value is not \
         valid {} input (direct INSERT into postvec.jobs?)",
        bad_ids.len(),
        entry.id,
        entry.pk_types[0]
    );
    move_to_dead(
        &bad_ids,
        &format!(
            "pk_value is not valid input for PK type {} (direct queue insert?)",
            entry.pk_types[0]
        ),
    );
    good
}

/// Read `<queued pk text> -> <document text>` for the given pks — the raw
/// `source::text`, or the format-template rendering when the entry carries a
/// format (source text is rendered at claim time, never stored in the job, so
/// re-driven and old pending jobs always use the current template). Rows that
/// are absent or whose source is NULL map to `None` (the template's CASE
/// anchor renders SQL NULL for a NULL source, whatever the context columns
/// hold).
///
/// The result is keyed by the **queued** text (via an unnest join), not by
/// re-rendering the row's `pk::text` — for single-column PKs the join compares
/// in the native type, so a PK type whose text rendering depends on session
/// settings (e.g. `timestamptz` under a different TimeZone) still matches the
/// key the trigger enqueued.
///
/// Byte budgets are enforced **inside the SQL**, before any rendered text
/// crosses into Rust: a row whose rendered text exceeds `item_cap` comes back
/// with a NULL text and its measured length (the caller dead-letters it — no
/// truncation, which would change embedding semantics and could split UTF-8);
/// rows past the cumulative `budget` (in claim order) come back marked
/// deferred (the caller returns them to pending without consuming the
/// attempt). Oversized text is stripped in the projection, so the Rust-side
/// aggregate stays bounded by `budget` no matter what the table holds.
fn read_sources(
    entry: &RegistryEntry,
    pks: &[String],
    item_cap: i64,
    budget: i64,
) -> BTreeMap<String, SourceRead> {
    let mut map = BTreeMap::new();
    if pks.is_empty() {
        return map;
    }
    // Phase 1: LENGTHS only. `format_len_expr` sums octet_lengths without
    // ever building the rendered concatenation, so the database does not
    // materialize a single over-ceiling row to measure it (the earlier
    // LATERAL form rendered every claimed row and then projected NULL).
    let len_q = format!(
        "SELECT d.pk, {len_expr} AS len
           FROM unnest($1::text[]) WITH ORDINALITY AS d(pk, ord)
           JOIN {tbl} t ON {join}
          ORDER BY d.ord",
        len_expr = crate::registry::format_len_expr(entry, "t"),
        tbl = entry.qualified_table(),
        join = entry.pk_staging_join_clause("t", "d", "pk"),
    );
    let lens: Vec<(String, Option<i64>)> = Spi::connect(|client| {
        let table = client
            .select(len_q.as_str(), None, &[pks.to_vec().into()])
            .expect("postvec: read_sources length scan failed");
        table
            .into_iter()
            .map(|row| {
                (
                    row.get::<String>(1).unwrap().unwrap(),
                    row.get::<i64>(2).unwrap(),
                )
            })
            .collect()
    });

    // Rust-side admission in claim order (FIFO: once the budget closes, every
    // later in-cap row defers — no cherry-picking).
    let mut admitted: Vec<String> = Vec::new();
    let mut admitted_lens: Vec<i64> = Vec::new();
    let mut used = 0i64;
    let mut budget_closed = false;
    for (pk, len) in lens {
        match len {
            None => {
                map.insert(pk, SourceRead::Null);
            }
            Some(l) if l > item_cap => {
                map.insert(pk, SourceRead::TooLarge(l));
            }
            Some(l) if !budget_closed && used + l <= budget => {
                used += l;
                admitted.push(pk);
                admitted_lens.push(l);
            }
            Some(_) => {
                budget_closed = true;
                map.insert(pk, SourceRead::Deferred);
            }
        }
    }

    // Phase 2: render ONLY the admitted rows — the one allocation that has
    // to happen, because these rows are being sent.
    if !admitted.is_empty() {
        // Per-row admitted-length recheck. Under READ COMMITTED, a
        // concurrent UPDATE between the phases can enlarge a row after
        // admission; rendering it anyway would break the byte ceiling.
        // The render is suppressed unless the current length still fits
        // the length this row was admitted at, so the total rendered can
        // never exceed the admitted budget. A grown row defers and is
        // re-measured next cycle.
        let q = format!(
            "SELECT d.pk,
                    CASE WHEN ({len_expr}) <= d.len THEN ({expr})::text END,
                    ({len_expr}) IS NULL AS now_null
               FROM unnest($1::text[], $2::int8[]) AS d(pk, len)
               JOIN {tbl} t ON {join}",
            len_expr = crate::registry::format_len_expr(entry, "t"),
            expr = crate::registry::format_expr(entry, "t"),
            tbl = entry.qualified_table(),
            join = entry.pk_staging_join_clause("t", "d", "pk"),
        );
        Spi::connect(|client| {
            let table = client
                .select(q.as_str(), None, &[admitted.into(), admitted_lens.into()])
                .expect("postvec: read_sources render failed");
            for row in table {
                let pk = row.get::<String>(1).unwrap().unwrap();
                let txt = row.get::<String>(2).unwrap();
                let now_null = row.get::<bool>(3).unwrap().unwrap_or(false);
                match (txt, now_null) {
                    (Some(t), _) => {
                        map.insert(pk, SourceRead::Text(t));
                    }
                    // The source went NULL between the phases: the NULL path.
                    (None, true) => {
                        map.insert(pk, SourceRead::Null);
                    }
                    // Grew past its admitted length between the phases: the
                    // concurrent writer's trigger has (or will) re-enqueue
                    // the row; defer and re-measure next cycle.
                    (None, false) => {
                        map.insert(pk, SourceRead::Deferred);
                    }
                }
            }
            // A row deleted between the phases stays absent from the map —
            // claim_and_read already treats that as the NULL path.
        });
    }
    map
}

/// One row's outcome from a budgeted source read.
enum SourceRead {
    /// Rendered text, within both budgets.
    Text(String),
    /// The source rendered NULL (NULL source column).
    Null,
    /// Rendered text exceeds the per-item ceiling — dead-letter (carries the
    /// measured byte length for the message).
    TooLarge(i64),
    /// Within the item ceiling but past the batch byte budget — return to
    /// pending without consuming the attempt.
    Deferred,
}

/// The dead-letter message for an over-ceiling rendered document.
fn too_large_error(len: i64, item_cap: i64) -> String {
    format!(
        "rendered text is {len} bytes; the ceiling is {item_cap} \
         (min of postvec.max_document_bytes, postvec.max_batch_total_bytes, and the \
         48 MiB wire-message ceiling); postvec never truncates — shorten the \
         source/template or raise the ceiling, then postvec.retry_dead()"
    )
}

/// Claim a batch and read its source texts, grouped by registry entry. Call
/// inside one transaction (the worker's claim/read txn).
///
/// Byte ceilings come from `postvec.max_document_bytes` /
/// `postvec.max_batch_total_bytes` (see [`claim_and_read_budgeted`]).
pub fn claim_and_read(batch: i32, vis_secs: f64) -> Vec<GroupWork> {
    claim_and_read_budgeted(
        batch,
        vis_secs,
        effective_item_cap(),
        crate::gucs::MAX_BATCH_TOTAL_BYTES.get() as i64,
    )
}

/// Cross-session regression seam: run the production claim/read transaction
/// body and report how many registry groups it produced.
#[cfg(feature = "pg_test_concurrency")]
#[pg_extern]
fn __test_claim_and_read() -> i64 {
    claim_and_read(64, 300.0).len() as i64
}

/// `item_cap`/`budget` are the byte ceilings (`postvec.max_document_bytes`,
/// `postvec.max_batch_total_bytes`): a rendered text over the per-item cap
/// dead-letters with its measured size, and once the summed kept bytes reach
/// the budget the remaining claims are returned to pending with their claim
/// attempt refunded — capacity scheduling, not failure. The effective item cap
/// is `min(item_cap, budget)`: an item that can never fit inside one batch
/// budget would otherwise defer forever.
pub fn claim_and_read_budgeted(
    batch: i32,
    vis_secs: f64,
    item_cap: i64,
    budget: i64,
) -> Vec<GroupWork> {
    let budget = budget.max(1);
    let item_cap = item_cap.clamp(1, budget);
    let mut remaining = budget;
    let claimed = claim_batch(batch, vis_secs);
    if claimed.is_empty() {
        return Vec::new();
    }
    let mut by_reg: BTreeMap<i64, Vec<Claimed>> = BTreeMap::new();
    for c in claimed {
        by_reg.entry(c.registry_id).or_default().push(c);
    }

    let mut groups = Vec::new();
    for (registry_id, claims) in by_reg {
        let Some(entry) = RegistryEntry::load(registry_id) else {
            // Registry row vanished (disable/drop race): drop the orphan jobs.
            let ids: Vec<i64> = claims.iter().map(|c| c.job_id).collect();
            delete_jobs(&ids);
            continue;
        };
        if entry.state == "disabled" {
            // disable() raced the claim; its own job purge missed these.
            let ids: Vec<i64> = claims.iter().map(|c| c.job_id).collect();
            delete_jobs(&ids);
            continue;
        }
        if let Some(reason) = entry.missing_dependency(&[]) {
            // Table or column dropped while enabled: without this, reading
            // the source texts would abort the worker on the same error every
            // pass. `claim_batch` already pinned healthy definitions before
            // claiming; this is a cheap defensive re-read. Quarantine purges
            // the entry's jobs, ours included.
            crate::api::registry::quarantine_entry(&entry, &reason);
            continue;
        }
        // PUBLIC can INSERT into postvec.jobs directly (the SECURITY INVOKER
        // triggers need it), so a queued key is not guaranteed to cast back
        // to the PK's native type. An uncastable key would abort this whole
        // claim/read transaction — rolling back the attempts bump, so the
        // same poison job re-selects every cycle, forever. Dead-letter such
        // keys here instead (composite keys compare as text; no cast).
        let claims = if entry.is_composite_pk() {
            claims
        } else {
            dead_letter_uncastable_keys(&entry, claims)
        };
        if claims.is_empty() {
            continue;
        }
        // Batch byte budget exhausted by earlier groups: defer this whole
        // group (attempt refunded) rather than reading more text this cycle.
        if remaining <= 0 {
            let ids: Vec<i64> = claims.iter().map(|c| c.job_id).collect();
            defer_jobs(&ids);
            continue;
        }
        let items: Vec<JobItem> = if entry.is_recursive() {
            read_chunk_items(&entry, claims, item_cap, &mut remaining)
        } else {
            let pks: Vec<String> = claims.iter().map(|c| c.pk_value.clone()).collect();
            let mut sources = read_sources(&entry, &pks, item_cap, remaining);
            let mut items = Vec::new();
            let mut deferred: Vec<i64> = Vec::new();
            for c in claims {
                match sources.remove(&c.pk_value) {
                    Some(SourceRead::Text(t)) => {
                        remaining -= t.len() as i64;
                        items.push(JobItem {
                            job_id: c.job_id,
                            pk_value: c.pk_value,
                            chunk_id: c.chunk_id,
                            attempts: c.attempts,
                            text: Some(t),
                        });
                    }
                    Some(SourceRead::TooLarge(len)) => {
                        move_to_dead(&[c.job_id], &too_large_error(len, item_cap));
                    }
                    Some(SourceRead::Deferred) => deferred.push(c.job_id),
                    // NULL source, or the row is gone: the NULL path.
                    Some(SourceRead::Null) | None => items.push(JobItem {
                        job_id: c.job_id,
                        pk_value: c.pk_value,
                        chunk_id: c.chunk_id,
                        attempts: c.attempts,
                        text: None,
                    }),
                }
            }
            defer_jobs(&deferred);
            items
        };
        if items.is_empty() {
            continue;
        }
        let routing = embed_routing(&entry);
        groups.push(GroupWork {
            entry,
            routing,
            items,
        });
    }
    groups
}

/// Resolve a recursive entry's claimed child jobs into embeddable items,
/// applying the claim-time validity matrix:
///
/// - an `embed` job without a `chunk_id` on a chunked entry is malformed (a
///   direct PUBLIC queue insert) and dead-letters
/// - a missing chunk row means an update/delete made the job obsolete: the
///   job is deleted without inference
/// - a chunk whose stored source key does not match the queued key is
///   malformed input and dead-letters rather than reading another document
/// - a chunk whose source row is gone, or whose template renders NULL
///   (NULL source), is obsolete. The triggers have already purged or will
///   purge it; the job is deleted without inference. Chunked entries never
///   produce NULL-text items: the column-mode "write NULL vector" path
///   would target the wrong relation.
///
/// The render happens here, at claim time. Re-driven and stale pending
/// jobs always use the current template. Byte ceilings ride the same SQL
/// shape as [`read_sources`]: an over-`item_cap` chunk render (the splitter
/// bounds `$chunk`, but the chunk template can add arbitrary context
/// columns) dead-letters with its measured size; renders past the running
/// `remaining` budget defer with the attempt refunded. `remaining` is
/// decremented by the kept bytes.
fn read_chunk_items(
    entry: &RegistryEntry,
    claims: Vec<Claimed>,
    item_cap: i64,
    remaining: &mut i64,
) -> Vec<JobItem> {
    let (chunked, malformed): (Vec<Claimed>, Vec<Claimed>) =
        claims.into_iter().partition(|c| c.chunk_id.is_some());
    if !malformed.is_empty() {
        let ids: Vec<i64> = malformed.iter().map(|c| c.job_id).collect();
        move_to_dead(
            &ids,
            "malformed queue row: a chunked entry's embed jobs are chunk-keyed \
             (direct queue insert?)",
        );
    }
    if chunked.is_empty() {
        return Vec::new();
    }
    // Phase 1: identity checks + LENGTHS only — `chunk_format_len_expr`
    // never builds the rendered document, so an over-ceiling chunk template
    // is measured, not materialized.
    let len_q = format!(
        "SELECT s.cid,
                (c.postvec_chunk_id IS NOT NULL) AS chunk_found,
                (c.postvec_source_pk IS NOT DISTINCT FROM s.pk::{pk_type}) AS pk_matches,
                {len_expr} AS len
           FROM unnest($1::int8[], $2::text[]) WITH ORDINALITY AS s(cid, pk, ord)
           LEFT JOIN {qdest} c ON c.postvec_chunk_id = s.cid
           LEFT JOIN {qsrc} d ON {src_join}
          ORDER BY s.ord",
        pk_type = entry.pk_types[0],
        len_expr = crate::registry::chunk_format_len_expr(entry, "d", "c"),
        qdest = entry.qualified_vector_table(),
        qsrc = entry.qualified_table(),
        src_join = entry.source_pk_join("d", "c"),
    );
    let cids: Vec<i64> = chunked.iter().map(|c| c.chunk_id.unwrap()).collect();
    let pks: Vec<String> = chunked.iter().map(|c| c.pk_value.clone()).collect();
    type ChunkLen = (bool, bool, Option<i64>);
    let resolved: BTreeMap<i64, ChunkLen> = Spi::connect(|client| {
        let t = client
            .select(len_q.as_str(), None, &[cids.into(), pks.into()])
            .expect("postvec: chunk claim length scan failed");
        t.into_iter()
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap(),
                    (
                        r.get::<bool>(2).unwrap().unwrap_or(false),
                        r.get::<bool>(3).unwrap().unwrap_or(false),
                        r.get::<i64>(4).unwrap(),
                    ),
                )
            })
            .collect()
    });

    // Rust-side admission in claim order, FIFO like read_sources.
    let mut admitted: Vec<&Claimed> = Vec::new();
    let mut admitted_lens: Vec<i64> = Vec::new();
    let mut obsolete: Vec<i64> = Vec::new();
    let mut mismatched: Vec<i64> = Vec::new();
    let mut deferred: Vec<i64> = Vec::new();
    let mut budget_closed = false;
    for c in &chunked {
        let cid = c.chunk_id.unwrap();
        match resolved.get(&cid) {
            Some((true, true, Some(len))) if *len > item_cap => {
                move_to_dead(&[c.job_id], &too_large_error(*len, item_cap));
            }
            Some((true, true, Some(len))) if !budget_closed && *len <= *remaining => {
                *remaining -= len;
                admitted.push(c);
                admitted_lens.push(*len);
            }
            Some((true, true, Some(_))) => {
                budget_closed = true;
                deferred.push(c.job_id);
            }
            Some((true, false, _)) => mismatched.push(c.job_id),
            // Chunk gone, source gone, or NULL-source render: obsolete.
            _ => obsolete.push(c.job_id),
        }
    }
    delete_jobs(&obsolete);
    defer_jobs(&deferred);
    if !mismatched.is_empty() {
        move_to_dead(
            &mismatched,
            "chunk identity does not belong to the queued source key (malformed queue row)",
        );
    }
    if admitted.is_empty() {
        return Vec::new();
    }

    // Phase 2: render ONLY the admitted chunks, each rechecked against the
    // length it was admitted at (a context column can grow between the
    // phases under READ COMMITTED; the chunk text itself is written once).
    let text_q = format!(
        "SELECT s.cid,
                CASE WHEN ({len_expr}) <= s.len THEN ({expr})::text END
           FROM unnest($1::int8[], $2::text[], $3::int8[]) AS s(cid, pk, len)
           LEFT JOIN {qdest} c ON c.postvec_chunk_id = s.cid
           LEFT JOIN {qsrc} d ON {src_join}",
        len_expr = crate::registry::chunk_format_len_expr(entry, "d", "c"),
        expr = crate::registry::chunk_format_expr(entry, "d", "c"),
        qdest = entry.qualified_vector_table(),
        qsrc = entry.qualified_table(),
        src_join = entry.source_pk_join("d", "c"),
    );
    let adm_cids: Vec<i64> = admitted.iter().map(|c| c.chunk_id.unwrap()).collect();
    let adm_pks: Vec<String> = admitted.iter().map(|c| c.pk_value.clone()).collect();
    let mut texts: BTreeMap<i64, String> = Spi::connect(|client| {
        let t = client
            .select(
                text_q.as_str(),
                None,
                &[adm_cids.into(), adm_pks.into(), admitted_lens.into()],
            )
            .expect("postvec: chunk claim render failed");
        t.into_iter()
            .filter_map(|r| {
                let cid = r.get::<i64>(1).unwrap().unwrap();
                r.get::<String>(2).unwrap().map(|txt| (cid, txt))
            })
            .collect()
    });

    let mut items = Vec::new();
    let mut vanished: Vec<i64> = Vec::new();
    for c in admitted {
        let cid = c.chunk_id.unwrap();
        match texts.remove(&cid) {
            Some(text) => items.push(JobItem {
                job_id: c.job_id,
                pk_value: c.pk_value.clone(),
                chunk_id: c.chunk_id,
                attempts: c.attempts,
                text: Some(text),
            }),
            // Replaced/deleted between the phases: obsolete, like phase 1.
            None => vanished.push(c.job_id),
        }
    }
    delete_jobs(&vanished);
    items
}

/// A job whose source is NULL/gone: `(job_id, pk_value)`.
pub type NullJob = (i64, String);
/// A job carrying text to embed:
/// `(job_id, pk_value, chunk_id, attempts, text)` — `chunk_id` is the
/// destination chunk identity for a recursive child job, `None` in column
/// mode.
pub type EmbedJob = (i64, String, Option<i64>, i32, String);

/// Split a group's items into the NULL-source jobs (no inference) and the
/// jobs that carry text to embed.
pub fn split_items(items: Vec<JobItem>) -> (Vec<NullJob>, Vec<EmbedJob>) {
    let mut null_jobs = Vec::new();
    let mut embed_jobs = Vec::new();
    for it in items {
        match it.text {
            None => null_jobs.push((it.job_id, it.pk_value)),
            Some(t) => embed_jobs.push((it.job_id, it.pk_value, it.chunk_id, it.attempts, t)),
        }
    }
    (null_jobs, embed_jobs)
}

/// The bisection core shared by the queue and the migration driver: embed
/// `texts`, isolating `ContextLengthExceeded` poison rows by recursive
/// halving. Runs `block_on` on the current-thread runtime — the network
/// phase, between transactions.
///
/// Per row (parallel to the input, in order): `Ok(vector)` or `Err(reason)`
/// for an isolated poison row. Any *other* failure — transport, transient,
/// permanent, count mismatch — aborts the whole call as the outer `Err`, so
/// each caller applies its own batch policy: the queue maps it to uniform
/// per-row Retry/Dead outcomes; the migration driver retries or fails the
/// batch wholesale with the watermark held.
pub fn bisect_embed<C: InferenceClient>(
    client: &C,
    model_internal: &str,
    route: &EmbedRoute,
    timeout_ms: u64,
    texts: &[String],
) -> Result<Vec<Result<Vec<f32>, String>>, PvError> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let res = runtime::block_on_with_timeout(timeout_ms, async {
        client.embed(texts, model_internal, route).await
    });
    match res {
        Ok(vecs) if vecs.len() == texts.len() => Ok(vecs.into_iter().map(Ok).collect()),
        Ok(vecs) => Err(PvError::Decode(format!(
            "embedding count mismatch: got {}, want {}",
            vecs.len(),
            texts.len()
        ))),
        Err(e) if matches!(e.class(), ErrorClass::PoisonRow) => {
            if texts.len() == 1 {
                Ok(vec![Err(format!("{e}"))])
            } else {
                let mid = texts.len() / 2;
                let mut out =
                    bisect_embed(client, model_internal, route, timeout_ms, &texts[..mid])?;
                out.extend(bisect_embed(
                    client,
                    model_internal,
                    route,
                    timeout_ms,
                    &texts[mid..],
                )?);
                Ok(out)
            }
        }
        Err(e) => Err(e),
    }
}

/// The gRPC response for a batch is `count × dim` doubles in a protobuf
/// `Value` tree (~12 wire bytes per float, several times that while
/// decoding). The request side is byte-budgeted, but a batch of many SHORT
/// texts against a high-dimensional model can still produce a response that
/// trips the transport's decode ceiling and fails the whole batch
/// permanently. This budget bounds one call's expected response wire size;
/// [`max_items_for_dim`] converts it to an item count.
const RESPONSE_WIRE_BUDGET_BYTES: u64 = 48 * 1024 * 1024;
const WIRE_BYTES_PER_FLOAT: u64 = 12;
/// A response exists first as the engine's `serde_json::Value` tree and then,
/// while that tree is still alive, as tonic/prost values. Ninety-six bytes per
/// component is a deliberately conservative combined accounting unit;
/// bounding this separately from protobuf wire bytes prevents a wire-valid
/// batch from multiplying into hundreds of MiB in the embedded launcher.
const RESPONSE_TREE_BUDGET_BYTES: u64 = 96 * 1024 * 1024;
const TRANSIENT_TREE_BYTES_PER_FLOAT: u64 = 96;
const TRANSIENT_TREE_BYTES_PER_ITEM: u64 = 256;

/// How many embeddings of `dim` dimensions fit one response budget. Always
/// at least 1 (a single embedding always fits any real configuration).
pub(crate) fn max_items_for_dim(dim: i32) -> usize {
    let dim = dim.max(1) as u64;
    let wire = RESPONSE_WIRE_BUDGET_BYTES / dim.saturating_mul(WIRE_BYTES_PER_FLOAT);
    let trees = RESPONSE_TREE_BUDGET_BYTES
        / dim
            .saturating_mul(TRANSIENT_TREE_BYTES_PER_FLOAT)
            .saturating_add(TRANSIENT_TREE_BYTES_PER_ITEM);
    wire.min(trees).max(1).min(MAX_REQUEST_ITEMS as u64) as usize
}

/// Absolute per-request item ceiling, independent of dimension. The
/// wire-derived bound alone admits millions of items at small dimensions,
/// and per-item container overhead (protobuf framing, `Vec`/`Value`/
/// `ListValue` headers on both server trees) dominates memory for tiny
/// rows. Byte-budget arithmetic that only counts components cannot see
/// that. 4096 items per gRPC call is far above any real embedding batch;
/// the client's chunking inherits this cap through [`max_items_for_dim`],
/// and the embedded server refuses above it before touching a request's
/// items.
pub(crate) const MAX_REQUEST_ITEMS: usize = 4096;

/// The request-side twin of [`max_items_for_dim`]: one call's summed text
/// bytes. Kept BELOW the transport's 64 MiB encode ceiling regardless of how
/// high an operator raises `postvec.max_batch_total_bytes` (whose hard
/// maximum, 256 MiB, deliberately exceeds one wire message — the batch then
/// simply spans several calls).
const REQUEST_WIRE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

/// The per-item byte ceiling every ingestion path enforces: the minimum of
/// the two GUCs and the request wire budget. An item larger than one wire
/// message can never be delivered, so it must be rejected/dead-lettered up
/// front — never emitted as an oversized single-item chunk for the transport
/// to bounce.
pub(crate) fn effective_item_cap() -> i64 {
    (crate::gucs::MAX_DOCUMENT_BYTES.get() as i64)
        .min(crate::gucs::MAX_BATCH_TOTAL_BYTES.get() as i64)
        .min(REQUEST_WIRE_BUDGET_BYTES as i64)
        .max(1)
}

/// Split `texts` into consecutive sub-batches bounded by BOTH the response
/// budget for `dim` (item count) and the request wire budget (summed bytes).
/// Every chunk holds at least one item, so the caller always progresses
/// (items over the wire budget are excluded upstream by
/// [`effective_item_cap`]; the single-item fallback here is a belt, not a
/// path).
pub(crate) fn request_chunks(texts: &[String], dim: i32) -> Vec<&[String]> {
    let max_items = max_items_for_dim(dim);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < texts.len() {
        let mut end = start + 1;
        let mut bytes = texts[start].len();
        while end < texts.len()
            && end - start < max_items
            && bytes.saturating_add(texts[end].len()) <= REQUEST_WIRE_BUDGET_BYTES
        {
            bytes = bytes.saturating_add(texts[end].len());
            end += 1;
        }
        chunks.push(&texts[start..end]);
        start = end;
    }
    chunks
}

/// [`embed_with_bisection`], sub-batched so no single call's response can
/// exceed the wire budget for the target dimension, nor its request the wire
/// request budget. Outcomes concatenate in input order.
pub fn embed_with_bisection_bounded<C: InferenceClient>(
    client: &C,
    model_internal: &str,
    route: &EmbedRoute,
    timeout_ms: u64,
    texts: &[String],
    dim: i32,
) -> Vec<ItemOutcome> {
    let mut out = Vec::with_capacity(texts.len());
    for chunk in request_chunks(texts, dim) {
        out.extend(embed_with_bisection(
            client,
            model_internal,
            route,
            timeout_ms,
            chunk,
        ));
    }
    out
}

/// The queue's mapping over [`bisect_embed`]: per-row outcomes ready for
/// [`apply_group`]. Poison rows and non-finite embeddings (which pgvector
/// would reject, failing the whole write-back batch) become per-row `Dead`;
/// a whole-batch failure becomes uniform `Dead` (permanent) or `Retry`
/// (transient/config) outcomes.
pub fn embed_with_bisection<C: InferenceClient>(
    client: &C,
    model_internal: &str,
    route: &EmbedRoute,
    timeout_ms: u64,
    texts: &[String],
) -> Vec<ItemOutcome> {
    match bisect_embed(client, model_internal, route, timeout_ms, texts) {
        Ok(rows) => rows
            .into_iter()
            .map(|row| match row {
                Ok(v) if v.iter().all(|f| f.is_finite()) => ItemOutcome::Ok(v),
                Ok(_) => {
                    ItemOutcome::Dead("model returned a non-finite embedding (NaN/Inf)".into())
                }
                Err(poison) => ItemOutcome::Dead(poison),
            })
            .collect(),
        Err(e) => {
            let msg = format!("{e}");
            let outcome = match e.class() {
                ErrorClass::Permanent => ItemOutcome::Dead(msg),
                _ => ItemOutcome::Retry(msg),
            };
            texts.iter().map(|_| outcome.clone()).collect()
        }
    }
}

/// When resolution/config makes the whole group unembeddable before any
/// network call, produce a uniform retry outcome per embed job.
pub fn all_retry(n: usize, msg: &str) -> Vec<ItemOutcome> {
    (0..n)
        .map(|_| ItemOutcome::Retry(msg.to_string()))
        .collect()
}

/// Apply a group's outcomes: write vectors / NULLs, delete finished jobs,
/// back off retries, and dead-letter poison/permanent/exhausted jobs. Call
/// inside one transaction (the worker's write-back txn).
///
/// The registry entry and the embed routing are **re-read inside this
/// transaction** and compared to `expected` (the routing captured at claim
/// time). If a migration started, finalized, or was aborted while the network
/// call was in flight, the embedded vectors were made for the wrong
/// model/column — the jobs are released for reprocessing instead of written.
pub fn apply_group(
    registry_id: i64,
    expected: &EmbedRouting,
    null_jobs: &[NullJob],
    embed_jobs: &[EmbedJob],
    outcomes: &[ItemOutcome],
    max_retries: i32,
    backoff_ms: i64,
) -> Applied {
    let mut applied = Applied::default();

    let Some(entry) = RegistryEntry::load(registry_id) else {
        // Registry row vanished (disable/drop race): drop the orphan jobs.
        let ids: Vec<i64> = null_jobs
            .iter()
            .map(|(id, _)| *id)
            .chain(embed_jobs.iter().map(|(id, _, _, _, _)| *id))
            .collect();
        delete_jobs(&ids);
        return applied;
    };
    if entry.state == "disabled" {
        // disable() landed while the network call was in flight: it already
        // purged the jobs and may have dropped the vector column — write
        // nothing (a disabled column must not be resurrected).
        let ids: Vec<i64> = null_jobs
            .iter()
            .map(|(id, _)| *id)
            .chain(embed_jobs.iter().map(|(id, _, _, _, _)| *id))
            .collect();
        delete_jobs(&ids);
        return applied;
    }
    let fresh = embed_routing(&entry);
    if let Some(reason) = entry.missing_dependency_locked(&[]) {
        // Table or one of the entry's own columns dropped mid-flight: the
        // write-back would abort the worker. Quarantine (which also purges
        // this group's jobs).
        crate::api::registry::quarantine_entry(&entry, &reason);
        return applied;
    }
    if fresh.vector_column != entry.vector_column {
        if let Some(reason) = entry.missing_dependency_locked(&[&fresh.vector_column]) {
            // Only the live migration's `_new` column is gone (dropped
            // manually mid-flight). Mirror the migration driver: fail the
            // migration and release the jobs — quarantining here would tear
            // down the whole entry for a recoverable situation. Once failed,
            // routing points back at the old column and the released jobs
            // reprocess correctly.
            if let Some(mid) = fresh.migration_id {
                crate::worker::migrate::fail_migration(mid, &reason);
            }
            let ids: Vec<i64> = null_jobs
                .iter()
                .map(|(id, _)| *id)
                .chain(embed_jobs.iter().map(|(id, _, _, _, _)| *id))
                .collect();
            unclaim_jobs(&ids, 0.0, &reason, false);
            applied.retried += ids.len() as i64;
            return applied;
        }
    }

    // 1. NULL-source rows: null the shadow column(s) (if the row still exists)
    //    and finish the jobs. Uses the *fresh* state: while migrating, both
    //    the old and the new column are nulled so neither goes stale.
    if !null_jobs.is_empty() {
        let mut cols = vec![entry.vector_column.clone()];
        if fresh.vector_column != entry.vector_column {
            cols.push(fresh.vector_column.clone());
        }
        let sets: Vec<String> = cols
            .iter()
            .map(|c| format!("{} = NULL", quote_ident(c)))
            .collect();
        let null_pks: Vec<String> = null_jobs.iter().map(|(_, pk)| pk.clone()).collect();
        let q = format!(
            "UPDATE {tbl} SET {sets} WHERE {predicate}",
            tbl = entry.qualified_table(),
            sets = sets.join(", "),
            predicate = entry.pk_any_clause("", "$1"),
        );
        Spi::connect_mut(|c| {
            c.update(q.as_str(), None, &[null_pks.into()]).unwrap();
        });
        let ids: Vec<i64> = null_jobs.iter().map(|(id, _)| *id).collect();
        delete_jobs(&ids);
        applied.nulled += null_jobs.len() as i64;
    }

    // 2. Routing changed mid-flight (migrate()/finalize/abort raced the
    //    network call): the vectors belong to the wrong model/column.
    //    Release the claims; the next pass reprocesses with fresh routing.
    if !embed_jobs.is_empty() && fresh != *expected {
        let ids: Vec<i64> = embed_jobs.iter().map(|(id, _, _, _, _)| *id).collect();
        // Release for immediate reprocessing (no backoff): the work wasn't
        // wrong, the routing under it changed.
        unclaim_jobs(
            &ids,
            0.0,
            "embed routing changed mid-flight (migration)",
            false,
        );
        applied.retried += ids.len() as i64;
        return applied;
    }

    // 3. Embedded rows, per outcome.
    let mut ok_pks: Vec<String> = Vec::new();
    let mut ok_chunks: Vec<i64> = Vec::new();
    let mut ok_vecs: Vec<String> = Vec::new();
    let mut ok_ids: Vec<i64> = Vec::new();
    for (job, outcome) in embed_jobs.iter().zip(outcomes.iter()) {
        let (job_id, pk, chunk_id, attempts, _text) = job;
        match outcome {
            ItemOutcome::Ok(v) => {
                if v.len() as i32 != fresh.dim {
                    move_to_dead(
                        &[*job_id],
                        &format!(
                            "model returned {} dims, column is vector({})",
                            v.len(),
                            fresh.dim
                        ),
                    );
                    applied.dead += 1;
                } else {
                    ok_pks.push(pk.clone());
                    ok_chunks.push(chunk_id.unwrap_or(0));
                    ok_vecs.push(serialize_vector(v));
                    ok_ids.push(*job_id);
                }
            }
            ItemOutcome::Dead(err) => {
                move_to_dead(&[*job_id], err);
                applied.dead += 1;
            }
            ItemOutcome::Retry(err) => {
                if *attempts > max_retries {
                    move_to_dead(&[*job_id], &format!("retries exhausted: {err}"));
                    applied.dead += 1;
                } else {
                    retry_job(*job_id, *attempts, err, backoff_ms);
                    applied.retried += 1;
                }
            }
        }
    }

    if !ok_ids.is_empty() {
        if entry.is_recursive() {
            // Every child write is keyed by the never-reused chunk identity
            // AND the queued source key. Zero updated rows for a chunk is
            // an obsolete success: an update/delete replaced the chunk
            // mid-flight; the replacement row has another identity and its
            // own job. The job is deleted either way and only real writes
            // count.
            let written =
                write_chunk_vectors(&entry, &fresh.vector_column, &ok_chunks, &ok_pks, &ok_vecs);
            delete_jobs(&ok_ids);
            applied.done += written;
        } else {
            write_vectors(&entry, &fresh.vector_column, &ok_pks, &ok_vecs);
            delete_jobs(&ok_ids);
            applied.done += ok_ids.len() as i64;
        }
    }

    if applied.done + applied.nulled > 0 && crate::gucs::NOTIFY_ON_WRITE.get() {
        Spi::run_with_args(
            "SELECT pg_notify('postvec', $1)",
            &[entry.id.to_string().into()],
        )
        .unwrap_or_else(|e| pgrx::warning!("postvec: pg_notify failed: {e}"));
    }
    applied
}

/// Bulk chunk write-back, keyed by chunk identity + queued source key.
/// Returns the number of rows actually updated (obsolete chunks match zero).
fn write_chunk_vectors(
    entry: &RegistryEntry,
    vector_column: &str,
    chunk_ids: &[i64],
    pks: &[String],
    vec_texts: &[String],
) -> i64 {
    let q = format!(
        "UPDATE {qdest} t SET {vec} = d.v::vector
           FROM (SELECT unnest($1::int8[]) AS cid, unnest($2::text[]) AS pk,
                        unnest($3::text[]) AS v) d
          WHERE t.postvec_chunk_id = d.cid
            AND t.postvec_source_pk = d.pk::{pk_type}
          RETURNING 1",
        qdest = entry.qualified_vector_table(),
        vec = quote_ident(vector_column),
        pk_type = entry.pk_types[0],
    );
    Spi::connect_mut(|c| {
        c.update(
            q.as_str(),
            None,
            &[
                chunk_ids.to_vec().into(),
                pks.to_vec().into(),
                vec_texts.to_vec().into(),
            ],
        )
        .unwrap()
        .len() as i64
    })
}

/// Bulk write-back: `UPDATE ... FROM (unnest, unnest)` — one statement for the
/// whole batch, vectors cast from pgvector text format (D7).
fn write_vectors(entry: &RegistryEntry, vector_column: &str, pks: &[String], vec_texts: &[String]) {
    let q = format!(
        "UPDATE {tbl} t SET {vec} = d.v::vector
           FROM (SELECT unnest($1::text[]) AS pk, unnest($2::text[]) AS v) d
          WHERE {predicate}",
        tbl = entry.qualified_table(),
        vec = quote_ident(vector_column),
        predicate = entry.pk_staging_join_clause("t", "d", "pk"),
    );
    Spi::connect_mut(|c| {
        c.update(
            q.as_str(),
            None,
            &[pks.to_vec().into(), vec_texts.to_vec().into()],
        )
        .unwrap();
    });
}

/// Return claimed jobs to the pending pool — THE one place that owns the
/// un-claiming invariant. Un-claiming re-enters the `jobs_pending_dedup`
/// partial unique index, so it is written as DELETE + `INSERT ... ON CONFLICT
/// DO NOTHING` rather than `UPDATE ... SET claimed_at = NULL`: an UPDATE
/// re-entering the partial index races an *in-flight* trigger INSERT for the
/// same row (an MVCC dedup pre-DELETE cannot see the uncommitted duplicate),
/// blocks on it, and aborts the whole worker transaction with a unique
/// violation when the writer commits. INSERT's ON CONFLICT arbitration
/// handles exactly that case (waits, then skips), and the `DISTINCT ON` keeps
/// only the newest duplicate within the released set itself. Attempts and
/// `created_at` are preserved (the job gets a fresh id). `delay_secs` is the
/// retry backoff (0 = due now); `keep_prior_error` preserves an existing
/// `last_error` over `err`.
fn unclaim_jobs(ids: &[i64], delay_secs: f64, err: &str, keep_prior_error: bool) {
    unclaim_jobs_inner(ids, delay_secs, err, keep_prior_error, false)
}

/// Return over-budget claims to pending without consuming the attempt the
/// claim bumped: byte-budget deferral is capacity scheduling, not failure —
/// a job repeatedly deferred at batch boundaries must not creep toward
/// `max_retries` and dead-letter without ever failing.
fn defer_jobs(ids: &[i64]) {
    unclaim_jobs_inner(ids, 0.0, "deferred: batch byte budget reached", true, true)
}

fn unclaim_jobs_inner(
    ids: &[i64],
    delay_secs: f64,
    err: &str,
    keep_prior_error: bool,
    refund_attempt: bool,
) {
    if ids.is_empty() {
        return;
    }
    Spi::connect_mut(|c| {
        // Four-column dedup key everywhere. Under a chunked entry every
        // child job of one document shares pk_value; a two-column
        // DISTINCT ON here would collapse a released group's N chunk jobs
        // into one and the other N-1 chunks would never be embedded again.
        c.update(
            "WITH released AS (
                 DELETE FROM postvec.jobs WHERE id = ANY($1)
                 RETURNING id, registry_id, pk_value, op, chunk_id, attempts,
                           last_error, created_at
             ), dedup AS (
                 SELECT DISTINCT ON (registry_id, op, pk_value, chunk_id)
                        registry_id, pk_value, op, chunk_id, attempts,
                        last_error, created_at
                   FROM released
                  ORDER BY registry_id, op, pk_value, chunk_id, id DESC
             )
             INSERT INTO postvec.jobs
                 (registry_id, pk_value, op, chunk_id, attempts, not_before,
                  last_error, created_at)
             SELECT registry_id, pk_value, op, chunk_id,
                    CASE WHEN $5 THEN greatest(attempts - 1, 0) ELSE attempts END,
                    now() + make_interval(secs => $2),
                    CASE WHEN $4 THEN COALESCE(last_error, left($3, 1024)) ELSE left($3, 1024) END,
                    created_at
               FROM dedup
             ON CONFLICT (registry_id, op, pk_value, chunk_id)
             WHERE claimed_at IS NULL DO NOTHING",
            None,
            &[
                ids.to_vec().into(),
                delay_secs.into(),
                err.into(),
                keep_prior_error.into(),
                refund_attempt.into(),
            ],
        )
        .unwrap();
    });
}

pub(crate) fn delete_jobs(ids: &[i64]) {
    if ids.is_empty() {
        return;
    }
    Spi::connect_mut(|c| {
        c.update(
            "DELETE FROM postvec.jobs WHERE id = ANY($1)",
            None,
            &[ids.to_vec().into()],
        )
        .unwrap();
    });
}

/// Exponential-backoff retry: un-claim with `not_before` pushed out by
/// `backoff · 2^attempts`.
fn retry_job(job_id: i64, attempts: i32, err: &str, backoff_ms: i64) {
    let exp = attempts.clamp(0, 10);
    let secs = (backoff_ms as f64 / 1000.0) * 2f64.powi(exp);
    unclaim_jobs(&[job_id], secs, err, false);
}

/// Recovery path for a write-back transaction that aborted wholesale — a
/// raising user trigger, a CHECK constraint or policy on the target table, or
/// a `lock_timeout` (the worker sets one so a blocked table cannot freeze the
/// pipeline). Runs in a **fresh** transaction after the failed one rolled
/// back: jobs whose attempts are exhausted dead-letter with the error;
/// the rest are released with exponential backoff so a persistently failing
/// table converges to `jobs_dead` instead of crash-looping the worker.
/// Returns `(dead, retried)`.
pub fn release_failed_group(ids: &[i64], max_retries: i32, backoff_ms: i64, err: &str) -> Applied {
    let mut applied = Applied::default();
    if ids.is_empty() {
        return applied;
    }
    let (exhausted, rest, max_attempts) = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT id, attempts FROM postvec.jobs WHERE id = ANY($1)",
                None,
                &[ids.to_vec().into()],
            )
            .expect("postvec: release_failed_group scan failed");
        let mut exhausted = Vec::new();
        let mut rest = Vec::new();
        let mut max_attempts = 0i32;
        for r in t {
            let id = r.get::<i64>(1).unwrap().unwrap();
            let attempts = r.get::<i32>(2).unwrap().unwrap();
            if attempts > max_retries {
                exhausted.push(id);
            } else {
                rest.push(id);
                max_attempts = max_attempts.max(attempts);
            }
        }
        (exhausted, rest, max_attempts)
    });
    move_to_dead(
        &exhausted,
        &format!("write-back failed; retries exhausted: {err}"),
    );
    applied.dead += exhausted.len() as i64;
    let secs = (backoff_ms.max(0) as f64 / 1000.0) * 2f64.powi(max_attempts.clamp(0, 10));
    unclaim_jobs(&rest, secs, &format!("write-back failed: {err}"), false);
    applied.retried += rest.len() as i64;
    applied
}

pub(crate) fn move_to_dead(ids: &[i64], err: &str) {
    if ids.is_empty() {
        return;
    }
    Spi::connect_mut(|c| {
        // jobs_dead has its own identity PK; the queue id is preserved as
        // job_id (the audit link to the failed job).
        c.update(
            "INSERT INTO postvec.jobs_dead
                 (job_id, registry_id, pk_value, op, chunk_id, attempts,
                  not_before, claimed_at, last_error, created_at)
             SELECT id, registry_id, pk_value, op, chunk_id, attempts,
                    not_before, claimed_at, left($2, 1024), created_at
               FROM postvec.jobs WHERE id = ANY($1)",
            None,
            &[ids.to_vec().into(), err.into()],
        )
        .unwrap();
        c.update(
            "DELETE FROM postvec.jobs WHERE id = ANY($1)",
            None,
            &[ids.to_vec().into()],
        )
        .unwrap();
    });
}

#[cfg(test)]
mod chunking_tests {
    use super::*;

    #[test]
    fn request_chunks_bound_items_and_bytes() {
        // Response bound: tiny texts, huge dim → item-count cut.
        let texts: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
        let big_dim_items = max_items_for_dim(2_000_000); // forces a tiny item cap
        let chunks = request_chunks(&texts, 2_000_000);
        assert!(chunks.iter().all(|c| c.len() <= big_dim_items.max(1)));
        assert_eq!(chunks.iter().map(|c| c.len()).sum::<usize>(), 10);

        // Request bound: two 30 MiB texts never share a 48 MiB request.
        let texts = vec!["x".repeat(30 << 20), "y".repeat(30 << 20)];
        let chunks = request_chunks(&texts, 3);
        assert_eq!(chunks.len(), 2, "byte budget splits the pair");

        // A single over-budget text still forms its own chunk (progress).
        let texts = vec!["z".repeat(60 << 20)];
        assert_eq!(request_chunks(&texts, 3).len(), 1);

        assert!(request_chunks(&[], 3).is_empty());
    }
}

#[cfg(any(test, feature = "pg_test"))]
pub mod mock {
    //! An in-process [`InferenceClient`] for tests: deterministic vectors,
    //! optional poison rows, and forced errors — no network.
    use crate::client::{EmbedRoute, InferenceClient, ModelInfo, PvError, RavennaCode};
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct MockClient {
        pub dim: usize,
        /// Dimension returned for *bridged* embed calls (route.target_model
        /// set) — a real embed-bridge executor answers in the target space.
        /// Unset => bridged calls use `dim` like direct ones.
        pub bridge_dim: Option<usize>,
        /// The (model, route) of the most recent embed call, for assertions.
        pub last_embed: Mutex<Option<(String, EmbedRoute)>>,
        /// Texts that trigger a `ContextLengthExceeded` (poison) error.
        pub poison: Vec<String>,
        /// Texts for which the model "returns" a NaN-laden vector.
        pub nan: Vec<String>,
        /// Texts for which embed() deliberately returns the wrong dimension.
        pub wrong_dim: Vec<String>,
        /// If set, every embed call fails with this error.
        pub fail_all: Option<PvError>,
        /// If set, embed() deliberately returns this many vectors, exercising
        /// decode/count-mismatch handling.
        pub embed_len: Option<usize>,
        /// If set, convert() maps each input to this dimension (deterministic
        /// output derived from the input sum) — mimics a real converter.
        pub convert_dim: Option<usize>,
        /// If set, every convert call fails with this error.
        pub fail_convert: Option<PvError>,
        /// If set, convert() returns a NaN vector for the input at this
        /// (call-local) index — exercises the driver's per-row skip path.
        pub convert_nan_index: Option<usize>,
        /// If set, convert() times out (Deadline) whenever it is called with
        /// more inputs than this — simulates an oversized batch, exercising
        /// the migration driver's deadline batch-splitting.
        pub convert_max_batch: Option<usize>,
    }

    fn clone_err(e: &PvError) -> PvError {
        match e {
            PvError::Deadline { ms } => PvError::Deadline { ms: *ms },
            PvError::Remote { code, message } => PvError::Remote {
                code: *code,
                message: message.clone(),
            },
            other => PvError::Internal(format!("{other}")),
        }
    }

    impl MockClient {
        pub fn new(dim: usize) -> Self {
            MockClient {
                dim,
                ..Default::default()
            }
        }
        pub fn with_poison(mut self, text: &str) -> Self {
            self.poison.push(text.to_string());
            self
        }
        pub fn with_nan(mut self, text: &str) -> Self {
            self.nan.push(text.to_string());
            self
        }
        pub fn with_wrong_dim(mut self, text: &str) -> Self {
            self.wrong_dim.push(text.to_string());
            self
        }
        pub fn with_convert_dim(mut self, dim: usize) -> Self {
            self.convert_dim = Some(dim);
            self
        }
        pub fn with_bridge_dim(mut self, dim: usize) -> Self {
            self.bridge_dim = Some(dim);
            self
        }
        /// The (model, route) of the most recent embed call.
        pub fn last_embed_call(&self) -> Option<(String, EmbedRoute)> {
            self.last_embed.lock().unwrap().clone()
        }
        fn canned(&self, text: &str, base_dim: usize) -> Vec<f32> {
            if self.nan.contains(&text.to_string()) {
                return (0..base_dim).map(|_| f32::NAN).collect();
            }
            let dim = if self.wrong_dim.contains(&text.to_string()) {
                base_dim + 1
            } else {
                base_dim
            };
            // Deterministic, text-derived so tests can assert stability.
            let base = text.len() as f32;
            (0..dim).map(|i| base + i as f32 * 0.001).collect()
        }
    }

    impl InferenceClient for MockClient {
        async fn embed(
            &self,
            texts: &[String],
            model: &str,
            route: &EmbedRoute,
        ) -> Result<Vec<Vec<f32>>, PvError> {
            *self.last_embed.lock().unwrap() = Some((model.to_string(), route.clone()));
            if let Some(e) = &self.fail_all {
                return Err(clone_err(e));
            }
            if let Some(bad) = texts.iter().find(|t| self.poison.contains(t)) {
                return Err(PvError::Remote {
                    code: RavennaCode::ContextLengthExceeded,
                    message: format!("poison: {bad}"),
                });
            }
            let base_dim = match (&route.target_model, self.bridge_dim) {
                (Some(_), Some(d)) => d,
                _ => self.dim,
            };
            let mut out: Vec<Vec<f32>> = texts.iter().map(|t| self.canned(t, base_dim)).collect();
            if let Some(n) = self.embed_len {
                out.truncate(n);
            }
            Ok(out)
        }

        async fn convert(&self, vecs: &[Vec<f32>], _model: &str) -> Result<Vec<Vec<f32>>, PvError> {
            if let Some(e) = &self.fail_convert {
                return Err(clone_err(e));
            }
            if let Some(max) = self.convert_max_batch {
                if vecs.len() > max {
                    return Err(PvError::Deadline { ms: 1 });
                }
            }
            Ok(vecs
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let dim = self.convert_dim.unwrap_or(v.len());
                    if self.convert_nan_index == Some(i) {
                        return vec![f32::NAN; dim];
                    }
                    let base: f32 = v.iter().sum();
                    (0..dim).map(|j| base + j as f32 * 0.01).collect()
                })
                .collect())
        }

        async fn list_models(&self) -> Result<Vec<ModelInfo>, PvError> {
            Ok(Vec::new())
        }
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use crate::registry::{RegistryEntry, RegistryEntryDb};

    /// Seed the cache with embed model 'm' at the given dimension.
    fn seed_model(dim: i32) {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',$1,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[dim.into()],
        )
        .unwrap();
    }

    fn enable_docs(dim: i32) {
        seed_model(dim);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
    }

    /// Byte ceilings: a rendered document over the per-item cap
    /// dead-letters at claim time with its measured size — never truncated,
    /// never materialized in Rust, never sent — while rows within the cap
    /// drain normally.
    #[pg_test]
    fn oversized_document_dead_letters_at_claim() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES (repeat('x', 100)), ('small')").unwrap();
        let groups = claim_and_read_budgeted(64, 300.0, 50, 1_000_000);
        let group = groups.into_iter().next().unwrap();
        assert_eq!(group.items.len(), 1, "only the in-cap row is readable");
        assert_eq!(group.items[0].text.as_deref(), Some("small"));
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .expect("the oversized row dead-lettered");
        assert!(err.contains("100 bytes"), "measured size reported: {err}");
        assert!(err.contains("ceiling is 50"), "{err}");
    }

    /// The batch byte budget defers rows past the boundary back to pending
    /// with the claim attempt refunded — capacity scheduling, not failure —
    /// and the deferred row drains on the next cycle.
    #[pg_test]
    fn batch_byte_budget_defers_with_attempt_refund() {
        enable_docs(3);
        Spi::run(
            "INSERT INTO docs (body)
             VALUES (repeat('a', 40)), (repeat('b', 40)), (repeat('c', 40))",
        )
        .unwrap();
        let group = claim_and_read_budgeted(64, 300.0, 1_000, 100)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            group.items.len(),
            2,
            "two 40-byte rows fit a 100-byte budget"
        );

        // The third row is pending again, due now, with its attempt refunded.
        let (pending, attempts) = (
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE claimed_at IS NULL AND not_before <= now()",
            )
            .unwrap()
            .unwrap(),
            Spi::get_one::<i32>("SELECT attempts FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap()
                .unwrap(),
        );
        assert_eq!(pending, 1, "the over-budget row went back to pending");
        assert_eq!(attempts, 0, "deferral refunded the claim's attempt bump");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0),
            "deferral never dead-letters"
        );

        // Next cycle picks the deferred row up. WHICH row deferred is
        // order-dependent (queue ids follow the transition table's row
        // order), so assert coverage, not identity: across the two cycles
        // every document was read exactly once.
        let group2 = claim_and_read_budgeted(64, 300.0, 1_000, 100)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(group2.items.len(), 1);
        let mut seen: Vec<String> = group
            .items
            .iter()
            .chain(group2.items.iter())
            .map(|i| i.text.clone().unwrap())
            .collect();
        seen.sort();
        assert_eq!(seen, vec!["a".repeat(40), "b".repeat(40), "c".repeat(40)]);
    }

    /// A column enabled on a convert-only model (no embed model of its own —
    /// e.g. a commercial target space reachable only through a converter)
    /// drains through the embed-bridge route: one EmbedTexts call against the
    /// bridge executor, answered in the target dimension. This is the exact
    /// resolve→embed→apply sequence the worker runs per group.
    #[pg_test]
    fn bridged_entry_drains_via_embed_bridge() {
        // Embed model 'm' (dim 3), a converter m→ext (target dim 4), and an
        // embed-bridge executor: 'ext' is embeddable only via the bridge.
        seed_model(3);
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('conv-m-ext', 'convert', 'm', 'ext', 4, '{}'::jsonb),
                    ('embed-bridge', 'embed-bridge', NULL, NULL, NULL, '{}'::jsonb)",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','ext', backfill => false)")
            .unwrap();
        // The shadow column is sized from the converter's target_dim.
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic'"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)")
        );

        Spi::run("INSERT INTO docs (body) VALUES ('hello'), ('world')").unwrap();
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (null_jobs, embed_jobs) = split_items(group.items);
        assert_eq!((null_jobs.len(), embed_jobs.len()), (0, 2));

        // The worker's per-group resolution: 'ext' rides the bridge.
        let (model, route) = crate::api::embed::resolve_embed_route(&group.routing.model)
            .expect("bridged model resolves")
            .into_call();
        assert_eq!(model, "embed-bridge");
        assert_eq!(
            route,
            EmbedRoute {
                bridge_model: Some("m".into()),
                target_model: Some("ext".into()),
                // The worker embeds stored content.
                purpose: crate::client::EmbedPurpose::Document,
            }
        );

        let client = super::mock::MockClient::new(3).with_bridge_dim(4);
        let texts: Vec<String> = embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(&client, &model, &route, 1000, &texts);
        let applied = apply_group(
            group.entry.id,
            &group.routing,
            &null_jobs,
            &embed_jobs,
            &outcomes,
            5,
            5000,
        );
        assert_eq!(applied.done, 2, "both rows written via the bridge");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs
                  WHERE body_semantic IS NOT NULL AND vector_dims(body_semantic) = 4"
            )
            .unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
        // The wire call went to the executor with the bridge route.
        let (called_model, called_route) = client.last_embed_call().unwrap();
        assert_eq!(called_model, "embed-bridge");
        assert_eq!(called_route.bridge_model.as_deref(), Some("m"));
        assert_eq!(called_route.target_model.as_deref(), Some("ext"));
    }

    #[pg_test]
    fn claim_marks_and_does_not_double_claim() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        // Two pending jobs from the triggers.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap(),
            Some(2)
        );
        let g1 = claim_and_read(64, 300.0);
        assert_eq!(g1.iter().map(|g| g.items.len()).sum::<usize>(), 2);
        // Everything is claimed; a second claim inside the visibility window
        // returns nothing (no double processing).
        let g2 = claim_and_read(64, 300.0);
        assert!(
            g2.is_empty(),
            "claimed jobs must not be re-claimed within the vis window"
        );
    }

    /// A claim whose visibility timeout expired (worker crash between claim
    /// and write-back) is re-delivered: un-claimed, immediately due, attempts
    /// bumped again on the next claim.
    #[pg_test]
    fn stale_claim_is_reclaimed_after_visibility_timeout() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        assert_eq!(claim_and_read(64, 300.0).len(), 1);

        // Age the claim past the visibility window.
        Spi::run("UPDATE postvec.jobs SET claimed_at = now() - interval '10 minutes'").unwrap();
        let groups = claim_and_read(64, 300.0);
        assert_eq!(groups.len(), 1, "the stale claim is re-delivered");
        assert_eq!(
            groups[0].items[0].attempts, 2,
            "attempts count both deliveries"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT last_error FROM postvec.jobs")
                .unwrap()
                .as_deref(),
            Some("visibility timeout expired; reclaimed"),
            "the reclaim is visible in last_error"
        );
    }

    /// A job that only ever dies mid-batch (worker crash loop) must not be
    /// re-delivered forever: once its attempts are exhausted, the reclaim
    /// dead-letters it instead of requeueing.
    #[pg_test]
    fn crash_looping_job_dead_letters_instead_of_reclaiming_forever() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        assert_eq!(claim_and_read(64, 300.0).len(), 1);
        // Simulate many crashed deliveries: attempts exhausted, claim stale.
        Spi::run(
            "UPDATE postvec.jobs
                SET attempts = 6, claimed_at = now() - interval '10 minutes'",
        )
        .unwrap();

        let groups = claim_and_read(64, 300.0); // MAX_RETRIES defaults to 5
        assert!(groups.is_empty(), "the poison claim is not re-delivered");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .unwrap_or_default();
        assert!(
            err.contains("visibility timeout expired with attempts exhausted"),
            "dead-letter reason names the crash loop: {err}"
        );
    }

    /// Two stale claims for the same row (claim → new job → claim → both go
    /// stale) must not collide with the pending-dedup unique index when they
    /// are released: the older duplicate is dropped, the newer re-delivers.
    #[pg_test]
    fn duplicate_stale_claims_reclaim_without_dedup_collision() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        assert_eq!(claim_and_read(64, 300.0).len(), 1);
        // A fresh job for the same row arrives while the first is claimed…
        Spi::run("UPDATE docs SET body = 'x2'").unwrap();
        // …and gets claimed too; then both claims go stale.
        assert_eq!(claim_and_read(64, 300.0).len(), 1);
        Spi::run("UPDATE postvec.jobs SET claimed_at = now() - interval '10 minutes'").unwrap();

        let groups = claim_and_read(64, 300.0);
        assert_eq!(
            groups.iter().map(|g| g.items.len()).sum::<usize>(),
            1,
            "exactly one survivor is re-delivered"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1),
            "the older duplicate was dropped, not collided"
        );
    }

    /// A timestamptz PK rendered under one TimeZone must still round-trip:
    /// the queue keys by the writer's text rendering, and the source read
    /// joins in the native type — a different worker-session TimeZone must
    /// not make the row look deleted (which would NULL its vector).
    /// The worker reads document text through the entry's template at
    /// claim time. NULL context renders empty; a NULL source stays a NULL
    /// job (no inference).
    #[pg_test]
    fn format_renders_for_worker() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false,
                                   format => '$title — $body')",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs (title, body) VALUES
                 ('T', 'B'), (NULL, 'B2'), ('T3', NULL)",
        )
        .unwrap();
        // The trigger enqueues rows 1 and 2 on INSERT (source non-NULL);
        // row 3 (NULL source) enqueues nothing — force a job for it so the
        // NULL-render path is exercised too.
        Spi::run(
            "INSERT INTO postvec.jobs (registry_id, pk_value) SELECT id, '3' FROM postvec.registry",
        )
        .unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let texts: BTreeMap<String, Option<String>> = group
            .items
            .into_iter()
            .map(|it| (it.pk_value, it.text))
            .collect();
        assert_eq!(texts["1"].as_deref(), Some("T — B"));
        assert_eq!(
            texts["2"].as_deref(),
            Some(" — B2"),
            "NULL context renders as an empty string"
        );
        assert_eq!(texts["3"], None, "a NULL source stays a NULL job");
    }

    /// A job claimed under the old template still converges. The
    /// set_format() full refresh inserts a new pending job for the same PK
    /// (the claimed one is outside the dedup predicate), which the single
    /// worker applies after the old write-back. No template-version column
    /// needed.
    #[pg_test]
    fn set_format_racing_claimed_old_template_converges() {
        seed_model(3);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false, format => 'A $body')",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();

        // Worker claims the job and reads the OLD template's rendering.
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(ej[0].4, "A x", "claimed under the old template");

        // set_format lands while the "network call" is in flight: it inserts
        // a fresh pending job for the same PK next to the claimed one.
        Spi::run("SELECT postvec.set_format('docs','body', 'BB $body')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap(),
            Some(1),
            "the refresh created a pending job despite the claimed twin"
        );

        // Old write-back applies first (routing unchanged, so it commits)...
        let client = super::mock::MockClient::new(3);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        // ..."A x" has 3 characters; the mock derives vectors from text
        // length, so the old-template vector leads with 3.
        assert_eq!(
            Spi::get_one::<String>("SELECT body_semantic::text FROM docs WHERE id = 1")
                .unwrap()
                .as_deref(),
            Some("[3,3.001,3.002]"),
            "the old-template vector (text length 3) landed first"
        );

        // ...then the refresh job re-embeds with the NEW template and wins.
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(ej[0].4, "BB x", "the refresh job reads the new template");
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(
            Spi::get_one::<String>("SELECT body_semantic::text FROM docs WHERE id = 1")
                .unwrap()
                .as_deref(),
            Some("[4,4.001,4.002]"),
            "the new-template vector (text length 4) converged last"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn timestamptz_pk_survives_session_timezone_change() {
        seed_model(3);
        Spi::run("CREATE TABLE ts (at timestamptz PRIMARY KEY, body text)").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('ts','body','m', backfill => false)").unwrap();

        // Writer session at UTC+2: the trigger enqueues '… 14:00:00+02'.
        Spi::run("SET TIME ZONE '+02'").unwrap();
        Spi::run("INSERT INTO ts VALUES ('2026-07-04 14:00:00+02', 'hello')").unwrap();
        // Worker session back at UTC: the same instant renders '… 12:00:00+00'.
        Spi::run("SET TIME ZONE 'UTC'").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(
            (nj.len(), ej.len()),
            (0, 1),
            "the row is found through the native-typed join, not treated as deleted"
        );
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 1);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM ts WHERE body_semantic IS NOT NULL").unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn success_path_writes_vectors_and_clears_jobs() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('hello'), ('world')").unwrap();

        let groups = claim_and_read(64, 300.0);
        assert_eq!(groups.len(), 1);
        let entry = groups.into_iter().next().unwrap();
        let (null_jobs, embed_jobs) = split_items(entry.items);
        assert_eq!(null_jobs.len(), 0);
        assert_eq!(embed_jobs.len(), 2);

        let client = super::mock::MockClient::new(3);
        let texts: Vec<String> = embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        let e = RegistryEntry::load(entry.entry.id).unwrap();
        let applied = apply_group(
            e.id,
            &embed_routing(&e),
            &null_jobs,
            &embed_jobs,
            &outcomes,
            5,
            5000,
        );
        assert_eq!(applied.done, 2);

        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn poison_row_is_dead_lettered_rest_succeed() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('ok1'), ('BAD'), ('ok2')").unwrap();

        let entry = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(entry.entry.id).unwrap();
        let (null_jobs, embed_jobs) = split_items(entry.items);
        let texts: Vec<String> = embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();

        let client = super::mock::MockClient::new(3).with_poison("BAD");
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        let applied = apply_group(
            e.id,
            &embed_routing(&e),
            &null_jobs,
            &embed_jobs,
            &outcomes,
            5,
            5000,
        );

        assert_eq!(applied.done, 2, "the two good rows embed");
        assert_eq!(applied.dead, 1, "the poison row dead-letters");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(1)
        );
        // jobs_dead has its own PK; the queue id is preserved as job_id.
        let (dead_id, job_id) = (
            Spi::get_one::<i64>("SELECT dead_id FROM postvec.jobs_dead").unwrap(),
            Spi::get_one::<i64>("SELECT job_id FROM postvec.jobs_dead").unwrap(),
        );
        assert!(dead_id.is_some(), "surrogate key assigned");
        assert!(job_id.is_some(), "original queue id preserved");
    }

    /// DROP TABLE while jobs are pending: the claim path must quarantine the
    /// entry (purge jobs, drop leftover functions, state -> disabled) instead
    /// of aborting on the source read — which would crash-loop the worker.
    #[pg_test]
    fn dropped_table_quarantines_entry() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        let entry = RegistryEntry::load_active("public", "docs", "body").unwrap();
        Spi::run("DROP TABLE docs").unwrap();

        let groups = claim_and_read(64, 300.0);
        assert!(groups.is_empty(), "no work groups for a vanished table");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "the entry's jobs are purged"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[entry.id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("disabled"),
            "the entry is quarantined"
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_proc WHERE proname IN ('trg_ins_{0}','trg_upd_{0}')",
                entry.id
            ))
            .unwrap(),
            Some(0),
            "leftover trigger functions dropped"
        );
    }

    /// Dropping just the shadow column quarantines too (the write-back
    /// target is gone).
    #[pg_test]
    fn dropped_vector_column_quarantines_entry() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        Spi::run("ALTER TABLE docs DROP COLUMN body_semantic").unwrap();

        let groups = claim_and_read(64, 300.0);
        assert!(groups.is_empty());
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
            )
            .unwrap(),
            Some(0),
            "the enqueue triggers were removed from the still-existing table"
        );
    }

    /// disable() landing between claim and write-back: the stale vectors must
    /// be discarded, not written (the column may even be gone).
    #[pg_test]
    fn disable_mid_flight_discards_stale_write_back() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );

        // disable() races in while the (mock) network call was in flight.
        Spi::run("SELECT postvec.disable('docs','body', drop_column => true)").unwrap();

        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 0, "nothing written after disable");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    #[pg_test]
    fn retry_backs_off_then_dead_letters_when_exhausted() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        let entry = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(entry.entry.id).unwrap();
        let (null_jobs, embed_jobs) = split_items(entry.items);

        // attempts is 1 after the first claim; with max_retries=0 a retryable
        // failure exhausts immediately and dead-letters.
        let outcomes = vec![ItemOutcome::Retry("transient".into())];
        let applied = apply_group(
            e.id,
            &embed_routing(&e),
            &null_jobs,
            &embed_jobs,
            &outcomes,
            0,
            5000,
        );
        assert_eq!(applied.dead, 1);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(1)
        );

        // With headroom (max_retries=5) the job is rescheduled, not killed.
        Spi::run("DELETE FROM postvec.jobs_dead").unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('y')").unwrap();
        let entry2 = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e2 = RegistryEntry::load(entry2.entry.id).unwrap();
        let (nj, ej) = split_items(entry2.items);
        let applied2 = apply_group(
            e2.id,
            &embed_routing(&e2),
            &nj,
            &ej,
            &[ItemOutcome::Retry("t".into())],
            5,
            5000,
        );
        assert_eq!(applied2.retried, 1);
        // Rescheduled with a future not_before and released claim.
        let due = Spi::get_one::<i64>(
            "SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL AND not_before > now()",
        )
        .unwrap();
        assert_eq!(due, Some(1));
    }

    #[pg_test]
    fn nan_embedding_is_dead_lettered_not_written() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('good'), ('NAN')").unwrap();
        let entry = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(entry.entry.id).unwrap();
        let (null_jobs, embed_jobs) = split_items(entry.items);
        let texts: Vec<String> = embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();

        // The model returns a NaN vector for 'NAN'; it must dead-letter cleanly
        // instead of failing the whole write-back batch.
        let client = super::mock::MockClient::new(3).with_nan("NAN");
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        let applied = apply_group(
            e.id,
            &embed_routing(&e),
            &null_jobs,
            &embed_jobs,
            &outcomes,
            5,
            5000,
        );

        assert_eq!(applied.done, 1, "the finite row still writes");
        assert_eq!(applied.dead, 1, "the NaN row dead-letters");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(1)
        );
    }

    #[pg_test]
    fn wrong_dim_embedding_is_dead_lettered_not_written() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('good'), ('WRONG_DIM')").unwrap();
        let entry = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(entry.entry.id).unwrap();
        let (null_jobs, embed_jobs) = split_items(entry.items);
        let texts: Vec<String> = embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();

        let client = super::mock::MockClient::new(3).with_wrong_dim("WRONG_DIM");
        let outcomes = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        let applied = apply_group(
            e.id,
            &embed_routing(&e),
            &null_jobs,
            &embed_jobs,
            &outcomes,
            5,
            5000,
        );

        assert_eq!(applied.done, 1, "the correctly-shaped row still writes");
        assert_eq!(applied.dead, 1, "the wrong-dim row dead-letters");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
                .unwrap()
                .as_deref(),
            Some("model returned 4 dims, column is vector(3)")
        );
    }

    /// PUBLIC can INSERT into postvec.jobs directly, so a malformed pk_value
    /// (one that cannot cast back to the PK's native type) can land in the
    /// queue. It must dead-letter at claim time — not abort the claim/read
    /// transaction, which would roll back the attempts bump and re-deliver
    /// the same poison job forever, starving healthy work.
    #[pg_test]
    fn malformed_direct_queue_insert_dead_letters_and_healthy_jobs_proceed() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("CREATE TABLE u (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text)")
            .unwrap();
        Spi::run("INSERT INTO u (body) VALUES ('alpha')").unwrap();
        let rid = Spi::get_one::<i64>("SELECT postvec.enable('u','body','m')")
            .unwrap()
            .unwrap();
        // The malformed direct insert (any role with the PUBLIC grant can).
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ($1, 'not-a-uuid')",
            &[rid.into()],
        )
        .unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(
            (nj.len(), ej.len()),
            (0, 1),
            "only the healthy job is delivered"
        );
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 1, "the healthy row still embeds");

        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "nothing left pinned in the queue"
        );
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .unwrap_or_default();
        assert!(
            err.contains("not valid input for PK type uuid"),
            "the malformed key dead-letters with a precise reason: {err}"
        );
    }

    /// End-to-end for a non-integer PK: enable on a uuid PK, enqueue via the
    /// trigger, run the queue with the mock, and confirm the right row filled.
    #[pg_test]
    fn uuid_pk_round_trips_through_the_queue() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("CREATE TABLE u (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), body text)")
            .unwrap();
        Spi::run("INSERT INTO u (body) VALUES ('alpha'), ('beta')").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('u','body','m')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(group.entry.id).unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(e.id, &embed_routing(&e), &nj, &ej, &outcomes, 5, 5000);

        assert_eq!(applied.done, 2, "both uuid-keyed rows write back");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM u WHERE body_semantic IS NOT NULL").unwrap(),
            Some(2),
            "vectors matched back to the correct uuid rows"
        );
    }

    /// Same, for a text PK (case-sensitive, embedded apostrophes) — proves the
    /// text-keying survives quoting.
    #[pg_test]
    fn text_pk_round_trips_through_the_queue() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("CREATE TABLE t (slug text PRIMARY KEY, body text)").unwrap();
        Spi::run("INSERT INTO t (slug, body) VALUES ('O''Brien', 'a'), ('mixedCase', 'b')")
            .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('t','body','m')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(group.entry.id).unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(e.id, &embed_routing(&e), &nj, &ej, &outcomes, 5, 5000);

        assert_eq!(applied.done, 2, "both text-keyed rows write back");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM t WHERE body_semantic IS NOT NULL AND slug = 'O''Brien'"
            )
            .unwrap(),
            Some(1),
            "the apostrophe-containing key matched correctly"
        );
    }

    /// Composite PK: the full trigger → queue → write-back loop matches rows
    /// through their record-text keys.
    #[pg_test]
    fn composite_pk_round_trips_through_the_queue() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("CREATE TABLE cd (a int, b text, body text, PRIMARY KEY (a, b))").unwrap();
        Spi::run("INSERT INTO cd VALUES (1,'x','alpha'), (1,'y','beta'), (2,'x','gamma')").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('cd','body','m')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(group.entry.id).unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(e.id, &embed_routing(&e), &nj, &ej, &outcomes, 5, 5000);

        assert_eq!(applied.done, 3, "all composite-keyed rows write back");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM cd WHERE body_semantic IS NOT NULL
                   AND a = 1 AND b = 'y'"
            )
            .unwrap(),
            Some(1),
            "vectors matched back to the correct composite-keyed row"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    /// Partitioned table: write-back through the parent updates rows in the
    /// right partitions.
    #[pg_test]
    fn partitioned_table_round_trips_through_the_queue() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE pd (id bigint NOT NULL, body text, PRIMARY KEY (id))
             PARTITION BY RANGE (id)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pd_lo PARTITION OF pd FOR VALUES FROM (0) TO (100)").unwrap();
        Spi::run("CREATE TABLE pd_hi PARTITION OF pd FOR VALUES FROM (100) TO (200)").unwrap();
        Spi::run("INSERT INTO pd VALUES (1, 'low'), (150, 'high')").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('pd','body','m', trigger_mode => 'row')")
            .unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let e = RegistryEntry::load(group.entry.id).unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(e.id, &embed_routing(&e), &nj, &ej, &outcomes, 5, 5000);

        assert_eq!(applied.done, 2);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM pd_hi WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(1),
            "the row in the high partition was updated through the parent"
        );
    }

    /// Simulate a live migration for the docs entry: adds the new column,
    /// inserts the migrations row, flips the registry state. Returns the
    /// migration id.
    fn start_fake_migration(registry_id: i64, new_dim: i32) -> i64 {
        Spi::run(&format!(
            "ALTER TABLE docs ADD COLUMN body_semantic_new vector({new_dim})"
        ))
        .unwrap();
        let mid = Spi::get_one_with_args::<i64>(
            "INSERT INTO postvec.migrations
                 (registry_id, old_model, new_model, old_dim, new_dim, strategy,
                  new_column, rows_total)
             VALUES ($1, 'm', 'm2', 3, $2, 'convert', 'body_semantic_new', 0)
             RETURNING id",
            &[registry_id.into(), new_dim.into()],
        )
        .unwrap()
        .unwrap();
        Spi::run_with_args(
            "UPDATE postvec.registry SET state = 'migrating' WHERE id = $1",
            &[registry_id.into()],
        )
        .unwrap();
        mid
    }

    /// While a migration is live, embed jobs must route to the new model and
    /// the new column.
    #[pg_test]
    fn routing_targets_new_column_during_migration() {
        enable_docs(3);
        let entry = RegistryEntry::load_active("public", "docs", "body").unwrap();
        assert_eq!(embed_routing(&entry).vector_column, "body_semantic");

        let mid = start_fake_migration(entry.id, 4);
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m2','embed','m2',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();

        let entry = RegistryEntry::load(entry.id).unwrap();
        let routing = embed_routing(&entry);
        assert_eq!(routing.migration_id, Some(mid));
        assert_eq!(routing.model, "m2");
        assert_eq!(routing.vector_column, "body_semantic_new");
        assert_eq!(routing.dim, 4);

        // Drive one job through the queue with the migration routing: the
        // vector lands in the NEW column.
        Spi::run("INSERT INTO docs (body) VALUES ('fresh write')").unwrap();
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        assert_eq!(group.routing.vector_column, "body_semantic_new");
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(4),
            "m2",
            &Default::default(),
            1000,
            &texts,
        );
        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 1);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs
                  WHERE body_semantic_new IS NOT NULL AND body_semantic IS NULL"
            )
            .unwrap(),
            Some(1),
            "the fresh write filled only the new column"
        );
    }

    #[pg_test]
    fn failed_migration_routes_fresh_writes_to_old_column() {
        enable_docs(3);
        let entry = RegistryEntry::load_active("public", "docs", "body").unwrap();
        let mid = start_fake_migration(entry.id, 4);
        Spi::run_with_args(
            "UPDATE postvec.migrations SET state = 'failed', error = 'boom' WHERE id = $1",
            &[mid.into()],
        )
        .unwrap();

        let entry = RegistryEntry::load(entry.id).unwrap();
        let routing = embed_routing(&entry);
        assert_eq!(routing.migration_id, None);
        assert_eq!(routing.model, "m");
        assert_eq!(routing.vector_column, "body_semantic");
        assert_eq!(routing.dim, 3);
    }

    /// A migration starting between claim and write-back means the embedded
    /// vectors were made for the wrong model — the jobs must be released, not
    /// written.
    #[pg_test]
    fn routing_change_mid_flight_releases_jobs() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let expected = group.routing.clone();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );

        // Migration races in while the (mock) network call was in flight.
        start_fake_migration(group.entry.id, 4);

        let applied = apply_group(group.entry.id, &expected, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 0, "stale-routing vectors must not be written");
        assert_eq!(applied.retried, 1, "the job is released for reprocessing");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE claimed_at IS NULL AND not_before <= now()"
            )
            .unwrap(),
            Some(1),
            "released immediately (no backoff)"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(0)
        );
    }

    /// Dropping a live migration's `_new` column between claim and write-back
    /// must fail the migration and release the jobs — NOT quarantine the whole
    /// entry (that tore down triggers and purged jobs for a recoverable
    /// situation).
    #[pg_test]
    fn dropped_migration_column_mid_flight_fails_migration_not_entry() {
        enable_docs(3);
        let entry = RegistryEntry::load_active("public", "docs", "body").unwrap();
        let mid = start_fake_migration(entry.id, 4);
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m2','embed','m2',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(4),
            "m2",
            &Default::default(),
            1000,
            &texts,
        );

        // Operator drops the migration column while the batch is in flight.
        Spi::run("ALTER TABLE docs DROP COLUMN body_semantic_new").unwrap();

        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 0);
        assert_eq!(applied.retried, 1, "the job is released, not lost");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("failed"),
            "the migration fails"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("migrating"),
            "the entry is NOT quarantined (abort() can clean up)"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap(),
            Some(1),
            "the job is pending again (re-routes to the old column next pass)"
        );
    }

    /// The recovery path for a write-back transaction that aborted wholesale
    /// (raising user trigger / constraint / lock timeout): jobs with attempts
    /// left are released with backoff; exhausted ones dead-letter.
    #[pg_test]
    fn release_failed_group_backs_off_then_dead_letters() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let ids: Vec<i64> = group.items.iter().map(|i| i.job_id).collect();

        // Attempts are 1 after the claim: with headroom both jobs release.
        let applied = release_failed_group(&ids, 5, 5000, "CHECK constraint boom");
        assert_eq!((applied.retried, applied.dead), (2, 0));
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE claimed_at IS NULL AND not_before > now()
                    AND last_error LIKE 'write-back failed:%'"
            )
            .unwrap(),
            Some(2),
            "released pending with backoff and the error recorded"
        );

        // Exhaust attempts: the recovery dead-letters instead.
        Spi::run("UPDATE postvec.jobs SET attempts = 6, claimed_at = now()").unwrap();
        let ids: Vec<i64> = Spi::connect(|c| {
            c.select("SELECT id FROM postvec.jobs", None, &[])
                .unwrap()
                .map(|r| r.get::<i64>(1).unwrap().unwrap())
                .collect()
        });
        let applied = release_failed_group(&ids, 5, 5000, "still failing");
        assert_eq!((applied.retried, applied.dead), (0, 2));
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(2)
        );
    }

    /// NULL-source jobs during a live migration null BOTH columns.
    #[pg_test]
    fn null_job_nulls_both_columns_during_migration() {
        enable_docs(3);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        let entry = RegistryEntry::load_active("public", "docs", "body").unwrap();
        start_fake_migration(entry.id, 3);
        Spi::run(
            "UPDATE docs SET body_semantic = '[1,2,3]'::vector,
                             body_semantic_new = '[4,5,6]'::vector",
        )
        .unwrap();
        Spi::run("UPDATE docs SET body = NULL").unwrap();

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(nj.len(), 1);
        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &[], 5, 5000);
        assert_eq!(applied.nulled, 1);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs
                  WHERE body_semantic IS NULL AND body_semantic_new IS NULL"
            )
            .unwrap(),
            Some(1),
            "both shadow columns nulled"
        );
    }

    // ---- Recursive child embedding (claim side) ----

    fn enable_chunked_docs(format: Option<&str>) -> i64 {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text)",
        )
        .unwrap();
        Spi::get_one_with_args::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks',
                                   chunk_size => 64, chunk_overlap => 8,
                                   format => $1)",
            &[format.into()],
        )
        .unwrap()
        .unwrap()
    }

    fn drain_refreshes() {
        while crate::worker::chunk::process_one_refresh(5).processed {}
    }

    /// Run one full claim → embed → apply pass with the mock client.
    fn run_embed_pass() -> Applied {
        let mut total = Applied::default();
        let client = super::mock::MockClient::new(3);
        loop {
            let groups = claim_and_read(64, 300.0);
            if groups.is_empty() {
                return total;
            }
            for g in groups {
                let routing = g.routing.clone();
                let (nj, ej) = split_items(g.items);
                let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
                let outcomes =
                    embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
                let a = apply_group(g.entry.id, &routing, &nj, &ej, &outcomes, 5, 5000);
                total.done += a.done;
                total.nulled += a.nulled;
                total.retried += a.retried;
                total.dead += a.dead;
            }
        }
    }

    /// End-to-end: refresh → child jobs → embed → chunk vectors written.
    #[pg_test]
    fn chunked_documents_embed_end_to_end() {
        enable_chunked_docs(None);
        Spi::run(
            "INSERT INTO docs (title, body) VALUES
                 ('t1', repeat('alpha ', 30)), ('t2', repeat('beta ', 25))",
        )
        .unwrap();
        drain_refreshes();
        let chunks = Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks")
            .unwrap()
            .unwrap();
        assert!(chunks > 2, "both documents split into several chunks");

        let applied = run_embed_pass();
        assert_eq!(applied.done, chunks, "every chunk received a vector");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs_chunks
                  WHERE body_semantic IS NOT NULL AND vector_dims(body_semantic) = 3"
            )
            .unwrap(),
            Some(chunks)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    /// With no template the chunk text is embedded exactly; with a `$chunk`
    /// template the context renders around each chunk and NULL context is
    /// empty.
    #[pg_test]
    fn chunk_template_renders_context_around_each_chunk() {
        enable_chunked_docs(Some("[$title] $chunk"));
        Spi::run("INSERT INTO docs (title, body) VALUES ('T', 'short doc'), (NULL, 'other doc')")
            .unwrap();
        drain_refreshes();

        let groups = claim_and_read(64, 300.0);
        let mut texts: Vec<String> = groups
            .into_iter()
            .flat_map(|g| g.items.into_iter().filter_map(|i| i.text))
            .collect();
        texts.sort();
        assert_eq!(
            texts,
            vec!["[T] short doc".to_string(), "[] other doc".to_string()],
            "template renders at claim time with $chunk and NULL context empty"
        );
    }

    /// [invariant 2/6]: an in-flight embed for a replaced chunk updates zero
    /// rows — obsolete success, never a write to the replacement chunk.
    #[pg_test]
    fn stale_chunk_identity_write_back_is_an_obsolete_no_op() {
        enable_chunked_docs(None);
        Spi::run("INSERT INTO docs (title, body) VALUES ('t', 'version one')").unwrap();
        drain_refreshes();

        // Claim the child job (the "network call" starts).
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let (nj, ej) = split_items(group.items);
        assert_eq!(ej.len(), 1);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );

        // The document changes while the call is in flight: the trigger
        // purges the old chunk and the refresh materializes a replacement
        // with a NEW identity.
        Spi::run("UPDATE docs SET body = 'version two'").unwrap();
        drain_refreshes();
        let new_chunk = Spi::get_one::<i64>("SELECT postvec_chunk_id FROM docs_chunks")
            .unwrap()
            .unwrap();
        assert_ne!(
            new_chunk,
            ej[0].2.unwrap(),
            "replacement has a new identity"
        );

        // The stale write-back matches zero rows and counts nothing.
        let applied = apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 0, "no vector write for the replaced chunk");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(0),
            "the replacement chunk was not corrupted by the stale vector"
        );
        // The replacement's own job is still pending and completes normally.
        assert_eq!(run_embed_pass().done, 1);
    }

    /// A claimed child whose chunk row vanished (document deleted) is
    /// consumed without inference at the next claim.
    #[pg_test]
    fn missing_chunk_job_is_deleted_without_inference() {
        let id = enable_chunked_docs(None);
        Spi::run("INSERT INTO docs (title, body) VALUES ('t', 'doomed doc')").unwrap();
        drain_refreshes();
        // Delete the chunk row directly (simulating an inline invalidation
        // that raced the pending child past the trigger's purge).
        Spi::run("ALTER TABLE docs_chunks DISABLE ROW LEVEL SECURITY").unwrap();
        Spi::run("DELETE FROM docs_chunks").unwrap();
        let groups = claim_and_read(64, 300.0);
        assert!(groups.is_empty(), "no embeddable items");
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "the obsolete child job was deleted"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0),
            "obsolete is not dead"
        );
    }

    /// A chunk-keyed job whose queued source key does not match the chunk's
    /// stored key is malformed and dead-letters (never reads another
    /// document).
    #[pg_test]
    fn mismatched_chunk_identity_dead_letters() {
        let id = enable_chunked_docs(None);
        Spi::run("INSERT INTO docs (title, body) VALUES ('t', 'doc')").unwrap();
        drain_refreshes();
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        let cid = Spi::get_one::<i64>("SELECT postvec_chunk_id FROM docs_chunks")
            .unwrap()
            .unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
             VALUES ($1, '424242', 'embed', $2)",
            &[id.into(), cid.into()],
        )
        .unwrap();
        assert!(claim_and_read(64, 300.0).is_empty());
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .unwrap_or_default();
        assert!(err.contains("does not belong"), "{err}");
    }

    /// A NULL-chunk embed job on a chunked entry (a direct PUBLIC queue
    /// insert) dead-letters as malformed instead of pinning the worker.
    #[pg_test]
    fn malformed_public_insert_on_chunked_entry_dead_letters() {
        let id = enable_chunked_docs(None);
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ($1, '1')",
            &[id.into()],
        )
        .unwrap();
        assert!(claim_and_read(64, 300.0).is_empty());
        let err = Spi::get_one::<String>("SELECT last_error FROM postvec.jobs_dead")
            .unwrap()
            .unwrap_or_default();
        assert!(err.contains("chunk-keyed"), "{err}");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    /// [R2-6]: releasing a claimed group of one document's child jobs (a
    /// routing change mid-flight) returns EVERY chunk to pending — this test
    /// fails against a two-column DISTINCT ON in unclaim_jobs().
    #[pg_test]
    fn released_child_group_returns_every_chunk_to_pending() {
        enable_chunked_docs(None);
        Spi::run("INSERT INTO docs (title, body) VALUES ('t', repeat('gamma ', 40))").unwrap();
        drain_refreshes();
        let n_chunks = Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks")
            .unwrap()
            .unwrap();
        assert!(n_chunks > 1, "need several children for the collapse test");

        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        let expected = group.routing.clone();
        let (nj, ej) = split_items(group.items);
        assert_eq!(ej.len() as i64, n_chunks);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let outcomes = embed_with_bisection(
            &super::mock::MockClient::new(3),
            "m",
            &Default::default(),
            1000,
            &texts,
        );

        // Force a routing change mid-flight: a migration starts.
        Spi::run("ALTER TABLE docs_chunks ADD COLUMN body_semantic_new vector(4)").unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.migrations
                 (registry_id, old_model, new_model, old_dim, new_dim, strategy,
                  new_column, rows_total)
             VALUES ($1, 'm', 'm2', 3, 4, 'convert', 'body_semantic_new', 0)",
            &[group.entry.id.into()],
        )
        .unwrap();
        Spi::run_with_args(
            "UPDATE postvec.registry SET state = 'migrating' WHERE id = $1",
            &[group.entry.id.into()],
        )
        .unwrap();

        let applied = apply_group(group.entry.id, &expected, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 0);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE claimed_at IS NULL AND op = 'embed' AND chunk_id IS NOT NULL"
            )
            .unwrap(),
            Some(n_chunks),
            "every chunk job survives the release — none collapse"
        );
    }

    /// [R2-2] end-to-end: a document edited during a backfill becomes
    /// searchable (its chunks materialize) without waiting for the whole
    /// child backlog to drain.
    #[pg_test]
    fn edited_document_materializes_during_backlog() {
        let id = enable_chunked_docs(None);
        // A backlog of child jobs below the inflight bound, so refreshes flow.
        Spi::run("INSERT INTO docs (title, body) SELECT 't', repeat('w ', 50) FROM generate_series(1, 5)")
            .unwrap();
        drain_refreshes();
        let backlog = Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'embed'")
            .unwrap()
            .unwrap();
        assert!(backlog > 0, "children are waiting for inference");

        // Edit a document while the backlog exists; its refresh runs without
        // the queue emptying.
        Spi::run("UPDATE docs SET body = 'edited content' WHERE id = 1").unwrap();
        drain_refreshes();
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM docs_chunks c
                  WHERE c.postvec_source_pk = 1 AND c.chunk_text = 'edited content'
                    AND $1 > 0",
                &[id.into()],
            )
            .unwrap(),
            Some(1),
            "the edited document's chunk text is materialized while children wait"
        );
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use crate::client::{PvError, RavennaCode};

    #[test]
    fn bisection_isolates_single_poison() {
        let client = super::mock::MockClient::new(3).with_poison("BAD");
        let texts: Vec<String> = ["a", "b", "BAD", "d"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        assert_eq!(out.len(), 4);
        assert!(matches!(out[0], ItemOutcome::Ok(_)));
        assert!(matches!(out[1], ItemOutcome::Ok(_)));
        assert!(
            matches!(out[2], ItemOutcome::Dead(_)),
            "poison row must dead-letter"
        );
        assert!(matches!(out[3], ItemOutcome::Ok(_)));
    }

    #[test]
    fn transient_error_retries_all() {
        let mut client = super::mock::MockClient::new(3);
        client.fail_all = Some(PvError::Remote {
            code: RavennaCode::Timeout,
            message: "boom".into(),
        });
        let texts: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let out = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        assert!(out.iter().all(|o| matches!(o, ItemOutcome::Retry(_))));
    }

    /// A provider auth failure (UPSTREAM_AUTH_FAILED, class Config) behaves
    /// like every other config-class error at the queue: uniform Retry
    /// outcomes — bounded backoff up to max_retries, never a straight
    /// dead-letter of data over a revoked key.
    #[test]
    fn provider_auth_failure_retries_not_dead() {
        let mut client = super::mock::MockClient::new(3);
        client.fail_all = Some(PvError::Remote {
            code: RavennaCode::UpstreamAuthFailed,
            message: "provider \"openai\": API request failed with status 401".into(),
        });
        let texts: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let out = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        assert!(out.iter().all(|o| matches!(o, ItemOutcome::Retry(_))));
    }

    #[test]
    fn permanent_error_dead_letters_all() {
        let mut client = super::mock::MockClient::new(3);
        client.fail_all = Some(PvError::Remote {
            code: RavennaCode::InvalidInput,
            message: "nope".into(),
        });
        let texts: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let out = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        assert!(out.iter().all(|o| matches!(o, ItemOutcome::Dead(_))));
    }

    #[test]
    fn count_mismatch_dead_letters_all() {
        let mut client = super::mock::MockClient::new(3);
        client.embed_len = Some(1);
        let texts: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let out = embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
        assert!(out.iter().all(|o| matches!(o, ItemOutcome::Dead(_))));
    }
}
