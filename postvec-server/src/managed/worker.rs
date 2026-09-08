// SPDX-License-Identifier: BUSL-1.1

use super::{inference::Client, ManagedDb};
use anyhow::{ensure, Context, Result};
use postvec_core::{
    client::{EmbedRoute, ErrorClass, PvError, RavennaCode},
    registry::{
        chunk_format_expr, chunk_format_len_expr, format_expr, format_len_expr, quote_ident,
        serialize_vector, RegistryEntry,
    },
};
use sqlx::{Connection, Executor, PgConnection, Row};

pub(super) const ITEM_CAP: i64 = 1024 * 1024;
const BATCH_CAP: usize = 8 * 1024 * 1024;

/// A registry entry whose source, destination or vector column no longer
/// matches what was registered. The entry is disabled instead of retried.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(super) struct Quarantine(pub String);

pub(super) async fn guard(conn: &mut PgConnection) -> Result<()> {
    conn.execute("SELECT pg_advisory_xact_lock_shared(1886615158,1); SET LOCAL search_path=pg_catalog; SET LOCAL row_security=off; SET LOCAL standard_conforming_strings=on").await?;
    let version: Option<(i32, String)> =
        sqlx::query_as("SELECT version, mode FROM postvec.schema_version")
            .fetch_optional(conn)
            .await?;
    ensure!(
        version == Some((super::install::VERSION, "managed".into())),
        "managed schema is missing or has an unsupported version"
    );
    Ok(())
}

pub(super) async fn entry(
    conn: &mut PgConnection,
    id: i64,
    lock: bool,
) -> Result<Option<RegistryEntry>> {
    loop {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT to_jsonb(r)::text FROM postvec.registry r WHERE id=$1 AND state<>'disabled'",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let e: RegistryEntry = serde_json::from_str(&raw)?;
        ensure!(
            !e.pk_columns.is_empty() && e.pk_columns.len() == e.pk_types.len(),
            Quarantine("invalid primary key metadata".into())
        );
        let mut columns = e
            .referenced_columns()
            .map_err(|e| Quarantine(e.to_string()))?;
        if !lock {
            return Ok(Some(e));
        }
        conn.execute(format!("LOCK TABLE {} IN ROW SHARE MODE", e.qualified_table()).as_str())
            .await?;
        let sentinel: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_trigger WHERE tgrelid=to_regclass($1) AND tgname=$2 AND tgfoid='postvec.trg_truncate()'::regprocedure)")
            .bind(e.qualified_table()).bind(format!("postvec_trunc_{}",e.id)).fetch_one(&mut *conn).await?;
        ensure!(
            sentinel,
            Quarantine("source relation identity changed".into())
        );
        if e.is_recursive() {
            conn.execute(
                format!(
                    "LOCK TABLE {} IN ROW EXCLUSIVE MODE",
                    e.qualified_vector_table()
                )
                .as_str(),
            )
            .await?;
            let marker: Option<String> =
                sqlx::query_scalar("SELECT obj_description(to_regclass($1),'pg_class')")
                    .bind(e.qualified_vector_table())
                    .fetch_one(&mut *conn)
                    .await?;
            ensure!(
                marker == e.destination_comments().map(|c| c.0),
                Quarantine("chunk destination ownership changed".into())
            );
        }
        columns.extend(e.pk_columns.iter().cloned());
        columns.push(e.source_column.clone());
        conn.execute(
            format!(
                "SELECT {} FROM {} LIMIT 0",
                columns
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(","),
                e.qualified_table()
            )
            .as_str(),
        )
        .await?;
        let shape: bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_attribute a JOIN pg_type t ON t.oid=a.atttypid JOIN pg_extension x ON x.extnamespace=t.typnamespace AND x.extname='vector' WHERE a.attrelid=to_regclass($1) AND a.attname=$2 AND a.atttypmod=$3 AND t.typname='vector' AND NOT a.attisdropped)").bind(e.qualified_vector_table()).bind(&e.vector_column).bind(e.dim).fetch_one(&mut *conn).await?;
        ensure!(shape, Quarantine("vector column identity changed".into()));
        let current: Option<String> = sqlx::query_scalar("SELECT to_jsonb(r)::text FROM postvec.registry r WHERE id=$1 AND state<>'disabled' FOR UPDATE") .bind(id).fetch_optional(&mut *conn).await?;
        match current {
            None => return Ok(None),
            Some(current) if current == raw => return Ok(Some(e)),
            Some(_) => continue,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Routing {
    pub model: String,
    pub space: Option<String>,
    pub column: String,
    pub dim: i32,
    pub migration: Option<i64>,
}
pub(super) async fn routing(conn: &mut PgConnection, e: &RegistryEntry) -> Result<Routing> {
    let m: Option<(i64,String,String,i32,Option<String>)> = sqlx::query_as("SELECT id,new_model,new_column,new_dim,resolved_via->>'space' FROM postvec.migrations WHERE registry_id=$1 AND state IN ('running','awaiting_finalize') ORDER BY id DESC LIMIT 1") .bind(e.id).fetch_optional(conn).await?;
    Ok(match m {
        Some((id, model, column, dim, space)) => Routing {
            model,
            space,
            column,
            dim,
            migration: Some(id),
        },
        None => Routing {
            model: e.model.clone(),
            space: e.space.clone(),
            column: e.vector_column.clone(),
            dim: e.dim,
            migration: None,
        },
    })
}

/// Index-friendly match of one source row by its text-rendered key.
pub(super) fn pk_pred(e: &RegistryEntry, alias: &str, param: &str) -> String {
    if e.is_composite_pk() {
        format!("{}={param}", e.pk_text_expr(alias))
    } else {
        let prefix = if alias.is_empty() { "" } else { "." };
        format!(
            "{alias}{prefix}{}=(CASE WHEN pg_input_is_valid({param},{}) THEN {param} END)::{}",
            quote_ident(&e.pk_columns[0]),
            postvec_core::registry::quote_literal_estring(&e.pk_types[0]),
            e.pk_types[0]
        )
    }
}

/// FROM clause, text key, document expression and its length, plus the
/// predicate matching one job's key.
pub(super) fn source(e: &RegistryEntry) -> (String, String, String, String, String) {
    if e.is_recursive() {
        (
            format!(
                "{} c JOIN {} s ON {}",
                e.qualified_vector_table(),
                e.qualified_table(),
                e.source_pk_join("s", "c")
            ),
            "c.postvec_chunk_id::text".into(),
            chunk_format_expr(e, "s", "c"),
            chunk_format_len_expr(e, "s", "c"),
            "c.postvec_chunk_id=$1::bigint".into(),
        )
    } else {
        (
            format!("{} s", e.qualified_table()),
            e.pk_text_expr("s"),
            format_expr(e, "s"),
            format_len_expr(e, "s"),
            pk_pred(e, "s", "$1"),
        )
    }
}

pub(super) async fn vector_type(conn: &mut PgConnection) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT quote_ident(n.nspname)||'.vector' FROM pg_extension x JOIN pg_namespace n ON n.oid=x.extnamespace WHERE x.extname='vector'").fetch_one(conn).await?)
}

struct Job {
    id: i64,
    pk: String,
    chunk: Option<i64>,
    text: Option<String>,
    version: Option<String>,
    oversized: bool,
}

pub(super) async fn step(conn: &mut PgConnection, client: &Client, db: &ManagedDb) -> Result<bool> {
    let mut tx = conn.begin().await?;
    guard(&mut tx).await?;
    let vis = client.visibility_secs();
    sqlx::query("WITH d AS (DELETE FROM postvec.jobs WHERE claimed_at<now()-make_interval(secs=>$1) AND attempts>=5 RETURNING *) INSERT INTO postvec.jobs_dead(job_id,registry_id,pk_value,op,chunk_id,attempts,last_error,created_at) SELECT id,registry_id,pk_value,op,chunk_id,attempts,'worker repeatedly lost during inference',created_at FROM d").bind(vis).execute(&mut *tx).await?;
    sqlx::query("WITH old AS (DELETE FROM postvec.jobs WHERE claimed_at<now()-make_interval(secs=>$1) RETURNING *) INSERT INTO postvec.jobs(registry_id,pk_value,op,chunk_id,attempts,last_error) SELECT registry_id,pk_value,op,chunk_id,attempts,'reclaimed after worker loss' FROM old ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING").bind(vis).execute(&mut *tx).await?;
    let id: Option<i64> = sqlx::query_scalar("SELECT j.registry_id FROM postvec.jobs j JOIN postvec.registry r ON r.id=j.registry_id WHERE j.op='embed' AND j.claimed_at IS NULL AND j.not_before<=now() AND r.state<>'disabled' ORDER BY j.not_before,j.id LIMIT 1") .fetch_optional(&mut *tx).await?;
    let Some(id) = id else {
        tx.commit().await?;
        return Ok(false);
    };
    let e = match entry(&mut tx, id, true).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            tx.commit().await?;
            return Ok(false);
        }
        Err(error) => {
            tx.rollback().await?;
            return quarantine_or_error(conn, id, error).await;
        }
    };
    let expected = routing(&mut tx, &e).await?;
    let rows = sqlx::query("UPDATE postvec.jobs SET claimed_at=clock_timestamp(),attempts=attempts+1 WHERE id IN (SELECT id FROM postvec.jobs WHERE registry_id=$1 AND op='embed' AND claimed_at IS NULL AND not_before<=now() ORDER BY not_before,id LIMIT $2 FOR UPDATE SKIP LOCKED) RETURNING id,pk_value,chunk_id") .bind(id).bind(db.batch_size).fetch_all(&mut *tx).await?;
    let (from, _, expr, len, pred) = source(&e);
    let version = if e.is_recursive() {
        "c.xmin::text || ':' || s.xmin::text"
    } else {
        "s.xmin::text"
    };
    let read = format!("SELECT CASE WHEN {len}<=$2 THEN {expr} END AS text,({len})::bigint AS len,{version} AS version FROM {from} WHERE {pred}");
    let mut jobs = Vec::new();
    let mut bytes = 0;
    for row in rows {
        let jid: i64 = row.get("id");
        let pk: String = row.get("pk_value");
        let chunk: Option<i64> = row.get("chunk_id");
        if e.is_recursive() != chunk.is_some() {
            finish(&mut tx, jid, Some("malformed embed job"), true).await?;
            continue;
        }
        let k = chunk.map(|n| n.to_string()).unwrap_or_else(|| pk.clone());
        let read = sqlx::query(&read)
            .bind(&k)
            .bind(ITEM_CAP)
            .fetch_optional(&mut *tx)
            .await?;
        let (text, version, oversized) = match read {
            Some(r) => (
                r.get::<Option<String>, _>("text"),
                Some(r.get::<String, _>("version")),
                r.get::<Option<i64>, _>("len").is_some_and(|n| n > ITEM_CAP),
            ),
            None => (None, None, false),
        };
        bytes += text.as_ref().map_or(0, String::len);
        if bytes > BATCH_CAP {
            release(&mut tx, jid, "batch byte budget", false).await?;
            continue;
        }
        jobs.push(Job {
            id: jid,
            pk,
            chunk,
            text,
            version,
            oversized,
        });
    }
    tx.commit().await?;
    let texts: Vec<_> = jobs.iter().filter_map(|j| j.text.clone()).collect();
    let space = expected.space.as_deref();
    let outcomes = infer(client, &texts, None, &expected.model, space, expected.dim).await;
    let mut tx = conn.begin().await?;
    guard(&mut tx).await?;
    let fresh = match entry(&mut tx, id, true).await {
        Ok(Some(e)) => e,
        Ok(None) => {
            tx.commit().await?;
            return Ok(true);
        }
        Err(error) => {
            tx.rollback().await?;
            return quarantine_or_error(conn, id, error).await;
        }
    };
    let route = routing(&mut tx, &fresh).await?;
    let changed = route != expected
        || fresh.format != e.format
        || fresh.qualified_vector_table() != e.qualified_vector_table();
    let vector_type = vector_type(&mut tx).await?;
    let (table, key, version_pred) = if e.is_recursive() {
        (
            e.qualified_vector_table(),
            "postvec_chunk_id=$2::bigint".to_string(),
            format!(
                "xmin::text || ':' || (SELECT s.xmin::text FROM {} s WHERE {})",
                e.qualified_table(),
                pk_pred(&e, "s", "$3")
            ),
        )
    } else {
        (
            e.qualified_table(),
            pk_pred(&e, "", "$2"),
            "xmin::text".into(),
        )
    };
    let write = |old_null: bool| {
        format!(
            "UPDATE {table} SET {}=$1::{vector_type}{} WHERE {key} AND {version_pred}=${}",
            quote_ident(&route.column),
            if old_null {
                format!(",{}=NULL", quote_ident(&e.vector_column))
            } else {
                String::new()
            },
            if e.is_recursive() { 4 } else { 3 }
        )
    };
    let mut outcomes = outcomes.into_iter();
    let (mut embedded, mut nulled) = (0i64, 0i64);
    for job in jobs {
        let result = if job.text.is_some() {
            Some(outcomes.next().context("missing inference outcome")?)
        } else {
            None
        };
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM postvec.jobs WHERE id=$1 AND claimed_at IS NOT NULL)",
        )
        .bind(job.id)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            continue;
        }
        if changed {
            release(&mut tx, job.id, "routing changed", false).await?;
            continue;
        }
        if job.oversized {
            finish(
                &mut tx,
                job.id,
                Some("source exceeds 1 MiB embedding limit"),
                true,
            )
            .await?;
            continue;
        }
        let output = match result {
            Some(Outcome::Vector(v)) => Some(serialize_vector(&v)),
            Some(Outcome::Dead(error) | Outcome::Failed(error)) => {
                finish(&mut tx, job.id, Some(&error), true).await?;
                continue;
            }
            Some(Outcome::Retry(error)) => {
                release(&mut tx, job.id, &error, true).await?;
                continue;
            }
            None => None,
        };
        if let Some(version) = job.version {
            let k = job
                .chunk
                .map(|n| n.to_string())
                .unwrap_or_else(|| job.pk.clone());
            let sql = write(output.is_none() && route.column != e.vector_column);
            let mut query = sqlx::query(&sql).bind(&output).bind(k);
            if e.is_recursive() {
                query = query.bind(&job.pk);
            }
            if query.bind(version).execute(&mut *tx).await?.rows_affected() == 0 {
                release(&mut tx, job.id, "source changed", false).await?;
                continue;
            }
        }
        finish(&mut tx, job.id, None, false).await?;
        if output.is_some() {
            embedded += 1;
        } else {
            nulled += 1;
        }
    }
    sqlx::query("UPDATE postvec.worker_heartbeat SET jobs_embedded=jobs_embedded+$1,jobs_nulled=jobs_nulled+$2,jobs_done=COALESCE(jobs_done,0)+$1+$2 WHERE id=1").bind(embedded).bind(nulled).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub(super) enum Outcome {
    Vector(Vec<f32>),
    Retry(String),
    Dead(String),
    Failed(String),
}

pub(super) async fn infer(
    client: &Client,
    texts: &[String],
    vectors: Option<&[Vec<f32>]>,
    model: &str,
    space: Option<&str>,
    dim: i32,
) -> Vec<Outcome> {
    let n = vectors.map_or(texts.len(), |v| v.len());
    let route = if vectors.is_some() {
        Ok((model.into(), EmbedRoute::default()))
    } else {
        client.route(model, space, postvec_core::client::EmbedPurpose::Document)
    };
    let (model, route) = match route {
        Ok(r) => r,
        Err(e) => return (0..n).map(|_| Outcome::Retry(e.to_string())).collect(),
    };
    let mut out: Vec<_> = (0..n)
        .map(|_| Outcome::Retry("inference incomplete".into()))
        .collect();
    let width = (96 * 1024 * 1024usize / (dim.max(1) as usize * 96 + 256)).clamp(1, 512);
    let mut pending: Vec<_> = (0..n)
        .step_by(width)
        .map(|lo| (lo, (lo + width).min(n)))
        .rev()
        .collect();
    while let Some((lo, hi)) = pending.pop() {
        if hi == lo {
            continue;
        }
        let text = if vectors.is_some() {
            &[][..]
        } else {
            &texts[lo..hi]
        };
        match client
            .predict(text, vectors.map(|v| &v[lo..hi]), &model, &route)
            .await
        {
            Ok(rows) if rows.len() == hi - lo => {
                for (index, row) in (lo..hi).zip(rows) {
                    out[index] = if row.len() != dim as usize {
                        Outcome::Retry("inference dimension mismatch".into())
                    } else if row.iter().any(|v| !v.is_finite()) {
                        Outcome::Dead("non-finite inference output".into())
                    } else {
                        Outcome::Vector(row)
                    };
                }
            }
            Ok(_) => {
                for item in &mut out[lo..hi] {
                    *item = Outcome::Retry("inference row count mismatch".into());
                }
            }
            Err(e) => {
                let timeout = matches!(
                    &e,
                    PvError::Deadline { .. }
                        | PvError::Remote {
                            code: RavennaCode::Timeout,
                            ..
                        }
                );
                if hi - lo > 1
                    && (e.class() == ErrorClass::PoisonRow || (vectors.is_some() && timeout))
                {
                    let mid = (lo + hi) / 2;
                    pending.push((mid, hi));
                    pending.push((lo, mid));
                } else {
                    for item in &mut out[lo..hi] {
                        *item = match e.class() {
                            ErrorClass::Permanent => Outcome::Failed(e.to_string()),
                            ErrorClass::PoisonRow => Outcome::Dead(e.to_string()),
                            _ => Outcome::Retry(e.to_string()),
                        };
                    }
                }
            }
        }
    }
    out
}

pub(super) async fn finish(
    conn: &mut PgConnection,
    id: i64,
    error: Option<&str>,
    dead: bool,
) -> Result<()> {
    if dead {
        sqlx::query("WITH d AS (DELETE FROM postvec.jobs WHERE id=$1 RETURNING *) INSERT INTO postvec.jobs_dead(job_id,registry_id,pk_value,op,chunk_id,attempts,not_before,claimed_at,last_error,created_at) SELECT id,registry_id,pk_value,op,chunk_id,attempts,not_before,claimed_at,left($2,1024),created_at FROM d") .bind(id).bind(error).execute(&mut *conn).await?;
        sqlx::query(
            "UPDATE postvec.worker_heartbeat SET jobs_dead=jobs_dead+1,last_error=left($1,1024) WHERE id=1",
        )
        .bind(error)
        .execute(conn)
        .await?;
    } else {
        sqlx::query("DELETE FROM postvec.jobs WHERE id=$1")
            .bind(id)
            .execute(conn)
            .await?;
    }
    Ok(())
}
pub(super) async fn release(
    conn: &mut PgConnection,
    id: i64,
    error: &str,
    backoff: bool,
) -> Result<()> {
    let attempts: Option<i32> = sqlx::query_scalar("SELECT attempts FROM postvec.jobs WHERE id=$1")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    if backoff && attempts.is_some_and(|n| n >= 5) {
        return finish(conn, id, Some(error), true).await;
    }
    sqlx::query("WITH d AS (DELETE FROM postvec.jobs WHERE id=$1 RETURNING *) INSERT INTO postvec.jobs(registry_id,pk_value,op,chunk_id,attempts,not_before,last_error,created_at) SELECT registry_id,pk_value,op,chunk_id,CASE WHEN $3 THEN attempts ELSE greatest(attempts-1,0) END,now()+make_interval(secs=>CASE WHEN $3 THEN least(60,power(2,least(attempts,6))) ELSE 0 END),left($2,1024),created_at FROM d ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING") .bind(id).bind(error).bind(backoff).execute(&mut *conn).await?;
    sqlx::query(
        "UPDATE postvec.worker_heartbeat SET jobs_retried=jobs_retried+1,last_error=left($1,1024) WHERE id=1",
    )
    .bind(error)
    .execute(conn)
    .await?;
    Ok(())
}

/// Disable the entry when its objects are gone or changed; anything else is
/// the caller's error.
pub(super) async fn quarantine_or_error(
    conn: &mut PgConnection,
    id: i64,
    error: anyhow::Error,
) -> Result<bool> {
    let missing = error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|e| e.code())
        .is_some_and(|code| matches!(code.as_ref(), "42P01" | "42703"));
    if !missing && error.downcast_ref::<Quarantine>().is_none() {
        return Err(error);
    }
    log::warn!("managed entry {id} disabled: {error}");
    let mut tx = conn.begin().await?;
    guard(&mut tx).await?;
    sqlx::query(
        "UPDATE postvec.registry SET state='disabled',index_error=left($2,1024) WHERE id=$1",
    )
    .bind(id)
    .bind(error.to_string())
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE postvec.migrations SET state='failed',error=left($2,1024),finished_at=now() WHERE registry_id=$1 AND state IN ('running','awaiting_finalize')").bind(id).bind(error.to_string()).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM postvec.jobs WHERE registry_id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}
