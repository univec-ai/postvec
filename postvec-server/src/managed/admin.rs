// SPDX-License-Identifier: BUSL-1.1

use super::{install, worker};
use crate::state::ServerState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{Connection, PgConnection};
use std::sync::Arc;

pub(super) async fn status(conn: &mut PgConnection) -> anyhow::Result<Value> {
    let raw:String=sqlx::query_scalar("SELECT jsonb_build_object('schema_version',(SELECT version FROM postvec.schema_version),'platform',(SELECT value FROM postvec.settings WHERE key='platform'),'leader',(SELECT value FROM postvec.settings WHERE key='leader'),'heartbeat_age_seconds',(SELECT extract(epoch FROM now()-last_beat) FROM postvec.worker_heartbeat),'queue_depth',(SELECT count(*) FROM postvec.jobs),'dead_letters',(SELECT count(*) FROM postvec.jobs_dead),'migrations',(SELECT coalesce(jsonb_agg(to_jsonb(m)),'[]') FROM postvec.migrations m WHERE state IN ('running','awaiting_finalize','awaiting_index')),'heartbeat',(SELECT to_jsonb(h) FROM postvec.worker_heartbeat h),'grant_script',(SELECT coalesce(string_agg(format('GRANT %I TO %I;',rolname,current_user),E'\\n'),'') FROM pg_roles WHERE oid IN (SELECT DISTINCT c.relowner FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ('postvec','information_schema') AND n.nspname NOT LIKE 'pg_%' AND NOT pg_has_role(current_user,c.relowner,'USAGE'))))::text") .fetch_one(conn).await?;
    Ok(serde_json::from_str(&raw)?)
}
async fn list(State(state): State<Arc<ServerState>>) -> Json<Value> {
    Json(json!({"success":true,"data":state.managed.snapshot()}))
}
#[derive(Deserialize)]
struct Retry {
    registry_id: i64,
    dead_ids: Option<Vec<i64>>,
}
async fn action(
    state: Arc<ServerState>,
    name: String,
    retry: Option<Retry>,
    jobs: bool,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = state
        .settings
        .managed
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"unknown database"})),
            )
        })?;
    let result=async {
        let mut conn=install::connect(&db.args()).await?;let mut tx=conn.begin().await?;worker::guard(&mut tx).await?;
        let data=if jobs {
            let raw:String=sqlx::query_scalar("SELECT jsonb_build_object('jobs',(SELECT coalesce(jsonb_agg(to_jsonb(j)),'[]') FROM (SELECT id,registry_id,op,attempts,not_before,claimed_at,last_error FROM postvec.jobs ORDER BY id DESC LIMIT 100) j),'dead',(SELECT coalesce(jsonb_agg(to_jsonb(j)),'[]') FROM (SELECT dead_id,registry_id,op,attempts,last_error FROM postvec.jobs_dead ORDER BY dead_id DESC LIMIT 100) j),'quarantine',(SELECT coalesce(jsonb_agg(to_jsonb(r)),'[]') FROM (SELECT id,table_schema,table_name,source_column,index_error FROM postvec.registry WHERE state='disabled' LIMIT 100) r))::text").fetch_one(&mut *tx).await?;serde_json::from_str::<Value>(&raw)?
        }else if let Some(retry)=retry {
            anyhow::ensure!(retry.dead_ids.as_ref().is_none_or(|v|v.len()<=1000),"too many dead IDs");
            let count:i64=sqlx::query_scalar("SELECT postvec.retry_dead(format('%I.%I',table_schema,table_name)::regclass,source_column,$2::bigint[]) FROM postvec.registry WHERE id=$1") .bind(retry.registry_id).bind(retry.dead_ids).fetch_one(&mut *tx).await?;json!({"retried":count})
        }else{sqlx::query("SELECT postvec.refresh_models()").execute(&mut *tx).await?;json!({})};
        tx.commit().await?;Ok::<_,anyhow::Error>(data)
    }.await;
    match result {
        Ok(data) => Ok(Json(json!({"success":true,"data":data}))),
        Err(e) => {
            log::warn!("managed admin {name}: {e}");
            Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"Database operation failed; check server log"})),
            ))
        }
    }
}
pub(crate) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/admin/managed", get(list))
        .route(
            "/admin/managed/:name/jobs",
            get(|State(s): State<Arc<ServerState>>, Path(n): Path<String>| {
                action(s, n, None, true)
            }),
        )
        .route(
            "/admin/managed/:name/refresh-models",
            post(|State(s): State<Arc<ServerState>>, Path(n): Path<String>| {
                action(s, n, None, false)
            }),
        )
        .route(
            "/admin/managed/:name/retry-dead",
            post(
                |State(s): State<Arc<ServerState>>, Path(n): Path<String>, Json(r): Json<Retry>| {
                    action(s, n, Some(r), false)
                },
            ),
        )
}
