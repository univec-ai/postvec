// SPDX-License-Identifier: BUSL-1.1

mod install;
mod platform;

use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Install the plain SQL schema, or verify an existing installation.
    Install(ConnectionArgs),
    /// Show schema version, platform, heartbeat and queue counts as JSON.
    Status(ConnectionArgs),
    /// Remove managed objects, preserving user tables and vector columns.
    Uninstall(ConnectionArgs),
}

#[derive(Debug, Args)]
pub struct ConnectionArgs {
    #[arg(long, value_name = "DSN")]
    pub dsn: String,
    /// Read the database password from a regular file with mode 0600.
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,
    /// Connection and statement timeout in seconds.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=3600))]
    pub timeout: u32,
}

pub async fn run(command: Command) -> anyhow::Result<i32> {
    install::run(command).await?;
    Ok(0)
}

mod admin;
mod inference;
mod maintenance;
mod proxy;
mod rewrite;
mod worker;
pub(crate) use admin::routes;

use crate::state::ServerState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{postgres::PgListener, Connection, Executor};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};
use tokio::time::Instant;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
/// Statement and lock timeout of worker connections: cursor scans over
/// large, mostly embedded tables need minutes, not the CLI's seconds.
const WORKER_TIMEOUT: u32 = 600;

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedDb {
    pub name: String,
    pub dsn: String,
    pub password_file: Option<PathBuf>,
    pub sync: bool,
    pub proxy_port: Option<u16>,
    pub proxy_max_connections: usize,
    pub poll_interval_ms: u64,
    pub poll_only: bool,
    pub batch_size: i32,
    pub index_concurrently: bool,
}
impl std::fmt::Debug for ManagedDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedDb")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
impl Default for ManagedDb {
    fn default() -> Self {
        Self {
            name: String::new(),
            dsn: String::new(),
            password_file: None,
            sync: true,
            proxy_port: None,
            proxy_max_connections: 256,
            poll_interval_ms: 2000,
            poll_only: false,
            batch_size: 64,
            index_concurrently: true,
        }
    }
}
impl ManagedDb {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || self.name.len() > 128
            || !self
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return Err("managed name must contain 1–128 letters, digits, -, _ or .".into());
        }
        if !(100..=3_600_000).contains(&self.poll_interval_ms)
            || !(1..=512).contains(&self.batch_size)
        {
            return Err("managed poll_interval_ms must be 100–3600000 and batch_size 1–512".into());
        }
        if !(1..=65536).contains(&self.proxy_max_connections) {
            return Err("managed proxy_max_connections must be 1–65536".into());
        }
        if self.proxy_port == Some(0) || (!self.sync && self.proxy_port.is_none()) {
            return Err("managed entry needs sync or a nonzero proxy_port".into());
        }
        self.dsn
            .parse::<sqlx::postgres::PgConnectOptions>()
            .map_err(|_| "invalid managed PostgreSQL DSN")?;
        Ok(())
    }
    fn args(&self) -> ConnectionArgs {
        ConnectionArgs {
            dsn: self.dsn.clone(),
            password_file: self.password_file.clone(),
            timeout: WORKER_TIMEOUT,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct DatabaseStatus {
    #[serde(skip)]
    sampled_at: std::time::SystemTime,
    pub name: String,
    pub leader: bool,
    pub error: Option<String>,
    pub database: Value,
    #[serde(skip)]
    stats: Option<Arc<proxy::Stats>>,
    pub proxy: Value,
}
pub struct ManagedRuntime {
    status: RwLock<Vec<DatabaseStatus>>,
}
impl ManagedRuntime {
    pub fn new(dbs: &[ManagedDb]) -> Self {
        Self {
            status: RwLock::new(
                dbs.iter()
                    .map(|d| DatabaseStatus {
                        sampled_at: std::time::SystemTime::now(),
                        name: d.name.clone(),
                        leader: false,
                        error: None,
                        database: json!({}),
                        stats: d.proxy_port.map(|port| {
                            Arc::new(proxy::Stats {
                                port,
                                ..Default::default()
                            })
                        }),
                        proxy: Value::Null,
                    })
                    .collect(),
            ),
        }
    }
    pub fn snapshot(&self) -> Vec<DatabaseStatus> {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .map(|mut s| {
                if let Some(age) = s.database["heartbeat_age_seconds"].as_f64() {
                    s.database["heartbeat_age_seconds"] =
                        json!(age + s.sampled_at.elapsed().unwrap_or_default().as_secs_f64());
                }
                if let Some(stats) = &s.stats {
                    s.proxy = stats.json();
                }
                s
            })
            .collect()
    }
    pub fn metrics(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for s in self.snapshot() {
            let d = &s.database;
            for (metric, value) in [
                ("queue_depth", d["queue_depth"].as_f64()),
                ("heartbeat_age_seconds", d["heartbeat_age_seconds"].as_f64()),
                ("leader", Some(if s.leader { 1.0 } else { 0.0 })),
            ] {
                if let Some(v) = value {
                    let _ = writeln!(out, "postvec_managed_{metric}{{db=\"{}\"}} {v}", s.name);
                }
            }
            for (outcome, field) in [
                ("embedded", "jobs_embedded"),
                ("nulled", "jobs_nulled"),
                ("retried", "jobs_retried"),
                ("dead", "jobs_dead"),
            ] {
                if let Some(v) = d["heartbeat"][field].as_u64() {
                    let _ = writeln!(
                        out,
                        "postvec_managed_jobs_total{{db=\"{}\",outcome=\"{outcome}\"}} {v}",
                        s.name
                    );
                }
            }
            if let Some(n) = s.proxy["connections"].as_i64() {
                let _ = writeln!(out, "postvec_proxy_connections{{db=\"{}\"}} {n}", s.name);
                for kind in ["search", "embed"] {
                    let _ = writeln!(
                        out,
                        "postvec_proxy_rewrites_total{{db=\"{}\",kind=\"{kind}\"}} {}",
                        s.name, s.proxy["rewrites"][kind]
                    );
                }
            }
        }
        out
    }
    fn stats(&self, name: &str) -> Option<Arc<proxy::Stats>> {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.stats.clone())
    }
    fn update(&self, name: &str, leader: bool, error: Option<String>, database: Option<Value>) {
        if let Some(s) = self
            .status
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .find(|s| s.name == name)
        {
            s.leader = leader;
            s.error = error;
            if let Some(database) = database {
                s.database = database;
                s.sampled_at = std::time::SystemTime::now();
            }
        }
    }
}

/// Bind every proxy port before anything slow happens, like the other listeners.
pub fn reserve(
    settings: &crate::config::Settings,
) -> Result<Vec<(String, std::net::TcpListener)>, String> {
    settings
        .managed
        .iter()
        .filter_map(|db| db.proxy_port.map(|port| (db, port)))
        .map(|(db, port)| {
            let addr = std::net::SocketAddr::new(settings.bind, port);
            std::net::TcpListener::bind(addr)
                .map(|l| (db.name.clone(), l))
                .map_err(|e| {
                    format!(
                        "cannot bind the proxy for managed database {} to {addr}: {e}",
                        db.name
                    )
                })
        })
        .collect()
}

pub fn start(
    state: &Arc<ServerState>,
    proxies: Vec<(String, std::net::TcpListener)>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks: Vec<_> = proxies
        .into_iter()
        .filter_map(|(name, listener)| {
            let db = state
                .settings
                .managed
                .iter()
                .find(|d| d.name == name)?
                .clone();
            let stats = state.managed.stats(&name)?;
            let state = state.clone();
            Some(tokio::spawn(async move {
                if let Err(e) = proxy::serve(state, db, listener, stats).await {
                    log::error!("proxy {name}: {e:#}");
                }
            }))
        })
        .collect();
    tasks.extend(state.settings.managed.iter().filter(|d| d.sync).map(|db| {
        if install::dsn_has_password(&db.dsn) {
            log::warn!("managed {}: password in DSN; prefer password_file", db.name);
        }
        let db = db.clone();
        let state = state.clone();
        tokio::spawn(async move {
            while !state.draining() {
                if let Err(e) = session(&state, &db).await {
                    log::warn!("managed {}: {e}", db.name);
                    state.managed.update(
                        &db.name,
                        false,
                        Some("Worker unavailable; see server log".into()),
                        None,
                    );
                }
                tokio::time::sleep(Duration::from_millis(db.poll_interval_ms.max(1000))).await;
            }
        })
    }));
    tasks
}

/// One database session: stand by until the leader lock is free, then drain
/// until the connection or the schema is lost. The lock is session-scoped, so
/// losing the connection is losing leadership.
async fn session(state: &Arc<ServerState>, db: &ManagedDb) -> anyhow::Result<()> {
    let mut conn = install::connect(&db.args()).await?;
    let mut listener: Option<PgListener> = None;
    let mut monitor: Option<AbortOnDrop> = None;
    let mut build: Option<AbortOnDrop> = None;
    let mut client = inference::Client::new(state);
    let lost = Arc::new(AtomicBool::new(false));
    let mut refresh = Instant::now();
    let mut sampled = Instant::now();
    loop {
        if state.draining() {
            return Ok(());
        }
        if lost.load(Ordering::Relaxed) {
            anyhow::bail!("leader session lost");
        }
        if monitor.is_none()
            && sqlx::query_scalar("SELECT pg_try_advisory_lock(1886615158, 2)")
                .fetch_one(&mut conn)
                .await?
        {
            let mut tx = conn.begin().await?;
            worker::guard(&mut tx).await?;
            sqlx::query("INSERT INTO postvec.settings(key,value) VALUES ('leader',to_jsonb($1::text)) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
                .bind(&state.identity.grpc_address).execute(&mut *tx).await?;
            tx.execute("INSERT INTO postvec.worker_heartbeat(id,pid,started_at,last_beat,jobs_done,errors) VALUES (1,pg_backend_pid(),now(),now(),0,0) ON CONFLICT(id) DO UPDATE SET pid=pg_backend_pid(),started_at=now(),last_beat=now()").await?;
            sqlx::query("UPDATE postvec.jobs SET claimed_at=now()-make_interval(secs=>$1) WHERE claimed_at IS NOT NULL")
                .bind(client.visibility_secs() + 60)
                .execute(&mut *tx)
                .await?;
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            monitor = Some(AbortOnDrop(tokio::spawn(heartbeat(
                state.clone(),
                db.clone(),
                lost.clone(),
                pid,
            ))));
            if !db.poll_only {
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(Duration::from_secs(10))
                    .connect_lazy_with(install::options(&db.args())?);
                let mut l = PgListener::connect_with(&pool).await?;
                l.listen("postvec_kick").await?;
                listener = Some(l);
            }
        }
        let mut progress = false;
        if monitor.is_some() {
            if refresh <= Instant::now() {
                client.refresh(state).await?;
                let mut tx = conn.begin().await?;
                worker::guard(&mut tx).await?;
                client.cache(&mut tx).await?;
                tx.commit().await?;
                refresh = Instant::now() + Duration::from_secs(30);
            }
            let stepped = async {
                Ok::<_, anyhow::Error>(
                    worker::step(&mut conn, &client, db).await?
                        | maintenance::step(&mut conn, &client, db, &mut build).await?,
                )
            }
            .await;
            progress = match stepped {
                Err(e) if transient(&e) => {
                    log::info!("managed {}: retrying after {e}", db.name);
                    let _ = conn.execute("ROLLBACK").await;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    true
                }
                other => other?,
            };
        } else if sampled <= Instant::now() {
            let status = admin::status(&mut conn).await?;
            state.managed.update(&db.name, false, None, Some(status));
            sampled = Instant::now() + SAMPLE_INTERVAL;
        }
        if progress {
            tokio::task::yield_now().await;
            continue;
        }
        let Some(l) = &mut listener else {
            tokio::time::sleep(Duration::from_millis(db.poll_interval_ms)).await;
            continue;
        };
        tokio::select! {
            first = l.recv() => {
                let mut kicks = vec![first?];
                kicks.extend(std::iter::from_fn(|| l.next_buffered()));
                if kicks.iter().any(|k| k.payload() == "refresh_models") {
                    refresh = Instant::now();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(db.poll_interval_ms)) => {}
        }
    }
}

/// The leader's heartbeat and status sampler, on its own connection so a
/// long index build or inference call never lets the beat go stale. The beat
/// is conditional on the leader session still holding its lock.
async fn heartbeat(state: Arc<ServerState>, db: ManagedDb, lost: Arc<AtomicBool>, pid: i32) {
    loop {
        let result = async {
            let mut conn = install::connect(&db.args()).await?;
            loop {
                let beats = sqlx::query("UPDATE postvec.worker_heartbeat SET last_beat=now() WHERE id=1 AND pid=$1 AND EXISTS(SELECT FROM postvec.schema_version WHERE version=$2 AND mode='managed') AND EXISTS(SELECT FROM pg_locks WHERE locktype='advisory' AND pid=$1 AND classid=1886615158 AND objid=2 AND objsubid=2 AND granted)")
                    .bind(pid).bind(install::VERSION).execute(&mut conn).await?.rows_affected();
                if beats != 1 {
                    lost.store(true, Ordering::Relaxed);
                    anyhow::bail!("leader session lost");
                }
                let status = admin::status(&mut conn).await?;
                state.managed.update(&db.name, true, None, Some(status));
                tokio::time::sleep(SAMPLE_INTERVAL).await;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            log::warn!("managed {} heartbeat: {e}", db.name);
        }
        state.managed.update(
            &db.name,
            false,
            Some("Heartbeat unavailable; see server log".into()),
            None,
        );
        if lost.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(SAMPLE_INTERVAL).await;
    }
}

fn transient(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<sqlx::Error>()?.as_database_error())
        .any(|d| matches!(d.code().as_deref(), Some("40001" | "40P01")))
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
