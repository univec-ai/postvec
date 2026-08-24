//! The migration driver: watermark-ordered batch conversion of existing
//! vectors into the migration's new column.
//!
//! Shaped like `crate::jobs`: pure SQL step functions that run in the ambient
//! transaction ([`read_migration_batch`], [`apply_migration_batch`]) plus the
//! network-side inference helper, composed by [`drain_step`] with the worker's
//! transaction boundaries — the read commits before the network call, the
//! write-back commits after it.
//!
//! Correctness properties:
//! - Both the batch read and the write-back carry a `<new_col> IS NULL`
//!   guard, so fresh embeds (routed to the new column by
//!   [`crate::jobs::embed_routing`]) always win over conversions of stale
//!   vectors.
//! - Iteration is watermark-ordered (`last_pk`), so rows the driver
//!   deliberately skips (non-finite or wrong-dimensional outputs; poison
//!   texts under the reembed strategy, isolated by bisection) are passed
//!   over exactly once instead of looping forever.
//! - Transient/config errors leave the migration `running` with `error` set,
//!   retried under the worker's in-memory exponential [`RetryBackoff`] —
//!   inference being down must not fail a migration, and neither must
//!   missing bridge inventory (`BridgePathNotFound`/`ConverterNotFound`:
//!   rollout skew or a load window, Config class); permanent errors
//!   (`InvalidInput`, `TargetRestricted`, no-such-model) mark it `failed`.

use crate::api::migrate::Migration;
use crate::client::grpc::GrpcClient;
use crate::client::{EmbedRoute, ErrorClass, InferenceClient, PvError, RavennaCode};
use crate::registry::RegistryEntryDb as _;
use crate::registry::{parse_vector, quote_ident, RegistryEntry};
use crate::runtime;
use crate::worker::{try_transaction, Counters};
use pgrx::prelude::*;

/// One readable batch of migration work.
pub struct MigrationBatch {
    pub migration: Migration,
    pub entry: RegistryEntry,
    /// Row text keys, in watermark order.
    pub pks: Vec<String>,
    /// Stored vector texts (convert) or source texts (reembed), parallel to
    /// `pks`. `Err(reason)` is a row decided at read time — a rendered text
    /// over the byte ceiling — that becomes [`RowResult::Skip`] without ever
    /// being materialized or sent (the watermark passes it exactly once).
    pub payloads: Vec<Result<String, String>>,
    /// The embed call (gRPC model + bridge route) for the reembed strategy,
    /// resolved at read time — bridged for convert-only target models.
    pub embed_call: Option<(String, EmbedRoute)>,
}

pub enum BatchRead {
    /// Nothing to do (not running, entry gone, or resolution failed softly).
    Idle,
    /// No rows left: the migration was moved to `awaiting_finalize`.
    Finished,
    Work(Box<MigrationBatch>),
}

/// Per-row inference result: a vector to write, or a reason to skip the row
/// (leave NULL, advance the watermark past it).
pub enum RowResult {
    Vector(Vec<f32>),
    Skip(String),
}

/// Read the next batch for a running migration. Must run inside a
/// transaction; commits (via the caller) before any network I/O.
pub fn read_migration_batch(migration_id: i64, limit: i32) -> BatchRead {
    let Some(m) = Migration::load(migration_id) else {
        return BatchRead::Idle;
    };
    if m.state != "running" {
        return BatchRead::Idle;
    }
    let Some(entry) = RegistryEntry::load(m.registry_id) else {
        fail_migration(migration_id, "registry entry vanished");
        return BatchRead::Idle;
    };
    if let Some(reason) = entry.missing_dependency_locked(&[]) {
        // Table (or a base column) dropped mid-migration: quarantine the
        // entry — that also fails this migration — instead of aborting the
        // worker on the batch read, forever.
        crate::api::registry::quarantine_entry(&entry, &reason);
        return BatchRead::Idle;
    }
    if let Some(reason) = entry.missing_dependency_locked(&[&m.new_column]) {
        // Only the migration's new column is gone (dropped manually): the
        // entry itself is fine — fail the migration; abort() cleans up.
        fail_migration(migration_id, &reason);
        return BatchRead::Idle;
    }

    let is_reembed = m.resolved_via["kind"] == "reembed";

    let embed_call = if is_reembed {
        match crate::api::embed::resolve_embed_route(&m.new_model) {
            Ok(resolution) => Some(resolution.into_call()),
            Err(e) => {
                // Cache may be stale (models refresh on cadence): retry later.
                set_migration_error(migration_id, &format!("resolve {:?}: {e}", m.new_model));
                return BatchRead::Idle;
            }
        }
    } else {
        None
    };

    // The driver's target relation and row key branch on the entry's vector
    // target: column mode iterates source rows by PK text exactly as
    // before; recursive mode iterates destination chunk rows by their
    // monotonic `postvec_chunk_id`** (rendered as text into the existing
    // `last_pk` watermark). Conversion never reads or sends chunk text;
    // reembed renders the chunk through the entry's `$chunk` template with
    // its live source context.
    // Every migration read carries the byte ceilings of the job engine —
    // reembed reads render document text; convert reads render stored-vector
    // text (up to ~hundreds of KiB per row at high dimensions, so a full
    // 4096-row batch could otherwise materialize hundreds of MiB). A row
    // over `item_cap` is returned with its payload stripped (it becomes a
    // Skip; never truncated, never materialized), and the batch stops once
    // the kept payloads reach the byte `budget` — the watermark stops with
    // it, so the tail is simply the next batch. Every batch keeps at least
    // one row (a first kept row fits by `item_cap <= budget`; a first
    // over-cap row contributes 0), so the driver always advances.
    let item_cap = crate::jobs::effective_item_cap();
    let budget = (crate::gucs::MAX_BATCH_TOTAL_BYTES.get() as i64).max(1);
    let budgeted =
        |inner_k: &str, expr: &str, from_clause: &str, where_clause: &str, order: &str| {
            format!(
                "SELECT k, CASE WHEN len <= {item_cap} THEN txt END AS txt, len
               FROM (
                 SELECT {inner_k} AS k, r.txt, octet_length(r.txt)::bigint AS len,
                        sum(CASE WHEN octet_length(r.txt) <= {item_cap}
                                 THEN octet_length(r.txt)::bigint ELSE 0 END)
                            OVER (ORDER BY {order}) AS cum,
                        row_number() OVER (ORDER BY {order}) AS ord
                   {from_clause}
                  CROSS JOIN LATERAL (SELECT ({expr})::text AS txt) r
                  WHERE {where_clause}
                  ORDER BY {order}
                  LIMIT {limit}
               ) s
              WHERE cum <= {budget}
              ORDER BY ord"
            )
        };
    let recursive_wm = m
        .last_pk
        .as_deref()
        .map(|w| {
            format!(
                " AND c.postvec_chunk_id > {}::bigint",
                crate::registry::quote_literal(w)
            )
        })
        .unwrap_or_default();
    let column_wm = m
        .last_pk
        .as_deref()
        .map(|w| entry.pk_watermark_clause("", w))
        .unwrap_or_default();
    let rows: Vec<(String, Result<String, String>)> = if is_reembed {
        // Reembed, two-phase: phase 1 reads keys + rendered
        // LENGTHS through the octet_length-sum expression — the database
        // never builds an over-ceiling row's text to measure it. Rust admits
        // a strict watermark-order prefix under the byte budget (over-cap
        // rows join the batch as Skips so the watermark passes them exactly
        // once); phase 2 renders the admitted rows only.
        let len_q = if entry.is_recursive() {
            let qdest = entry.qualified_vector_table();
            let new_col = quote_ident(&m.new_column);
            let len_expr = crate::registry::chunk_format_len_expr(&entry, "d", "c");
            format!(
                "SELECT c.postvec_chunk_id::text AS k, {len_expr} AS len
                   FROM {qdest} c JOIN {qsrc} d ON {join}
                  WHERE c.{new_col} IS NULL AND ({len_expr}) IS NOT NULL{recursive_wm}
                  ORDER BY c.postvec_chunk_id
                  LIMIT {limit}",
                qsrc = entry.qualified_table(),
                join = entry.source_pk_join("d", "c"),
            )
        } else {
            let guard_col = quote_ident(&entry.source_column);
            format!(
                "SELECT {pk} AS k, {len_expr} AS len
                   FROM {tbl}
                  WHERE {new_col} IS NULL AND {guard_col} IS NOT NULL{column_wm}
                  ORDER BY {order}
                  LIMIT {limit}",
                pk = entry.pk_text_expr(""),
                len_expr = crate::registry::format_len_expr(&entry, ""),
                tbl = entry.qualified_table(),
                new_col = quote_ident(&m.new_column),
                order = entry.pk_order_expr(""),
            )
        };
        let lens: Vec<(String, Option<i64>)> = Spi::connect(|c| {
            let t = c
                .select(len_q.as_str(), None, &[])
                .expect("postvec: migration length scan failed");
            t.into_iter()
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<i64>(2).unwrap(),
                    )
                })
                .collect()
        });

        let mut rows: Vec<(String, Result<String, String>)> = Vec::with_capacity(lens.len());
        let mut admitted: Vec<String> = Vec::new();
        let mut admitted_lens: Vec<i64> = Vec::new();
        let mut used = 0i64;
        for (k, len) in lens {
            let Some(len) = len else {
                // The guard requires a renderable row; NULL here is a raced
                // concurrent write — skip, never poison.
                rows.push((k, Err("payload read back NULL (concurrent write?)".into())));
                continue;
            };
            if len > item_cap {
                rows.push((
                    k,
                    Err(format!(
                        "payload is {len} bytes; the ceiling is {item_cap} \
                         (min of postvec.max_document_bytes, \
                         postvec.max_batch_total_bytes, and the 48 MiB \
                         wire-message ceiling); left NULL"
                    )),
                ));
                continue;
            }
            if used.saturating_add(len) > budget {
                // Strict prefix: the tail is simply the next batch.
                break;
            }
            used += len;
            admitted.push(k.clone());
            admitted_lens.push(len);
            rows.push((k, Err("pending render".into())));
        }

        if !admitted.is_empty() {
            // Per-row admitted-length recheck, exactly like the claim reads:
            // a row that grew between the phases is not rendered (its
            // concurrent writer's trigger re-enqueued it as a fresh embed
            // that targets the new column directly).
            let text_q = if entry.is_recursive() {
                let qdest = entry.qualified_vector_table();
                let len_expr = crate::registry::chunk_format_len_expr(&entry, "d", "c");
                let expr = crate::registry::chunk_format_expr(&entry, "d", "c");
                format!(
                    "SELECT c.postvec_chunk_id::text,
                            CASE WHEN ({len_expr}) <= u.len THEN ({expr})::text END
                       FROM unnest($1::text[], $2::int8[]) AS u(k, len)
                       JOIN {qdest} c ON c.postvec_chunk_id = u.k::bigint
                       JOIN {qsrc} d ON {join}",
                    qsrc = entry.qualified_table(),
                    join = entry.source_pk_join("d", "c"),
                )
            } else {
                let len_expr = crate::registry::format_len_expr(&entry, "t");
                let expr = crate::registry::format_expr(&entry, "t");
                format!(
                    "SELECT u.k,
                            CASE WHEN ({len_expr}) <= u.len THEN ({expr})::text END
                       FROM unnest($1::text[], $2::int8[]) AS u(k, len)
                       JOIN {tbl} t ON {join}",
                    tbl = entry.qualified_table(),
                    join = entry.pk_staging_join_clause("t", "u", "k"),
                )
            };
            let mut texts: std::collections::BTreeMap<String, String> = Spi::connect(|c| {
                let t = c
                    .select(
                        text_q.as_str(),
                        None,
                        &[admitted.into(), admitted_lens.into()],
                    )
                    .expect("postvec: migration render failed");
                t.into_iter()
                    .filter_map(|r| {
                        let k = r.get::<String>(1).unwrap().unwrap();
                        r.get::<String>(2).unwrap().map(|txt| (k, txt))
                    })
                    .collect()
            });
            for (k, payload) in rows.iter_mut() {
                if matches!(payload, Err(e) if e == "pending render") {
                    *payload = match texts.remove(k) {
                        Some(txt) => Ok(txt),
                        // Vanished, went NULL, or GREW between the phases:
                        // skipped; a concurrent writer's fresh embed job
                        // owns the row now.
                        None => Err("row changed between read phases".into()),
                    };
                }
            }
        }
        rows
    } else {
        // Convert reads stored vectors — dimension-bounded values whose text
        // rendering is cheap relative to documents — through the single-pass
        // budgeted query.
        let q = if entry.is_recursive() {
            let qdest = entry.qualified_vector_table();
            let new_col = quote_ident(&m.new_column);
            let v = quote_ident(&entry.vector_column);
            budgeted(
                "c.postvec_chunk_id::text",
                &format!("c.{v}"),
                &format!("FROM {qdest} c"),
                &format!("c.{new_col} IS NULL AND c.{v} IS NOT NULL{recursive_wm}"),
                "c.postvec_chunk_id",
            )
        } else {
            let v = quote_ident(&entry.vector_column);
            budgeted(
                &entry.pk_text_expr(""),
                &v,
                &format!("FROM {tbl}", tbl = entry.qualified_table()),
                &format!(
                    "{new_col} IS NULL AND {v} IS NOT NULL{column_wm}",
                    new_col = quote_ident(&m.new_column),
                ),
                &entry.pk_order_expr(""),
            )
        };
        Spi::connect(|c| {
            let t = c
                .select(q.as_str(), None, &[])
                .expect("postvec: migration batch read failed");
            t.into_iter()
                .map(|r| {
                    let k = r.get::<String>(1).unwrap().unwrap();
                    let txt = r.get::<String>(2).unwrap();
                    let len = r.get::<i64>(3).unwrap();
                    let payload = match (txt, len) {
                        (Some(t), _) => Ok(t),
                        (None, Some(l)) => Err(format!(
                            "payload is {l} bytes; the ceiling is {item_cap} \
                             (min of postvec.max_document_bytes, \
                             postvec.max_batch_total_bytes, and the 48 MiB \
                             wire-message ceiling); left NULL"
                        )),
                        (None, None) => Err("payload read back NULL (concurrent write?)".into()),
                    };
                    (k, payload)
                })
                .collect()
        })
    };

    if rows.is_empty() {
        Spi::run_with_args(
            "UPDATE postvec.migrations
                SET state = 'awaiting_finalize', error = NULL
              WHERE id = $1 AND state = 'running'",
            &[migration_id.into()],
        )
        .unwrap();
        log!(
            "postvec: migration {migration_id} converted all rows; run \
             postvec.migration_finalize({migration_id}) to swap columns"
        );
        return BatchRead::Finished;
    }

    let (pks, payloads) = rows.into_iter().unzip();
    BatchRead::Work(Box::new(MigrationBatch {
        migration: m,
        entry,
        pks,
        payloads,
        embed_call,
    }))
}

/// The network phase (no transaction): convert or re-embed a batch's payloads.
/// Returns one result per row, in order.
///
/// The reembed leg rides [`crate::jobs::bisect_embed`] (the shared bisection
/// core): an isolated `ContextLengthExceeded` poison row becomes a
/// [`RowResult::Skip`] — left NULL, the watermark moves past it exactly once
/// — while any other failure aborts the whole batch so [`drain_step`]'s
/// retry/fail policy applies and the watermark stays put. Without this, one
/// oversized text left a reembed migration `running` and retrying the same
/// batch forever.
pub fn run_inference<C: InferenceClient>(
    client: &C,
    batch: &MigrationBatch,
    timeout_ms: u64,
) -> Result<Vec<RowResult>, PvError> {
    let m = &batch.migration;
    // Sub-batch every network call so no single response (sized by the NEW
    // dimension) or request (old-dimension vectors / rendered text) can
    // exceed the transport's wire ceilings.
    let max_items = crate::jobs::max_items_for_dim(m.new_dim.max(m.old_dim));
    let outputs: Vec<RowResult> = if let Some((model, route)) = &batch.embed_call {
        // Rows the read already decided to skip (over-ceiling renders) never
        // reach the network; only the Ok payloads are embedded, then merged
        // back in order.
        let texts: Vec<String> = batch
            .payloads
            .iter()
            .filter_map(|p| p.as_ref().ok().cloned())
            .collect();
        let mut embedded = Vec::with_capacity(texts.len());
        for chunk in crate::jobs::request_chunks(&texts, m.new_dim) {
            embedded.extend(crate::jobs::bisect_embed(
                client, model, route, timeout_ms, chunk,
            )?);
        }
        let mut out_iter = embedded.into_iter();
        batch
            .payloads
            .iter()
            .map(|p| match p {
                Ok(_) => match out_iter.next().expect("length verified above") {
                    Ok(v) => RowResult::Vector(v),
                    Err(poison) => RowResult::Skip(poison),
                },
                Err(reason) => RowResult::Skip(reason.clone()),
            })
            .collect()
    } else {
        // Parse the stored vectors; anything unparseable is skipped (cannot
        // happen for healthy pgvector data, but never poison a whole batch).
        let mut parsed: Vec<Result<Vec<f32>, String>> = Vec::with_capacity(batch.payloads.len());
        for p in &batch.payloads {
            parsed.push(match p {
                Ok(text) => {
                    parse_vector(text).map_err(|e| format!("unparseable stored vector: {e}"))
                }
                Err(reason) => Err(reason.clone()),
            });
        }
        let good: Vec<Vec<f32>> = parsed.iter().filter_map(|r| r.clone().ok()).collect();
        let converted = if good.is_empty() {
            Vec::new()
        } else {
            let model = convert_target(m);
            let mut converted = Vec::with_capacity(good.len());
            for chunk in good.chunks(max_items.max(1)) {
                converted.extend(convert_with_split(client, &model, chunk, timeout_ms, 3)?);
            }
            converted
        };
        let mut out_iter = converted.into_iter();
        parsed
            .into_iter()
            .map(|r| match r {
                Ok(_) => RowResult::Vector(out_iter.next().expect("length verified above")),
                Err(e) => RowResult::Skip(e),
            })
            .collect()
    };

    // Validate outputs row-by-row: wrong dimension or non-finite components
    // would make the write-back fail wholesale — skip those rows instead.
    Ok(outputs
        .into_iter()
        .map(|r| match r {
            RowResult::Vector(v) if v.len() as i32 != m.new_dim => RowResult::Skip(format!(
                "model returned {} dims, column is vector({})",
                v.len(),
                m.new_dim
            )),
            RowResult::Vector(v) if !v.iter().all(|f| f.is_finite()) => {
                RowResult::Skip("model returned a non-finite vector (NaN/Inf)".into())
            }
            other => other,
        })
        .collect())
}

/// Convert `vecs`, halving the batch (depth-limited) on a whole-batch
/// deadline: an oversized batch that can never finish inside the RPC
/// deadline converges to smaller requests instead of retrying at the same
/// size forever (the embed leg gets the equivalent through
/// [`crate::jobs::bisect_embed`]). A genuinely down node fails after at most
/// `depth` extra attempts down the leftmost split and stays a transient
/// error, so the migration's exponential backoff applies as before.
fn convert_with_split<C: InferenceClient>(
    client: &C,
    model: &str,
    vecs: &[Vec<f32>],
    timeout_ms: u64,
    depth: u8,
) -> Result<Vec<Vec<f32>>, PvError> {
    let res =
        runtime::block_on_with_timeout(timeout_ms, async { client.convert(vecs, model).await });
    match res {
        Ok(out) if out.len() == vecs.len() => Ok(out),
        Ok(out) => Err(PvError::Decode(format!(
            "conversion count mismatch: got {}, want {}",
            out.len(),
            vecs.len()
        ))),
        Err(
            PvError::Deadline { .. }
            | PvError::Remote {
                code: RavennaCode::Timeout,
                ..
            },
        ) if vecs.len() > 1 && depth > 0 => {
            log!(
                "postvec: convert batch of {} timed out; retrying in halves \
                 (lower postvec.migrate_batch_size if this recurs)",
                vecs.len()
            );
            let mid = vecs.len() / 2;
            let mut out = convert_with_split(client, model, &vecs[..mid], timeout_ms, depth - 1)?;
            out.extend(convert_with_split(
                client,
                model,
                &vecs[mid..],
                timeout_ms,
                depth - 1,
            )?);
            Ok(out)
        }
        Err(e) => Err(e),
    }
}

/// The gRPC target for a convert-strategy batch, from the pre-flight result
/// stored in `resolved_via`: the direct convert model's name. Two-hop
/// convert-bridge routes no longer exist — the preflight only ever writes
/// `{"kind": "direct"}` or `{"kind": "reembed"}` (which never reaches this
/// function), and an older row's bridge kind falls through to the old-model
/// fallback, whose failure is a truthful "no such route" rather than a
/// silent chain.
fn convert_target(m: &Migration) -> String {
    m.resolved_via["model"]
        .as_str()
        .unwrap_or(&m.old_model)
        .to_string()
}

/// Outcome of one migration write-back: rows written / skipped, and whether
/// the watermark advanced (a batch can legitimately write 0 rows — every row
/// guarded away by fresh embeds — and still be progress).
#[derive(Debug, Default, Clone, Copy)]
pub struct MigrationApplied {
    pub done: i64,
    pub skipped: i64,
    pub advanced: bool,
}

/// Write a batch's results and advance the watermark. Must run inside a
/// transaction.
pub fn apply_migration_batch(batch: &MigrationBatch, results: &[RowResult]) -> MigrationApplied {
    let none = MigrationApplied::default();
    let Some(m) = Migration::load(batch.migration.id) else {
        return none;
    };
    if m.state != "running" {
        return none;
    }
    let Some(entry) = RegistryEntry::load(m.registry_id) else {
        fail_migration(m.id, "registry entry vanished before migration write-back");
        return none;
    };
    if let Some(reason) = entry.missing_dependency_locked(&[]) {
        crate::api::registry::quarantine_entry(&entry, &reason);
        return none;
    }
    if let Some(reason) = entry.missing_dependency_locked(&[&m.new_column]) {
        fail_migration(m.id, &reason);
        return none;
    }

    let mut ok_pks: Vec<String> = Vec::new();
    let mut ok_vecs: Vec<String> = Vec::new();
    let mut skipped = 0i64;
    for (pk, res) in batch.pks.iter().zip(results.iter()) {
        match res {
            RowResult::Vector(v) => {
                ok_pks.push(pk.clone());
                ok_vecs.push(crate::registry::serialize_vector(v));
            }
            RowResult::Skip(reason) => {
                skipped += 1;
                warning!("postvec: migration {} skips row {pk:?}: {reason}", m.id);
            }
        }
    }

    let mut done = 0i64;
    if !ok_pks.is_empty() {
        // `new_col IS NULL` guard: a fresh embed that landed while we were
        // converting wins — its vector came from the new model directly.
        // rows_done counts the rows actually written (RETURNING), not the
        // batch size — guarded-away rows were done by the job engine, not us.
        // A recursive batch keys the write by the chunk identity; a chunk
        // replaced mid-batch simply matches zero rows (its replacement's
        // child job embeds the new model directly into the scratch column).
        let predicate = if entry.is_recursive() {
            "t.postvec_chunk_id = d.pk::bigint".to_string()
        } else {
            entry.pk_staging_join_clause("t", "d", "pk")
        };
        let q = format!(
            "UPDATE {tbl} t SET {new_col} = d.v::vector
               FROM (SELECT unnest($1::text[]) AS pk, unnest($2::text[]) AS v) d
              WHERE {predicate} AND t.{new_col} IS NULL
              RETURNING 1",
            tbl = entry.qualified_vector_table(),
            new_col = quote_ident(&m.new_column),
        );
        done = Spi::connect_mut(|c| {
            c.update(
                q.as_str(),
                None,
                &[ok_pks.clone().into(), ok_vecs.clone().into()],
            )
            .unwrap()
            .len() as i64
        });
    }

    Spi::run_with_args(
        "UPDATE postvec.migrations
            SET rows_done = rows_done + $1,
                rows_skipped = rows_skipped + $2,
                last_pk = $3,
                error = NULL,
                retry_failures = 0
          WHERE id = $4",
        &[
            done.into(),
            skipped.into(),
            batch.pks.last().map(|s| s.as_str()).into(),
            m.id.into(),
        ],
    )
    .unwrap();
    MigrationApplied {
        done,
        skipped,
        advanced: true,
    }
}

/// Record a retryable error: the migration stays `running`, but its
/// `not_before` is pushed out exponentially (base `postvec.retry_backoff_ms`,
/// capped at 5 minutes) so an unreachable inference host is not hammered every
/// poll tick. Persisted in the row — same idiom as `jobs.not_before` — so it
/// survives worker restarts and is visible in SQL. A successful batch resets
/// it ([`apply_migration_batch`]).
pub fn set_migration_error(migration_id: i64, msg: &str) {
    let base_secs = crate::gucs::RETRY_BACKOFF_MS.get().max(0) as f64 / 1000.0;
    // `state = 'running'` guard: an abort/finalize racing a failing batch
    // must keep its terminal state — never have its audit row rewritten.
    Spi::run_with_args(
        "UPDATE postvec.migrations
            SET error = left($2, 1024),
                retry_failures = retry_failures + 1,
                not_before = now() + make_interval(secs =>
                    LEAST($3 * power(2::float8, LEAST(retry_failures, 10)::float8), 300.0))
          WHERE id = $1 AND state = 'running'",
        &[migration_id.into(), msg.into(), base_secs.into()],
    )
    .unwrap();
}

/// Permanent failure: the migration stops. The registry stays `migrating` for
/// operator visibility, but fresh writes route back to the old column because
/// embed routing only follows `running` / `awaiting_finalize` migrations.
pub fn fail_migration(migration_id: i64, msg: &str) {
    // Same racing-abort guard as set_migration_error, and stamp finished_at —
    // 'failed' is terminal for the driver (only abort() moves it further).
    Spi::run_with_args(
        "UPDATE postvec.migrations
            SET state = 'failed', error = $2, finished_at = now()
          WHERE id = $1 AND state = 'running'",
        &[migration_id.into(), msg.into()],
    )
    .unwrap();
    warning!("postvec: migration {migration_id} failed: {msg}");
}

/// Worker-side: process one batch for every running migration whose
/// `not_before` retry gate has passed ([`Migration::running_ids`] filters).
/// Returns whether any batch made progress (the worker keeps draining while
/// true).
pub fn drain_step(counters: &mut Counters) -> bool {
    let mids = match try_transaction(Migration::running_ids) {
        Ok(mids) => mids,
        Err(e) => {
            counters.error(format!("migration scan: {e}"));
            warning!("postvec: migration scan failed (will retry): {e}");
            return false;
        }
    };
    if mids.is_empty() {
        return false;
    }
    let batch_size = crate::gucs::MIGRATE_BATCH_SIZE.get().max(1);
    let timeout_ms = crate::gucs::EMBED_TIMEOUT_MS.get().max(100) as u64;
    let client = GrpcClient::from_gucs(timeout_ms);
    let request_timeout_ms = client.overall_timeout_ms();

    /// Record a transient failure in its own transaction (the failing one
    /// rolled back).
    fn record_soft_failure(mid: i64, msg: String) {
        if let Err(e) = try_transaction(move || set_migration_error(mid, &msg)) {
            warning!("postvec: recording migration {mid} error failed too: {e}");
        }
    }

    let mut did_work = false;
    for mid in mids {
        if crate::worker::shutdown_requested() {
            // Honour SIGTERM between migrations; nothing is lost (the read
            // transaction has not started for this one yet).
            return did_work;
        }
        let read = try_transaction(move || read_migration_batch(mid, batch_size));
        let batch = match read {
            Ok(BatchRead::Idle) | Ok(BatchRead::Finished) => continue,
            Ok(BatchRead::Work(b)) => b,
            Err(e) => {
                // e.g. lock_timeout on a blocked user table: retryable.
                counters.error(format!("migration {mid}: {e}"));
                warning!(
                    "postvec: migration {mid} batch read failed ({e}); will retry with backoff"
                );
                record_soft_failure(mid, format!("batch read failed: {e}"));
                continue;
            }
        };

        // Network phase (no transaction).
        match run_inference(&client, &batch, request_timeout_ms) {
            Ok(results) => {
                let applied = match try_transaction(move || apply_migration_batch(&batch, &results))
                {
                    Ok(applied) => applied,
                    Err(e) => {
                        // A raising constraint/trigger on the new column,
                        // or a lock timeout: keep the migration running
                        // with backoff; the watermark did not advance.
                        counters.error(format!("migration {mid}: {e}"));
                        warning!(
                            "postvec: migration {mid} write-back failed ({e}); \
                                 will retry with backoff"
                        );
                        record_soft_failure(mid, format!("write-back failed: {e}"));
                        continue;
                    }
                };
                counters.converted += applied.done;
                counters.skipped += applied.skipped;
                did_work |= applied.advanced;
                log!(
                    "postvec: migration {mid} — {} converted, {} skipped",
                    applied.done,
                    applied.skipped
                );
            }
            Err(e) => {
                let msg = format!("{e}");
                let permanent = matches!(e.class(), ErrorClass::Permanent);
                let res = try_transaction(move || {
                    if permanent {
                        fail_migration(mid, &msg);
                    } else {
                        set_migration_error(mid, &msg);
                    }
                });
                if let Err(e2) = res {
                    warning!("postvec: recording migration {mid} outcome failed: {e2}");
                }
                counters.error(format!("migration {mid}: {e}"));
                warning!(
                    "postvec: migration {mid} batch failed ({e}); {}",
                    if permanent {
                        "marked failed"
                    } else {
                        "will retry with backoff"
                    }
                );
            }
        }
    }
    did_work
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;
    use crate::api::migrate::test_fixtures::{docs_enabled, seed_models};
    use crate::jobs::mock::MockClient;

    /// Drive a migration to completion the way `drain_step` does, but in the
    /// ambient test transaction (no BackgroundWorker). Returns
    /// `(converted, skipped, finished)`.
    fn drive<C: InferenceClient>(client: &C, mid: i64, batch: i32) -> (i64, i64, bool) {
        let (mut converted, mut skipped) = (0i64, 0i64);
        for _ in 0..1000 {
            match read_migration_batch(mid, batch) {
                BatchRead::Idle => return (converted, skipped, false),
                BatchRead::Finished => return (converted, skipped, true),
                BatchRead::Work(b) => match run_inference(client, &b, 1000) {
                    Ok(results) => {
                        let applied = apply_migration_batch(&b, &results);
                        converted += applied.done;
                        skipped += applied.skipped;
                    }
                    Err(e) => {
                        if matches!(e.class(), ErrorClass::Permanent) {
                            fail_migration(mid, &format!("{e}"));
                        } else {
                            set_migration_error(mid, &format!("{e}"));
                        }
                        return (converted, skipped, false);
                    }
                },
            }
        }
        panic!("driver did not converge");
    }

    #[pg_test]
    fn convert_driver_end_to_end_with_finalize() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();

        // Batch size 2 forces multiple watermark-ordered batches.
        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!(converted, 3, "all three old vectors convert");
        assert_eq!(skipped, 0);
        assert!(finished, "driver reaches awaiting_finalize");

        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic_new IS NOT NULL")
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_finalize")
        );

        // Finalize: swap columns, update registry. No index existed, so the
        // migration completes outright even in 'manual' reindex mode.
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        let typ = Spi::get_one::<String>(
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_attribute
              WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic'
                AND NOT attisdropped",
        )
        .unwrap();
        assert_eq!(
            typ.as_deref(),
            Some("vector(4)"),
            "column swapped to the new dim"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'body_semantic_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "temp column name gone after rename"
        );
        let (model, dim, state) = (
            Spi::get_one::<String>("SELECT model FROM postvec.registry").unwrap(),
            Spi::get_one::<i32>("SELECT dim FROM postvec.registry").unwrap(),
            Spi::get_one::<String>("SELECT state FROM postvec.registry").unwrap(),
        );
        assert_eq!(model.as_deref(), Some("m2"));
        assert_eq!(dim, Some(4));
        assert_eq!(state.as_deref(), Some("active"));
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done")
        );
    }

    /// Add a convert-only target to the fixture: 'ext' (dim 6) exists only as
    /// conv-m-ext's target space, plus an embed-bridge executor. This is the
    /// "migrate to a commercial embedding model with no provider API key"
    /// scenario: existing vectors convert; fresh writes embed via the bridge.
    fn seed_convert_only_target() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('conv-m-ext', 'convert', 'm', 'ext', 6, '{}'::jsonb),
                    ('embed-bridge', 'embed-bridge', NULL, NULL, NULL, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
    }

    /// Migrating to a model with no embed model of its own: the dimension
    /// comes from the converter, stored vectors convert as usual, a fresh
    /// write that lands mid-migration is embedded through the embed-bridge
    /// into the new column, and finalize swaps the column.
    #[pg_test]
    fn migrate_to_convert_only_target_end_to_end() {
        use crate::jobs::{apply_group, claim_and_read, embed_with_bisection, split_items};

        docs_enabled();
        seed_convert_only_target();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','ext')")
            .unwrap()
            .unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic_new'"
            )
            .unwrap()
            .as_deref(),
            Some("vector(6)"),
            "new column sized from the converter's target_dim"
        );

        // A fresh write mid-migration routes to the new model — which has no
        // embed model: it must ride the embed-bridge into the new column.
        let client = MockClient::new(3).with_convert_dim(6).with_bridge_dim(6);
        Spi::run("INSERT INTO docs (body) VALUES ('fresh mid-migration row')").unwrap();
        let group = claim_and_read(64, 300.0).into_iter().next().unwrap();
        assert_eq!(group.routing.model, "ext");
        assert_eq!(group.routing.vector_column, "body_semantic_new");
        let (null_jobs, embed_jobs) = split_items(group.items);
        let (model, route) = crate::api::embed::resolve_embed_route(&group.routing.model)
            .expect("convert-only migration target must stay embeddable via the bridge")
            .into_call();
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
        assert_eq!(applied.done, 1);
        let (called_model, called_route) = client.last_embed_call().unwrap();
        assert_eq!(called_model, "embed-bridge");
        assert_eq!(called_route.bridge_model.as_deref(), Some("m"));
        assert_eq!(called_route.target_model.as_deref(), Some("ext"));

        // The driver converts the three stored vectors (the fresh row has no
        // old vector to convert and is already filled).
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!((converted, skipped), (3, 0));
        assert!(finished);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs
                  WHERE body_semantic_new IS NOT NULL AND vector_dims(body_semantic_new) = 6"
            )
            .unwrap(),
            Some(4),
            "three conversions + one bridged fresh embed"
        );

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT model FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("ext")
        );
        assert_eq!(
            Spi::get_one::<i32>("SELECT dim FROM postvec.registry").unwrap(),
            Some(6)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );
    }

    /// A reembed migration to a convert-only target: the reembed leg itself
    /// rides the embed-bridge (embed source text with the converter's source
    /// model, convert engine-side), answered in the new dimension.
    #[pg_test]
    fn reembed_migration_to_bridged_target() {
        docs_enabled();
        seed_convert_only_target();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','ext', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();

        let client = MockClient::new(3).with_bridge_dim(6);
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!(
            (converted, skipped),
            (4, 0),
            "all four rows with source text re-embed via the bridge"
        );
        assert!(finished);
        let (called_model, called_route) = client.last_embed_call().unwrap();
        assert_eq!(called_model, "embed-bridge");
        assert_eq!(called_route.bridge_model.as_deref(), Some("m"));
        assert_eq!(called_route.target_model.as_deref(), Some("ext"));
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs
                  WHERE body_semantic_new IS NOT NULL AND vector_dims(body_semantic_new) = 6"
            )
            .unwrap(),
            Some(4)
        );
    }

    /// The `new_col IS NULL` guard: a fresh embed that landed mid-migration
    /// must not be overwritten by a conversion of the stale old vector.
    #[pg_test]
    fn fresh_embed_wins_over_conversion() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        // Simulate the job engine writing a fresh new-model embed for row 1.
        Spi::run("UPDATE docs SET body_semantic_new = '[9,9,9,9]'::vector WHERE id = 1").unwrap();

        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, _, finished) = drive(&client, mid, 64);
        assert!(finished);
        assert_eq!(converted, 2, "row 1 is already filled and is not re-read");

        let v = Spi::get_one::<String>("SELECT body_semantic_new::text FROM docs WHERE id = 1")
            .unwrap();
        assert_eq!(v.as_deref(), Some("[9,9,9,9]"), "fresh embed preserved");
    }

    /// Non-finite conversion output: the row is skipped, the watermark moves
    /// past it, and the migration still completes.
    #[pg_test]
    fn non_finite_output_skips_row_and_advances() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            convert_nan_index: Some(0), // first row of every call
            ..Default::default()
        };
        // One big batch: row 1 comes back NaN, rows 2 and 3 convert.
        let (converted, skipped, finished) = drive(&client, mid, 64);
        assert!(finished);
        assert_eq!(converted, 2);
        assert_eq!(skipped, 1);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs WHERE id = 1 AND body_semantic_new IS NULL"
            )
            .unwrap(),
            Some(1),
            "the NaN row stays NULL"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT rows_skipped FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Some(1)
        );
    }

    /// rows_done must count rows actually written by the driver: a fresh
    /// embed landing between the batch read and the write-back is excluded by
    /// the `new_col IS NULL` guard and must not inflate the counter.
    #[pg_test]
    fn rows_done_counts_only_driver_writes() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let BatchRead::Work(batch) = read_migration_batch(mid, 64) else {
            panic!("expected a batch");
        };
        // A fresh embed lands on row 2 while the (mock) conversion runs.
        Spi::run("UPDATE docs SET body_semantic_new = '[9,9,9,9]'::vector WHERE id = 2").unwrap();

        let client = MockClient::new(3).with_convert_dim(4);
        let results = run_inference(&client, &batch, 1000).unwrap();
        let applied = apply_migration_batch(&batch, &results);
        assert_eq!(
            applied.done, 2,
            "only the two guard-passing rows count as done"
        );
        assert_eq!(applied.skipped, 0);
        assert!(applied.advanced, "the watermark still advanced");
        assert_eq!(
            Spi::get_one::<String>("SELECT body_semantic_new::text FROM docs WHERE id = 2")
                .unwrap()
                .as_deref(),
            Some("[9,9,9,9]"),
            "the fresh embed survives"
        );
    }

    #[pg_test]
    fn abort_racing_in_flight_batch_is_ignored_on_apply() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let BatchRead::Work(batch) = read_migration_batch(mid, 64) else {
            panic!("expected a batch");
        };
        let client = MockClient::new(3).with_convert_dim(4);
        let results = run_inference(&client, &batch, 1000).unwrap();

        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();
        let applied = apply_migration_batch(&batch, &results);
        assert_eq!((applied.done, applied.skipped), (0, 0));
        assert!(
            !applied.advanced,
            "an aborted migration reports no progress"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("aborted")
        );
    }

    /// DROP TABLE mid-migration: the driver must fail the migration and
    /// quarantine the entry instead of aborting the worker on the batch read.
    #[pg_test]
    fn dropped_table_mid_migration_fails_and_quarantines() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run("DROP TABLE docs").unwrap();

        assert!(matches!(read_migration_batch(mid, 64), BatchRead::Idle));
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("failed")
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("disabled"),
            "the entry is quarantined"
        );
    }

    /// Dropping only the migration's new column fails the migration but the
    /// entry itself stays active (abort() can clean up).
    #[pg_test]
    fn dropped_new_column_fails_migration_only() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run("ALTER TABLE docs DROP COLUMN body_semantic_new").unwrap();

        assert!(matches!(read_migration_batch(mid, 64), BatchRead::Idle));
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("failed")
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("migrating"),
            "entry not quarantined for a missing migration column"
        );
        // The failed migration aborts cleanly.
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );
    }

    /// The reembed leg reads the entry's rendered template, not the bare
    /// source column; a NULL source row stays outside the batch (the guard is
    /// anchored on the raw column).
    #[pg_test]
    fn reembed_migration_uses_rendered_text() {
        seed_models();
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
        Spi::run("INSERT INTO docs (title, body) VALUES ('T', 'B'), ('T2', NULL)").unwrap();
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();

        let BatchRead::Work(batch) = read_migration_batch(mid, 64) else {
            panic!("expected a work batch");
        };
        assert_eq!(batch.pks, vec!["1"], "the NULL-source row is not batched");
        assert_eq!(
            batch.payloads,
            vec![Ok("T — B".to_string())],
            "the reembed payload is the rendered template"
        );
    }

    #[pg_test]
    fn reembed_driver_embeds_text_rows() {
        docs_enabled();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        // m3 is dim 5; the mock embeds everything at dim 5.
        let client = MockClient::new(5);
        let (converted, _, finished) = drive(&client, mid, 64);
        assert!(finished);
        assert_eq!(
            converted, 4,
            "all four rows have text (incl. the vectorless one)"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic_new IS NOT NULL")
                .unwrap(),
            Some(4)
        );
    }

    /// One oversized text (ContextLengthExceeded) in a reembed migration is
    /// skipped via bisection, not retried as the same batch forever.
    #[pg_test]
    fn reembed_poison_row_is_skipped_not_retried_forever() {
        docs_enabled();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        let client = MockClient::new(5).with_poison("bb");
        let (converted, skipped, finished) = drive(&client, mid, 64);
        assert!(finished, "the migration completes despite the poison row");
        assert_eq!(converted, 3, "the healthy rows re-embed");
        assert_eq!(skipped, 1, "the poison row is skipped exactly once");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs WHERE body = 'bb' AND body_semantic_new IS NULL"
            )
            .unwrap(),
            Some(1),
            "the poison row stays NULL"
        );
    }

    /// A permanent error during reembed still fails the migration (it must
    /// not be silently skipped row-by-row).
    #[pg_test]
    fn permanent_reembed_error_fails_migration() {
        docs_enabled();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        let client = MockClient {
            dim: 5,
            fail_all: Some(crate::client::PvError::Remote {
                code: crate::client::RavennaCode::InvalidInput,
                message: "bad".into(),
            }),
            ..Default::default()
        };
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 0);
        assert!(!finished);
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("failed")
        );
    }

    /// A transient error during reembed keeps the migration running (retried
    /// later) with the error surfaced, and the watermark stays put.
    #[pg_test]
    fn transient_reembed_error_keeps_running() {
        docs_enabled();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        let client = MockClient {
            dim: 5,
            fail_all: Some(crate::client::PvError::Remote {
                code: crate::client::RavennaCode::Timeout,
                message: "busy".into(),
            }),
            ..Default::default()
        };
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(!finished);
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("running")
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT last_pk FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            None,
            "no watermark advance on a transient whole-batch failure"
        );

        // inference recovers: the same migration completes.
        let ok_client = MockClient::new(5);
        let (converted, _, finished) = drive(&ok_client, mid, 64);
        assert_eq!(converted, 4);
        assert!(finished);
    }

    #[pg_test]
    fn permanent_convert_error_fails_migration() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            // TargetRestricted is the canonical permanent code: a policy
            // refusal, not missing inventory (which is Config and retries).
            fail_convert: Some(crate::client::PvError::Remote {
                code: crate::client::RavennaCode::TargetRestricted,
                message: "target blocked by deployment policy".into(),
            }),
            ..Default::default()
        };
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 0);
        assert!(!finished);
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("failed")
        );
        // A failed migration can still be aborted cleanly.
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );
    }

    /// Bridge-inventory errors (`ConverterNotFound`/`BridgePathNotFound`) are
    /// Config, not permanent: the chain may be mid-rollout or in a model
    /// load/unload window, so the migration keeps retrying (visible in
    /// `migration_status().error`) and completes once inventory recovers.
    #[pg_test]
    fn bridge_inventory_convert_error_keeps_migration_running() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            fail_convert: Some(crate::client::PvError::Remote {
                code: crate::client::RavennaCode::ConverterNotFound,
                message: "chain incomplete on this node".into(),
            }),
            ..Default::default()
        };
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 0);
        assert!(!finished);
        let (state, error) = (
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<String>(
                "SELECT error FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(
            state.as_deref(),
            Some("running"),
            "missing bridge inventory retries instead of failing the migration"
        );
        assert!(error.is_some(), "error surfaced for status()");

        // Inventory recovers: the same migration completes.
        let ok_client = MockClient::new(3).with_convert_dim(4);
        let (converted, _, finished) = drive(&ok_client, mid, 64);
        assert_eq!(converted, 3);
        assert!(finished);
    }

    #[pg_test]
    fn transient_convert_error_keeps_running() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            fail_convert: Some(crate::client::PvError::Remote {
                code: crate::client::RavennaCode::Timeout,
                message: "busy".into(),
            }),
            ..Default::default()
        };
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(!finished);
        let (state, error) = (
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<String>(
                "SELECT error FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(state.as_deref(), Some("running"), "transient error retries");
        assert!(error.is_some(), "error surfaced for status()");

        // Once inference recovers, the same migration completes.
        let ok_client = MockClient::new(3).with_convert_dim(4);
        let (converted, _, finished) = drive(&ok_client, mid, 64);
        assert_eq!(converted, 3);
        assert!(finished);
    }

    /// The persisted per-migration retry backoff: a transient failure pushes
    /// `not_before` out (gating `running_ids`), repeated failures grow the
    /// delay, and a successful batch resets the counter.
    #[pg_test]
    fn transient_failure_backs_off_and_gates_running_ids() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        assert_eq!(Migration::running_ids(), vec![mid], "fresh: ready to run");

        set_migration_error(mid, "node down");
        let (failures, gated) = (
            Spi::get_one_with_args::<i32>(
                "SELECT retry_failures FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<bool>(
                "SELECT not_before > now() FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(failures, Some(1));
        assert_eq!(gated, Some(true), "not_before pushed into the future");
        assert!(
            Migration::running_ids().is_empty(),
            "a backed-off migration is not offered to the driver"
        );

        set_migration_error(mid, "still down");
        assert_eq!(
            Spi::get_one_with_args::<i32>(
                "SELECT retry_failures FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Some(2),
            "consecutive failures accumulate"
        );

        // A successful batch resets the backoff bookkeeping.
        let client = MockClient::new(3).with_convert_dim(4);
        let BatchRead::Work(batch) = read_migration_batch(mid, 64) else {
            panic!("expected a batch (read ignores the gate; only running_ids filters)");
        };
        let results = run_inference(&client, &batch, 1000).unwrap();
        let applied = apply_migration_batch(&batch, &results);
        assert!(applied.advanced);
        assert_eq!(
            Spi::get_one_with_args::<i32>(
                "SELECT retry_failures FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Some(0),
            "success resets the failure counter"
        );
    }

    /// An oversized convert batch that can never finish inside the RPC
    /// deadline is split into halves instead of retrying at the same size
    /// forever.
    #[pg_test]
    fn oversized_convert_batch_splits_on_deadline() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        // The "node" times out on any batch above 1 vector; batch size 64
        // would retry forever without the split.
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            convert_max_batch: Some(1),
            ..Default::default()
        };
        let (converted, skipped, finished) = drive(&client, mid, 64);
        assert!(finished, "the migration completes through the splits");
        assert_eq!(converted, 3);
        assert_eq!(skipped, 0);
    }

    /// A genuinely down node still surfaces as a transient error (the split
    /// is depth-limited): the migration keeps running with backoff.
    #[pg_test]
    fn convert_deadline_at_depth_limit_stays_transient() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient {
            dim: 3,
            convert_dim: Some(4),
            convert_max_batch: Some(0), // every call times out
            ..Default::default()
        };
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 0);
        assert!(!finished);
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("running"),
            "deadline stays transient (retry with backoff), never permanent"
        );
    }

    /// An abort racing a failing batch must keep its terminal state — the
    /// late fail/set_migration_error writes are guarded on state='running'.
    #[pg_test]
    fn late_failure_writes_cannot_rewrite_an_aborted_migration() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();

        fail_migration(mid, "late permanent failure");
        set_migration_error(mid, "late transient failure");
        let (state, error) = (
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<String>(
                "SELECT error FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(state.as_deref(), Some("aborted"), "audit row preserved");
        assert_eq!(error, None, "no late error overwrite");
    }

    // ---- Adopted-entry migration, observed acknowledgement, swap safety ----

    /// docs with an adopted, populated `embedding vector(3)` column on model
    /// m (fixture models: m dim 3, m2 dim 4, direct converter m→m2).
    fn docs_adopted(extra_adopt_args: &str) -> i64 {
        seed_models();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(3))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs (body, embedding) VALUES
                ('a', '[1,1,1]'), ('bb', '[2,2,2]'), ('ccc', '[3,3,3]')",
        )
        .unwrap();
        let q = format!(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
             model => 'm'{extra_adopt_args})"
        );
        Spi::get_one::<i64>(&q).unwrap().unwrap()
    }

    /// Adopt a populated column, migrate
    /// via convert, finalize — the column swaps, model/dim update, and
    /// ownership flips to true (the replacement column is postvec-built).
    #[pg_test]
    fn adopted_entry_migrates_and_sets_ownership() {
        let rid = docs_adopted("");
        assert_eq!(
            Spi::get_one::<bool>("SELECT owns_vector_column FROM postvec.registry").unwrap(),
            Some(false)
        );
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!((converted, skipped), (3, 0));
        assert!(finished);

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass AND attname = 'embedding'
                    AND NOT attisdropped"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)"),
            "the adopted column was swapped for the converted one"
        );
        let (model, dim, owns) = (
            Spi::get_one::<String>("SELECT model FROM postvec.registry").unwrap(),
            Spi::get_one::<i32>("SELECT dim FROM postvec.registry").unwrap(),
            Spi::get_one::<bool>("SELECT owns_vector_column FROM postvec.registry").unwrap(),
        );
        assert_eq!(model.as_deref(), Some("m2"));
        assert_eq!(dim, Some(4));
        assert_eq!(
            owns,
            Some(true),
            "finalize makes the replacement column postvec-owned"
        );
        let _ = rid;
    }

    /// Observed migration: the default call refuses before `_new`
    /// exists; the acknowledged call proceeds; after finalize the entry is
    /// owned but still observed, and a second adopt() promotes it in place.
    #[pg_test]
    fn observed_migrate_requires_quiescence_acknowledgement() {
        let rid = docs_adopted(", sync => false, backfill => 'none'");
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").ok();
        });
        assert!(
            r.is_err(),
            "an observed entry must require the acknowledgement"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'embedding_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "the refusal happened before the _new column was created"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.migrations").unwrap(),
            Some(0),
            "no migration row either"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );

        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m2', observed_writes_quiesced => true)",
        )
        .unwrap()
        .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 3);
        assert!(finished);
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();

        let (owns, mode) = (
            Spi::get_one::<bool>("SELECT owns_vector_column FROM postvec.registry").unwrap(),
            Spi::get_one::<String>("SELECT trigger_mode FROM postvec.registry").unwrap(),
        );
        assert_eq!(owns, Some(true), "finalize flips ownership");
        assert_eq!(
            mode.as_deref(),
            Some("none"),
            "finalize does not turn triggers on"
        );

        // Promotion keys on trigger_mode='none', not ownership: the column is
        // now on the embeddable model m2 — turn sync on.
        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm2')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(id2, rid, "promotion preserves the registry id");
        assert_eq!(
            Spi::get_one::<bool>("SELECT owns_vector_column FROM postvec.registry").unwrap(),
            Some(true),
            "promotion preserves ownership"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT trigger_mode FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("statement")
        );
    }

    /// The swap-hazard helper classifies catalog objects into the three
    /// buckets, with schema-qualified stable names, and a sibling
    /// `<column>_new` dependency is never a false positive.
    #[pg_test]
    fn column_swap_hazards_classify_catalog_objects() {
        use crate::api::migrate::inspect_column_swap;
        Spi::run(
            "CREATE TABLE hz (id bigint PRIMARY KEY,
                              emb vector(3) NOT NULL DEFAULT '[0,0,0]',
                              emb_new vector(3), body text)",
        )
        .unwrap();
        Spi::run("ALTER TABLE hz ADD CONSTRAINT emb_dims CHECK (vector_dims(emb) = 3)").unwrap();
        Spi::run("COMMENT ON COLUMN hz.emb IS 'the adopted embedding'").unwrap();
        Spi::run("ALTER TABLE hz ALTER COLUMN emb SET STATISTICS 500").unwrap();
        Spi::run("GRANT SELECT (emb) ON hz TO PUBLIC").unwrap();
        Spi::run("CREATE STATISTICS hz_ext (ndistinct) ON id, emb FROM hz").unwrap();
        Spi::run("CREATE INDEX hz_emb_hnsw ON hz USING hnsw (emb vector_cosine_ops)").unwrap();
        Spi::run("CREATE INDEX hz_emb_expr ON hz ((vector_dims(emb)))").unwrap();
        Spi::run("CREATE VIEW \"Weird View\" AS SELECT id, emb FROM hz").unwrap();
        Spi::run("CREATE MATERIALIZED VIEW hz_mv AS SELECT id, emb FROM hz").unwrap();
        // Sibling objects on emb_new must not appear anywhere.
        Spi::run("CREATE INDEX hz_new_idx ON hz USING hnsw (emb_new vector_cosine_ops)").unwrap();
        Spi::run("CREATE VIEW hz_new_v AS SELECT emb_new FROM hz").unwrap();

        let oid = Spi::get_one::<pgrx::pg_sys::Oid>("SELECT 'hz'::regclass::oid")
            .unwrap()
            .unwrap();
        let hz = inspect_column_swap(oid, "emb");

        let has = |v: &[String], needle: &str| v.iter().any(|s| s.contains(needle));
        assert!(
            has(&hz.lossy_metadata, "NOT NULL"),
            "{:?}",
            hz.lossy_metadata
        );
        assert!(
            has(&hz.lossy_metadata, "default"),
            "{:?}",
            hz.lossy_metadata
        );
        assert!(
            has(&hz.lossy_metadata, "emb_dims"),
            "{:?}",
            hz.lossy_metadata
        );
        assert!(
            has(&hz.lossy_metadata, "comment"),
            "{:?}",
            hz.lossy_metadata
        );
        assert!(
            has(&hz.lossy_metadata, "statistics target"),
            "{:?}",
            hz.lossy_metadata
        );
        assert!(has(&hz.lossy_metadata, "ACL"), "{:?}", hz.lossy_metadata);
        assert!(has(&hz.lossy_metadata, "hz_ext"), "{:?}", hz.lossy_metadata);
        assert!(
            has(&hz.dependent_indexes, "hz_emb_hnsw") && has(&hz.dependent_indexes, "hz_emb_expr"),
            "{:?}",
            hz.dependent_indexes
        );
        assert!(
            has(&hz.blocking_dependents, "\"Weird View\"") && has(&hz.blocking_dependents, "hz_mv"),
            "{:?}",
            hz.blocking_dependents
        );
        for bucket in [
            &hz.lossy_metadata,
            &hz.blocking_dependents,
            &hz.dependent_indexes,
        ] {
            assert!(
                !has(bucket, "hz_new"),
                "sibling emb_new objects must not appear: {bucket:?}"
            );
        }

        // The sibling column's own inspection sees only its objects.
        let hz_new = inspect_column_swap(oid, "emb_new");
        assert!(has(&hz_new.dependent_indexes, "hz_new_idx"));
        assert!(has(&hz_new.blocking_dependents, "hz_new_v"));
        assert!(
            !has(&hz_new.lossy_metadata, "NOT NULL"),
            "{:?}",
            hz_new.lossy_metadata
        );
    }

    /// DROP COLUMN on a partitioned parent recurses into every partition, so
    /// child-only column metadata/dependents are swap hazards too: the
    /// inspection walks pg_partition_tree, names the partition, gates
    /// migrate() and the locked finalize recheck, and a clean tree cuts over.
    #[pg_test]
    fn partitioned_child_metadata_gates_migration() {
        use crate::api::migrate::inspect_column_swap;
        seed_models();
        Spi::run(
            "CREATE TABLE pt (id bigint NOT NULL, body text, embedding vector(3),
                              PRIMARY KEY (id)) PARTITION BY RANGE (id)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (100)").unwrap();
        Spi::run("CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (100) TO (200)").unwrap();
        Spi::run(
            "INSERT INTO pt (id, body, embedding) VALUES
                (1, 'a', '[1,1,1]'), (150, 'b', '[2,2,2]')",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('pt','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap();

        // Child-only metadata and a child-only dependent are both visible,
        // named by partition.
        Spi::run("COMMENT ON COLUMN pt1.embedding IS 'child metadata'").unwrap();
        Spi::run("CREATE VIEW pt1_v AS SELECT embedding FROM pt1").unwrap();
        let oid = Spi::get_one::<pgrx::pg_sys::Oid>("SELECT 'pt'::regclass::oid")
            .unwrap()
            .unwrap();
        let hz = inspect_column_swap(oid, "embedding");
        assert!(
            hz.lossy_metadata
                .iter()
                .any(|s| s.contains("on partition") && s.contains("pt1") && s.contains("comment")),
            "child comment is a named lossy hazard: {:?}",
            hz.lossy_metadata
        );
        assert!(
            hz.blocking_dependents
                .iter()
                .any(|s| s.contains("on partition") && s.contains("pt1_v")),
            "child view is a named blocking dependent: {:?}",
            hz.blocking_dependents
        );

        Spi::run("DROP VIEW pt1_v").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.migrate('pt','body','m2')").ok();
        });
        assert!(
            r.is_err(),
            "child-only lossy metadata must refuse migrate()"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'pt'::regclass
                    AND attname = 'embedding_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "refused before any DDL"
        );

        Spi::run("COMMENT ON COLUMN pt1.embedding IS NULL").unwrap();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('pt','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, _, finished) = drive(&client, mid, 64);
        assert_eq!(converted, 2);
        assert!(finished);

        // Metadata added to the *other* child mid-migration gates the locked
        // finalize recheck, then a clean retry cuts the whole tree over.
        Spi::run("COMMENT ON COLUMN pt2.embedding IS 'late child metadata'").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).ok();
        });
        assert!(r.is_err(), "late child metadata must gate finalize");
        Spi::run("COMMENT ON COLUMN pt2.embedding IS NULL").unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        for rel in ["pt", "pt1", "pt2"] {
            assert_eq!(
                Spi::get_one_with_args::<String>(
                    "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                      WHERE attrelid = to_regclass($1) AND attname = 'embedding'
                        AND NOT attisdropped",
                    &[rel.into()],
                )
                .unwrap()
                .as_deref(),
                Some("vector(4)"),
                "{rel} swapped to the new dimension"
            );
        }
    }

    /// Lossy column metadata refuses migrate() before any DDL or state
    /// mutation; removing it lets the same call succeed.
    #[pg_test]
    fn migrate_refuses_lossy_column_metadata_before_ddl() {
        docs_adopted("");
        Spi::run("ALTER TABLE docs ALTER COLUMN embedding SET DEFAULT '[0,0,0]'").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").ok();
        });
        assert!(r.is_err(), "a column default must refuse migrate()");
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'embedding_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "refused before the _new column"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.migrations").unwrap(),
            Some(0),
            "refused before the migration row"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active"),
            "refused before the registry state change"
        );

        Spi::run("ALTER TABLE docs ALTER COLUMN embedding DROP DEFAULT").unwrap();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        assert!(mid.is_some(), "clean column migrates");
    }

    /// A dependent view is allowed while the conversion runs, blocks finalize
    /// with its exact name, survives the refused call, and finalize succeeds
    /// on retry after the view is dropped.
    #[pg_test]
    fn dependent_view_is_an_early_warning_and_a_finalize_gate() {
        docs_adopted("");
        Spi::run("CREATE VIEW docs_v AS SELECT id, embedding FROM docs").unwrap();

        // migrate() proceeds (the view is a warning, not a gate, here).
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished, "conversion runs to completion under the view");

        let r = std::panic::catch_unwind(|| {
            Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).ok();
        });
        assert!(r.is_err(), "the view must gate finalize");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_finalize"),
            "the migration remains retryable"
        );
        for col in ["embedding", "embedding_new"] {
            assert_eq!(
                Spi::get_one_with_args::<i64>(
                    "SELECT count(*) FROM pg_attribute
                      WHERE attrelid = 'docs'::regclass AND attname = $1 AND NOT attisdropped",
                    &[col.into()],
                )
                .unwrap(),
                Some(1),
                "{col} intact after the refused finalize"
            );
        }
        assert!(
            Spi::get_one::<bool>("SELECT to_regclass('docs_v') IS NOT NULL")
                .unwrap()
                .unwrap(),
            "the view was not cascaded away"
        );

        Spi::run("DROP VIEW docs_v").unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done"),
            "retry after remediation succeeds"
        );
    }

    /// Metadata/dependents added *after* migrate() started are caught by the
    /// locked finalize recheck.
    #[pg_test]
    fn finalize_rechecks_metadata_added_after_migrate() {
        docs_adopted("");
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished);

        Spi::run("COMMENT ON COLUMN docs.embedding IS 'added mid-migration'").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).ok();
        });
        assert!(r.is_err(), "late metadata must gate finalize");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_finalize")
        );

        Spi::run("COMMENT ON COLUMN docs.embedding IS NULL").unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done")
        );
    }

    /// Multiple blockers: nothing is cascaded, every blocker survives a
    /// refused finalize, and the helper reports all of them.
    #[pg_test]
    fn finalize_never_cascades_and_reports_all_blockers() {
        use crate::api::migrate::inspect_column_swap;
        docs_adopted("");
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished);

        Spi::run("CREATE VIEW blocker_one AS SELECT embedding FROM docs").unwrap();
        Spi::run("CREATE VIEW blocker_two AS SELECT id, embedding FROM docs").unwrap();

        let oid = Spi::get_one::<pgrx::pg_sys::Oid>("SELECT 'docs'::regclass::oid")
            .unwrap()
            .unwrap();
        let hz = inspect_column_swap(oid, "embedding");
        assert!(
            hz.blocking_dependents
                .iter()
                .any(|s| s.contains("blocker_one"))
                && hz
                    .blocking_dependents
                    .iter()
                    .any(|s| s.contains("blocker_two")),
            "every blocker is reported: {:?}",
            hz.blocking_dependents
        );

        let r = std::panic::catch_unwind(|| {
            Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).ok();
        });
        assert!(r.is_err());
        for v in ["blocker_one", "blocker_two"] {
            assert!(
                Spi::get_one_with_args::<bool>("SELECT to_regclass($1) IS NOT NULL", &[v.into()])
                    .unwrap()
                    .unwrap(),
                "{v} survives — nothing is cascaded"
            );
        }
    }

    /// Application indexes on the old column die only at a *successful*
    /// cutover, and the existing reindex semantics (awaiting_index + the
    /// suggested SQL for the postvec ANN index) are unchanged.
    #[pg_test]
    fn finalize_documents_index_loss() {
        docs_adopted("");
        Spi::run("CREATE INDEX user_emb_idx ON docs USING hnsw (embedding vector_cosine_ops)")
            .unwrap();
        Spi::run("CREATE INDEX user_expr_idx ON docs ((vector_dims(embedding)))").unwrap();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished);
        // Indexes are intact right up to the cutover.
        for idx in ["user_emb_idx", "user_expr_idx"] {
            assert!(
                Spi::get_one_with_args::<bool>("SELECT to_regclass($1) IS NOT NULL", &[idx.into()])
                    .unwrap()
                    .unwrap(),
                "{idx} intact before finalize"
            );
        }

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        for idx in ["user_emb_idx", "user_expr_idx"] {
            assert!(
                !Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass($1) IS NOT NULL",
                    &[idx.into()]
                )
                .unwrap()
                .unwrap(),
                "{idx} is gone only after the successful cutover"
            );
        }
        // The old column carried an ANN index, so 'manual' reindex semantics
        // park the migration awaiting_index with the exact suggestion.
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_index"),
            "reindex semantics unchanged"
        );
        let suggested = Spi::get_one::<String>(&format!(
            "SELECT suggested_index_sql FROM postvec.migration_status({mid})"
        ))
        .unwrap()
        .unwrap();
        Spi::run(&suggested.replace("CONCURRENTLY ", "")).unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done")
        );
    }

    /// awaiting_index completion must never claim an externally-built index:
    /// with a valid custom ANN index (which satisfies readiness) *and* a
    /// conventional-name B-tree expression index on the same column, the
    /// migration completes and the conventional-name object gains no
    /// extension dependency — only indexes postvec itself creates are ever
    /// stamped, so nothing here can become DROP EXTENSION collateral.
    #[pg_test]
    fn awaiting_index_never_claims_external_indexes() {
        let rid = docs_adopted("");
        Spi::run("CREATE INDEX old_ann ON docs USING hnsw (embedding vector_cosine_ops)").unwrap();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished);
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_index"),
            "the old column carried an ANN index"
        );

        // The operator builds their own correctly-opclassed ANN index, plus
        // an unrelated B-tree expression index that happens to reuse the
        // conventional name.
        Spi::run("CREATE INDEX custom_ann ON docs USING hnsw (embedding vector_cosine_ops)")
            .unwrap();
        Spi::run(&format!(
            "CREATE INDEX postvec_vec_{rid} ON docs ((vector_dims(embedding)))"
        ))
        .unwrap();

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done"),
            "the valid custom ANN index satisfies readiness"
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_depend d
                   JOIN pg_extension e ON e.oid = d.refobjid
                  WHERE d.classid = 'pg_class'::regclass
                    AND d.objid = 'postvec_vec_{rid}'::regclass
                    AND d.refclassid = 'pg_extension'::regclass
                    AND d.deptype = 'x' AND e.extname = 'postvec'"
            ))
            .unwrap(),
            Some(0),
            "the conventional-name B-tree index is never stamped"
        );
    }

    /// awaiting_index readiness is opclass-aware: an ANN index whose opclass
    /// does not match the entry's distance would never serve search(), so it
    /// must not mark the migration done.
    #[pg_test]
    fn awaiting_index_requires_expected_opclass() {
        docs_adopted("");
        Spi::run("CREATE INDEX old_ann ON docs USING hnsw (embedding vector_cosine_ops)").unwrap();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (_, _, finished) = drive(&client, mid, 64);
        assert!(finished);
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();

        // A wrong-opclass ANN index (l2 on a cosine entry) is not readiness.
        Spi::run("CREATE INDEX wrong_ops ON docs USING hnsw (embedding vector_l2_ops)").unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_index"),
            "a wrong-opclass index must not complete the migration"
        );

        Spi::run("CREATE INDEX right_ops ON docs USING hnsw (embedding vector_cosine_ops)")
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done")
        );
    }

    /// Composite-PK migration: watermark iteration over ROW(...)::text keys.
    #[pg_test]
    fn composite_pk_migration_converts_all_rows() {
        seed_models();
        Spi::run("CREATE TABLE ck (a int, b text, body text, PRIMARY KEY (a, b))").unwrap();
        Spi::run(
            "INSERT INTO ck VALUES (1,'x','one'), (1,'y','two'), (2,'x','three'),
                                   (2,'comma,paren)','four')",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('ck','body','m', backfill => false)").unwrap();
        Spi::run("UPDATE ck SET body_semantic = ('[' || a || ',0,0]')::vector").unwrap();

        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('ck','body','m2')")
            .unwrap()
            .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        // Batch of 1 exercises the composite watermark clause three times.
        let (converted, skipped, finished) = drive(&client, mid, 1);
        assert_eq!(converted, 4, "all rows, incl. the quoting-hostile key");
        assert_eq!(skipped, 0);
        assert!(finished);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM ck WHERE body_semantic_new IS NOT NULL")
                .unwrap(),
            Some(4)
        );
    }

    // ---- Chunked migration ----

    /// Chunked entry with several embedded chunks over a few documents.
    /// Returns (registry id, chunk count).
    fn chunked_enabled(format: Option<&str>) -> (i64, i64) {
        seed_models();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text)",
        )
        .unwrap();
        let id = Spi::get_one_with_args::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks',
                                   chunk_size => 64, chunk_overlap => 0,
                                   format => $1)",
            &[format.into()],
        )
        .unwrap()
        .unwrap();
        Spi::run(
            "INSERT INTO docs (title, body) VALUES
                 ('t1', repeat('alpha ', 30)), ('t2', repeat('beta ', 25))",
        )
        .unwrap();
        while crate::worker::chunk::process_one_refresh(5).processed {}
        // Embed the children with the mock so old vectors exist.
        let client = MockClient::new(3);
        loop {
            let groups = crate::jobs::claim_and_read(64, 300.0);
            if groups.is_empty() {
                break;
            }
            for g in groups {
                let routing = g.routing.clone();
                let (nj, ej) = crate::jobs::split_items(g.items);
                let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
                let outcomes = crate::jobs::embed_with_bisection(
                    &client,
                    "m",
                    &Default::default(),
                    1000,
                    &texts,
                );
                crate::jobs::apply_group(g.entry.id, &routing, &nj, &ej, &outcomes, 5, 5000);
            }
        }
        let n =
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks WHERE body_semantic IS NOT NULL")
                .unwrap()
                .unwrap();
        assert!(n > 2, "several embedded chunks: {n}");
        (id, n)
    }

    /// Convert migration over chunk rows: the scratch column lives on the
    /// destination, progress counts chunks, no chunk text is read, a fresh
    /// document written mid-migration routes its children to the scratch
    /// column with the new model, and finalize swaps the destination column.
    #[pg_test]
    fn chunked_convert_migration_end_to_end() {
        let (id, n) = chunked_enabled(None);
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs_chunks'::regclass
                    AND attname = 'body_semantic_new'"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)"),
            "the scratch column is on the destination"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT rows_total FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Some(n),
            "rows_total counts chunks"
        );

        // A fresh document lands mid-migration: its children route to the
        // NEW model and the scratch column directly.
        let client = MockClient::new(3).with_convert_dim(4);
        Spi::run("INSERT INTO docs (title, body) VALUES ('t3', 'fresh mid-migration doc')")
            .unwrap();
        while crate::worker::chunk::process_one_refresh(5).processed {}
        let group = crate::jobs::claim_and_read(64, 300.0)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(group.routing.model, "m2");
        assert_eq!(group.routing.vector_column, "body_semantic_new");
        let (nj, ej) = crate::jobs::split_items(group.items);
        let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
        let fresh_client = MockClient::new(4);
        let outcomes = crate::jobs::embed_with_bisection(
            &fresh_client,
            "m2",
            &Default::default(),
            1000,
            &texts,
        );
        let applied =
            crate::jobs::apply_group(group.entry.id, &group.routing, &nj, &ej, &outcomes, 5, 5000);
        assert_eq!(applied.done, 1, "the fresh chunk embedded into the scratch");

        // Drive the conversion; monotonic chunk ids beyond the watermark are
        // not skipped, and the fresh chunk is guarded away (already filled).
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!(converted, n, "every old chunk vector converted");
        assert_eq!(skipped, 0);
        assert!(finished);

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs_chunks'::regclass AND attname = 'body_semantic'
                    AND NOT attisdropped"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)"),
            "the destination column swapped"
        );
        assert_eq!(
            Spi::get_one_with_args::<bool>(
                "SELECT model = 'm2' AND dim = 4 AND state = 'active'
                   FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(true)
        );
        // [R2-3] payoff: the lean view survived finalize untouched.
        assert!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks_view")
                .unwrap()
                .unwrap()
                > 0,
            "the generated view is intact after the swap"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs_chunks
                  WHERE body_semantic IS NOT NULL AND vector_dims(body_semantic) = 4"
            )
            .unwrap()
            .unwrap(),
            n + 1,
            "old conversions + the fresh mid-migration chunk"
        );
    }

    /// Reembed migration renders each chunk through the current template with
    /// live source context and embeds with the new model.
    #[pg_test]
    fn chunked_reembed_migration_renders_chunk_template() {
        let (_id, n) = chunked_enabled(Some("[$title] $chunk"));
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m2', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        let client = MockClient::new(4);
        let (converted, skipped, finished) = drive(&client, mid, 2);
        assert_eq!(converted, n, "every chunk re-embedded");
        assert_eq!(skipped, 0);
        assert!(finished);
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs_chunks
                  WHERE body_semantic IS NOT NULL AND vector_dims(body_semantic) = 4"
            )
            .unwrap()
            .unwrap(),
            n
        );
    }

    /// `reindex => 'blocking'` on a chunked entry rebuilds the ANN index
    /// on the destination. The source has no vector column at all, so
    /// building on the source would fail the whole finalize transaction.
    #[pg_test]
    fn chunked_blocking_reindex_builds_on_destination() {
        let (id, n) = chunked_enabled(None);
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m2', reindex => 'blocking')",
        )
        .unwrap()
        .unwrap();
        let client = MockClient::new(3).with_convert_dim(4);
        let (converted, _skipped, finished) = drive(&client, mid, 2);
        assert_eq!(converted, n);
        assert!(finished);
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("done"),
            "blocking reindex finishes the migration outright"
        );
        let def = Spi::get_one::<String>(&format!(
            "SELECT pg_get_indexdef(('postvec_vec_{id}')::regclass)"
        ))
        .unwrap()
        .unwrap_or_default();
        assert!(
            def.contains("docs_chunks") && def.contains("body_semantic") && def.contains("hnsw"),
            "the rebuilt ANN index lives on the destination: {def}"
        );
    }

    /// Abort drops only the proven destination scratch column.
    #[pg_test]
    fn chunked_migration_abort_drops_scratch_only() {
        let (_id, n) = chunked_enabled(None);
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs_chunks'::regclass
                    AND attname = 'body_semantic_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "the scratch column is gone"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks WHERE body_semantic IS NOT NULL")
                .unwrap()
                .unwrap(),
            n,
            "old chunk vectors untouched"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );
    }
}
