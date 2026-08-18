// SPDX-License-Identifier: BUSL-1.1

use super::{
    inference::Client,
    worker::{self, Outcome},
    ManagedDb,
};
use anyhow::{ensure, Context, Result};
use postvec_core::{
    chunking,
    registry::{
        distance_opclass, parse_vector, quote_ident as qi, quote_literal_estring as ql,
        serialize_vector, vector_index_probe_sql,
    },
};
use sqlx::{Connection, Executor, PgConnection, Row};

/// Backfill cursors, chunk refreshes, migrations and index builds, for the
/// entries that currently have such work. Each entry's failure is isolated.
pub(super) async fn step(
    conn: &mut PgConnection,
    client: &Client,
    db: &ManagedDb,
    build: &mut Option<super::AbortOnDrop>,
) -> Result<bool> {
    if build.as_ref().is_some_and(|b| b.0.is_finished()) {
        *build = None;
    }
    let plan: Vec<(i64, bool, bool, bool)> = sqlx::query_as(&format!(
        "SELECT r.id,
            (r.backfill_mode='cursor' AND r.state='active' AND NOT EXISTS(SELECT FROM postvec.jobs j WHERE j.registry_id=r.id))
            OR EXISTS(SELECT FROM postvec.jobs j WHERE j.registry_id=r.id AND j.op='refresh' AND j.claimed_at IS NULL AND j.not_before<=now()),
            EXISTS(SELECT FROM postvec.migrations m WHERE m.registry_id=r.id AND m.state='running' AND m.not_before<=now()),
            r.index_mode='auto' AND r.state='active' AND r.index_error IS NULL AND r.backfill_mode<>'cursor'
            AND NOT EXISTS(SELECT FROM postvec.jobs j WHERE j.registry_id=r.id)
            AND NOT EXISTS(SELECT FROM postvec.jobs_dead d WHERE d.registry_id=r.id)
            AND NOT {}
         FROM postvec.registry r WHERE r.state<>'disabled' ORDER BY r.id",
        vector_index_probe_sql(
            "to_regclass(format('%I.%I',coalesce(r.destination_schema,r.table_schema),coalesce(r.destination_table,r.table_name)))",
            "r.vector_column"
        )
    ))
    .fetch_all(&mut *conn)
    .await?;
    let mut progress = false;
    for (id, local_work, migrating, wants_index) in plan {
        if wants_index && build.is_none() {
            let db = db.clone();
            *build = Some(super::AbortOnDrop(tokio::spawn(async move {
                if let Err(e) = index(&db, id).await {
                    log::warn!("managed {} index build {id}: {e}", db.name);
                }
            })));
        }
        let result = async {
            let mut work = false;
            if local_work {
                work |= local(conn, id, db).await?;
            }
            if migrating {
                work |= migrate(conn, client, id, db).await?;
            }
            Ok::<_, anyhow::Error>(work)
        }
        .await;
        progress |= match result {
            Ok(work) => work,
            Err(error) => worker::quarantine_or_error(conn, id, error).await?,
        };
    }
    Ok(progress)
}

async fn local(conn: &mut PgConnection, id: i64, db: &ManagedDb) -> Result<bool> {
    let mut tx = conn.begin().await?;
    worker::guard(&mut tx).await?;
    let Some(e) = worker::entry(&mut tx, id, true).await? else {
        tx.commit().await?;
        return Ok(false);
    };
    let mut progress = false;
    if e.backfill_mode == "cursor" && e.state == "active" {
        let queued: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT FROM postvec.jobs WHERE registry_id=$1)")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if !queued {
            let watermark = e
                .backfill_watermark
                .as_ref()
                .map(|w| e.pk_watermark_clause("", w))
                .unwrap_or_default();
            let all: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT FROM postvec.settings WHERE key='backfill_all:'||$1::text)",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
            let vector_pred = if e.is_recursive() || all {
                String::new()
            } else {
                format!(" AND {} IS NULL", qi(&e.vector_column))
            };
            let pks:Vec<String>=sqlx::query_scalar(&format!("SELECT {} FROM {} WHERE {} IS NOT NULL{vector_pred}{watermark} ORDER BY {} LIMIT $1",e.pk_text_expr(""),e.qualified_table(),qi(&e.source_column),e.pk_order_expr(""))) .bind(db.batch_size).fetch_all(&mut *tx).await?;
            sqlx::query("INSERT INTO postvec.jobs(registry_id,pk_value,op) SELECT $1,unnest($2::text[]),$3 ON CONFLICT(registry_id,op,pk_value,chunk_id) WHERE claimed_at IS NULL DO NOTHING") .bind(id).bind(&pks).bind(if e.is_recursive(){"refresh"}else{"embed"}).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE postvec.registry SET backfill_watermark=$2,backfill_mode=$3 WHERE id=$1",
            )
            .bind(id)
            .bind(pks.last())
            .bind(if pks.is_empty() { "done" } else { "cursor" })
            .execute(&mut *tx)
            .await?;
            if pks.is_empty() {
                sqlx::query("DELETE FROM postvec.settings WHERE key='backfill_all:'||$1::text")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            progress = true;
        }
    }
    let mut pending: i64=sqlx::query_scalar("SELECT count(*) FROM (SELECT 1 FROM postvec.jobs WHERE registry_id=$1 AND op='embed' LIMIT 10000) q").bind(id).fetch_one(&mut *tx).await?;
    for _ in 0..db.batch_size {
        if pending >= 10000 {
            break;
        }
        let job:Option<(i64,String)>=sqlx::query_as("UPDATE postvec.jobs SET claimed_at=now(),attempts=attempts+1 WHERE id=(SELECT id FROM postvec.jobs WHERE registry_id=$1 AND op='refresh' AND claimed_at IS NULL AND not_before<=now() ORDER BY not_before,id LIMIT 1 FOR UPDATE SKIP LOCKED) RETURNING id,pk_value") .bind(id).fetch_optional(&mut *tx).await?;
        let Some((jid, pk)) = job else {
            break;
        };
        progress = true;
        if !e.is_recursive() {
            worker::finish(&mut tx, jid, Some("refresh on non-chunked entry"), true).await?;
            continue;
        }
        let src = qi(&e.source_column);
        let row:Option<(Option<String>,Option<i64>)>=sqlx::query_as(&format!("SELECT CASE WHEN octet_length({src}::text)<=$2 THEN {src}::text END,octet_length({src}::text)::bigint FROM {} WHERE {} FOR SHARE",e.qualified_table(),worker::pk_pred(&e,"","$1"))) .bind(&pk).bind(chunking::MAX_DOCUMENT_BYTES as i64).fetch_optional(&mut *tx).await?;
        let text = row.as_ref().and_then(|r| r.0.as_deref()).unwrap_or("");
        let chunks = if row
            .as_ref()
            .and_then(|r| r.1)
            .is_some_and(|n| n > chunking::MAX_DOCUMENT_BYTES as i64)
        {
            Err("document exceeds splitter limit".into())
        } else {
            chunking::split_recursive(
                text,
                e.chunk_size.context("missing chunk size")?,
                e.chunk_overlap.context("missing chunk overlap")?,
            )
            .map_err(|e| e.to_string())
        };
        let chunks = match chunks {
            Err(error) => {
                worker::finish(&mut tx, jid, Some(&error), true).await?;
                continue;
            }
            Ok(chunks) => chunks,
        };
        sqlx::query(&format!(
            "DELETE FROM {} WHERE postvec_source_pk=$1::{}",
            e.qualified_vector_table(),
            e.pk_types[0]
        ))
        .bind(&pk)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM postvec.jobs WHERE registry_id=$1 AND pk_value=$2 AND op='embed'")
            .bind(id)
            .bind(&pk)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM postvec.jobs_dead WHERE registry_id=$1 AND pk_value=$2")
            .bind(id)
            .bind(&pk)
            .execute(&mut *tx)
            .await?;
        let seqs: Vec<_> = chunks.iter().map(|c| c.seq).collect();
        let starts: Vec<_> = chunks.iter().map(|c| c.char_start).collect();
        let ends: Vec<_> = chunks.iter().map(|c| c.char_end).collect();
        let texts: Vec<_> = chunks.iter().map(|c| &c.text).collect();
        let ids:Vec<i64>=sqlx::query_scalar(&format!("INSERT INTO {}(postvec_source_pk,postvec_chunk_seq,postvec_char_start,postvec_char_end,chunk_text) SELECT $1::{},* FROM unnest($2::int[],$3::bigint[],$4::bigint[],$5::text[]) RETURNING postvec_chunk_id",e.qualified_vector_table(),e.pk_types[0])) .bind(&pk).bind(seqs).bind(starts).bind(ends).bind(texts).fetch_all(&mut *tx).await?;
        sqlx::query("INSERT INTO postvec.jobs(registry_id,pk_value,op,chunk_id) SELECT $1,$2,'embed',unnest($3::bigint[])").bind(id).bind(&pk).bind(&ids).execute(&mut *tx).await?;
        worker::finish(&mut tx, jid, None, false).await?;
        sqlx::query("UPDATE postvec.worker_heartbeat SET documents_chunked=documents_chunked+1,chunks_created=chunks_created+$1 WHERE id=1").bind(ids.len() as i64).execute(&mut *tx).await?;
        pending += ids.len() as i64;
    }
    tx.commit().await?;
    Ok(progress)
}

async fn migrate(
    conn: &mut PgConnection,
    client: &Client,
    id: i64,
    db: &ManagedDb,
) -> Result<bool> {
    let mut tx = conn.begin().await?;
    worker::guard(&mut tx).await?;
    let Some(e) = worker::entry(&mut tx, id, true).await? else {
        tx.commit().await?;
        return Ok(false);
    };
    let row=sqlx::query("SELECT id,new_model,new_column,new_dim,resolved_via::text,last_pk FROM postvec.migrations WHERE registry_id=$1 AND state='running' AND not_before<=now() ORDER BY id LIMIT 1 FOR UPDATE") .bind(id).fetch_optional(&mut *tx).await?;
    let Some(m) = row else {
        tx.commit().await?;
        return Ok(false);
    };
    let mid: i64 = m.get("id");
    let model: String = m.get("new_model");
    let column: String = m.get("new_column");
    let dim: i32 = m.get("new_dim");
    let via: serde_json::Value = serde_json::from_str(&m.get::<String, _>("resolved_via"))?;
    let reembed = via["kind"] == "reembed";
    let last: Option<String> = m.get("last_pk");
    let (from, key, text, len, _) = worker::source(&e);
    let alias = if e.is_recursive() { "c" } else { "s" };
    let (expr, len) = if reembed {
        (text, len)
    } else {
        let expr = format!("{alias}.{}::text", qi(&e.vector_column));
        (expr.clone(), format!("octet_length({expr})"))
    };
    let (wm, order) = if e.is_recursive() {
        (
            last.as_ref()
                .map(|w| format!(" AND c.postvec_chunk_id>{}::bigint", ql(w)))
                .unwrap_or_default(),
            "c.postvec_chunk_id".to_string(),
        )
    } else {
        (
            last.as_ref()
                .map(|w| e.pk_watermark_clause("s", w))
                .unwrap_or_default(),
            e.pk_order_expr("s"),
        )
    };
    let rows=sqlx::query(&format!("SELECT {key} AS pk,CASE WHEN {len}<=$2 THEN {expr} END AS payload,{alias}.xmin::text AS version FROM {from} WHERE {alias}.{} IS NULL{wm} ORDER BY {order} LIMIT $1",qi(&column))) .bind(db.batch_size).bind(worker::ITEM_CAP).fetch_all(&mut *tx).await?;
    if rows.is_empty() {
        let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM postvec.jobs WHERE registry_id=$1) OR EXISTS(SELECT FROM postvec.registry WHERE id=$1 AND backfill_mode='cursor')").bind(id).fetch_one(&mut *tx).await?;
        if !busy {
            sqlx::query(
                "UPDATE postvec.migrations SET state='awaiting_finalize',error=NULL WHERE id=$1",
            )
            .bind(mid)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Ok(false);
    }
    tx.commit().await?;
    let payloads: Vec<String> = rows
        .iter()
        .filter_map(|r| r.get::<Option<String>, _>("payload"))
        .collect();
    let mut parsed = Vec::new();
    let mut parse_ok = Vec::new();
    if !reembed {
        for p in &payloads {
            match parse_vector(p) {
                Ok(v) => {
                    parsed.push(v);
                    parse_ok.push(true);
                }
                Err(_) => parse_ok.push(false),
            }
        }
    }
    let target = if reembed {
        &model
    } else {
        via["model"].as_str().context("missing converter model")?
    };
    let results = worker::infer(
        client,
        &payloads,
        if reembed { None } else { Some(&parsed) },
        target,
        dim,
    )
    .await;
    let mut tx = conn.begin().await?;
    worker::guard(&mut tx).await?;
    let Some(fresh) = worker::entry(&mut tx, id, true).await? else {
        tx.commit().await?;
        return Ok(true);
    };
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM postvec.migrations WHERE id=$1 AND state='running' AND last_pk IS NOT DISTINCT FROM $2)").bind(mid).bind(&last).fetch_one(&mut *tx).await?;
    if !active || fresh.format != e.format {
        tx.commit().await?;
        return Ok(false);
    }
    let key = if e.is_recursive() {
        "postvec_chunk_id=$2::bigint".to_string()
    } else {
        worker::pk_pred(&e, "", "$2")
    };
    let write = format!(
        "UPDATE {} SET {}=$1::{} WHERE {key} AND xmin::text=$3 AND {} IS NULL",
        e.qualified_vector_table(),
        qi(&column),
        worker::vector_type(&mut tx).await?,
        qi(&column)
    );
    let mut outcomes = results.into_iter();
    let mut parse_ok = parse_ok.into_iter();
    let (mut done, mut skipped) = (0i64, 0i64);
    let mut watermark = last.clone();
    for row in rows {
        let pk: String = row.get("pk");
        let version: String = row.get("version");
        let result = if row.get::<Option<String>, _>("payload").is_some() {
            if !reembed && parse_ok.next() != Some(true) {
                None
            } else {
                outcomes.next()
            }
        } else {
            None
        };
        match result {
            Some(Outcome::Vector(v)) => {
                let affected = sqlx::query(&write)
                    .bind(serialize_vector(&v))
                    .bind(&pk)
                    .bind(version)
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if affected == 0 {
                    break;
                }
                done += 1;
            }
            Some(Outcome::Retry(error)) => {
                sqlx::query("UPDATE postvec.migrations SET retry_failures=retry_failures+1,not_before=now()+make_interval(secs=>least(60,power(2,least(retry_failures+1,6)))),error=$2 WHERE id=$1").bind(mid).bind(error).execute(&mut *tx).await?;
                break;
            }
            Some(Outcome::Failed(error)) => {
                sqlx::query("UPDATE postvec.migrations SET state='failed',error=$2,finished_at=now() WHERE id=$1").bind(mid).bind(error).execute(&mut *tx).await?;
                break;
            }
            Some(Outcome::Dead(_)) | None => skipped += 1,
        }
        watermark = Some(pk);
    }
    if done + skipped > 0 && watermark != last {
        sqlx::query("UPDATE postvec.migrations SET retry_failures=0,error=NULL WHERE id=$1 AND state='running' AND not_before<=now()").bind(mid).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE postvec.migrations SET last_pk=$2,rows_done=rows_done+$3,rows_skipped=rows_skipped+$4 WHERE id=$1").bind(mid).bind(&watermark).bind(done).bind(skipped).execute(&mut *tx).await?;
    sqlx::query("UPDATE postvec.worker_heartbeat SET rows_converted=rows_converted+$1,rows_skipped=rows_skipped+$2 WHERE id=1").bind(done).bind(skipped).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(watermark != last)
}

/// Builds run on their own connection so draining continues meanwhile.
async fn index(db: &ManagedDb, id: i64) -> Result<()> {
    let mut conn = super::install::connect(&db.args()).await?;
    if let Err(error) = build_index(&mut conn, id, db).await {
        if super::transient(&error) {
            return Ok(());
        }
        let mut tx = conn.begin().await?;
        worker::guard(&mut tx).await?;
        sqlx::query("UPDATE postvec.registry SET index_error=$2 WHERE id=$1")
            .bind(id)
            .bind(error.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

async fn build_index(conn: &mut PgConnection, id: i64, db: &ManagedDb) -> Result<()> {
    let mut tx = conn.begin().await?;
    worker::guard(&mut tx).await?;
    let Some(e) = worker::entry(&mut tx, id, true).await? else {
        tx.commit().await?;
        return Ok(());
    };
    let busy:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM postvec.jobs WHERE registry_id=$1) OR EXISTS(SELECT FROM postvec.jobs_dead WHERE registry_id=$1)").bind(id).fetch_one(&mut *tx).await?;
    if e.index_mode != "auto"
        || e.state != "active"
        || e.index_error.is_some()
        || e.backfill_mode == "cursor"
        || busy
    {
        tx.commit().await?;
        return Ok(());
    }
    let table = e.qualified_vector_table();
    let probe = format!(
        "SELECT {}",
        vector_index_probe_sql(&format!("{}::regclass", ql(&table)), &ql(&e.vector_column))
    );
    if sqlx::query_scalar(&probe).fetch_one(&mut *tx).await? {
        tx.commit().await?;
        return Ok(());
    }
    let key = format!("index:{id}");
    let mut name: Option<String> =
        sqlx::query_scalar("SELECT value#>>'{}' FROM postvec.settings WHERE key=$1")
            .bind(&key)
            .fetch_optional(&mut *tx)
            .await?;
    if name.is_none() {
        name = Some(format!("postvec_{}", uuid::Uuid::new_v4().simple()));
        sqlx::query("INSERT INTO postvec.settings(key,value) VALUES($1,to_jsonb($2::text))")
            .bind(&key)
            .bind(&name)
            .execute(&mut *tx)
            .await?;
    }
    let name = name.unwrap();
    let qualified = format!(
        "{}.{}",
        qi(e.destination_schema.as_ref().unwrap_or(&e.table_schema)),
        qi(&name)
    );
    let vector_schema:String=sqlx::query_scalar("SELECT quote_ident(n.nspname) FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector'").fetch_one(&mut *tx).await?;
    let col = qi(&e.vector_column);
    let (expr, op) = if e.dim > 2000 {
        (
            format!("({col}::{vector_schema}.halfvec({}))", e.dim),
            postvec_core::registry::halfvec_opclass(&e.distance),
        )
    } else {
        (col, distance_opclass(&e.distance))
    };
    let existing:Option<(bool,bool)>=sqlx::query_as("SELECT i.indisvalid,i.indrelid=to_regclass($2) AND am.amname='hnsw' AND i.indnatts=1 AND i.indpred IS NULL AND NOT i.indisunique AND oc.opcname=$3 AND EXISTS(SELECT FROM pg_depend d JOIN pg_attribute a ON a.attrelid=d.refobjid AND a.attnum=d.refobjsubid WHERE d.classid='pg_class'::regclass AND d.objid=i.indexrelid AND d.refobjid=i.indrelid AND a.attname=$4) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam JOIN pg_opclass oc ON oc.oid=i.indclass[0] WHERE i.indexrelid=to_regclass($1)") .bind(&qualified).bind(&table).bind(op).bind(&e.vector_column).fetch_optional(&mut *tx).await?;
    if let Some((valid, ours)) = existing {
        ensure!(ours, "index build intent no longer matches catalog");
        if valid {
            tx.commit().await?;
            return Ok(());
        }
    }
    tx.commit().await?;
    let concurrently = if db.index_concurrently {
        "CONCURRENTLY "
    } else {
        ""
    };
    conn.execute("SET statement_timeout='1h'; SET lock_timeout=0")
        .await?;
    if existing.is_some() {
        conn.execute(format!("DROP INDEX {concurrently}{qualified}").as_str())
            .await?;
    }
    let result = conn
        .execute(
            format!(
                "CREATE INDEX {concurrently}{} ON {table} USING hnsw ({expr} {vector_schema}.{op})",
                qi(&name)
            )
            .as_str(),
        )
        .await;
    if let Err(error) = result {
        let retry = error
            .as_database_error()
            .and_then(|e| e.code())
            .is_some_and(|c| matches!(c.as_ref(), "57014" | "40P01" | "55P03"));
        return if retry { Ok(()) } else { Err(error.into()) };
    }
    conn.execute(
        format!(
            "COMMENT ON INDEX {qualified} IS {}",
            ql(&format!("postvec managed index {id}"))
        )
        .as_str(),
    )
    .await?;
    Ok(())
}
