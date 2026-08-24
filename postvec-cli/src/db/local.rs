//! In-process execution of [`DbRequest`]s.
//!
//! This is the only place queries actually run — the privilege-dropped agent
//! decodes a request and calls straight into here, so root and the cluster
//! owner cannot diverge in behaviour.
//!
//! Connections are cached per database. A `doctor` run over three databases
//! opens three connections and no more.

use super::sql;
use super::{
    DatabaseListing, DbRequest, DbTarget, ExtensionExpectation, HeartbeatSample, InstallOutcome,
    UninstallOutcome, MAINTENANCE_DATABASES,
};
use crate::error::{redact, CliError, Result};
use crate::facts::*;
use crate::validate;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{Connection, Executor, Row};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

/// Per-statement bound. Prevents a lock wait or an unresponsive server from
/// consuming the whole command deadline in one query.
const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Direct {
    target: DbTarget,
    statement_timeout: Duration,
    /// The maintenance database that answered, remembered so the fallback is
    /// resolved once.
    maintenance: Option<String>,
    connections: HashMap<String, PgConnection>,
}

impl Direct {
    pub fn new(target: DbTarget, statement_timeout: Option<Duration>) -> Self {
        Self {
            target,
            statement_timeout: statement_timeout.unwrap_or(DEFAULT_STATEMENT_TIMEOUT),
            maintenance: None,
            connections: HashMap::new(),
        }
    }

    fn options(&self, database: &str) -> Result<PgConnectOptions> {
        let options = match &self.target {
            DbTarget::Socket { dir, port, user } => PgConnectOptions::new()
                .socket(dir)
                .port(*port)
                .username(user),
            // The database is set through the option, never by editing the URI
            // string. `sslmode` and every other parameter the operator chose
            // survives untouched — a requested TLS connection is never
            // silently downgraded.
            DbTarget::Url(url) => PgConnectOptions::from_str(&normalize_database_url(url))
                .map_err(|e| {
                    CliError::usage(format!(
                        "--database-url is not a valid PostgreSQL URI: {}",
                        redact(&e.to_string())
                    ))
                })?,
        };
        Ok(options
            .database(database)
            .application_name("postvec-cli")
            // Applied by the server at connection start, so it covers every
            // statement including the first.
            .options([(
                "statement_timeout",
                self.statement_timeout.as_millis().to_string().as_str(),
            )]))
    }

    async fn connect(&self, database: &str) -> Result<PgConnection> {
        let options = self.options(database)?;
        PgConnection::connect_with(&options).await.map_err(|e| {
            CliError::precondition(format!(
                "cannot connect to database {database:?} ({}): {}",
                self.target.describe(),
                redact(&e.to_string())
            ))
            .with_fix(connect_hint(&self.target, &e))
        })
    }

    async fn conn(&mut self, database: &str) -> Result<&mut PgConnection> {
        if !self.connections.contains_key(database) {
            let connection = self.connect(database).await?;
            self.connections.insert(database.to_string(), connection);
        }
        Ok(self
            .connections
            .get_mut(database)
            .expect("just inserted a connection"))
    }

    /// A connection to a database that always exists, for cluster-wide work.
    async fn maintenance_conn(&mut self) -> Result<&mut PgConnection> {
        if let Some(name) = self.maintenance.clone() {
            return self.conn(&name).await;
        }
        let mut last_error = None;
        for candidate in MAINTENANCE_DATABASES {
            match self.connect(candidate).await {
                Ok(connection) => {
                    self.connections.insert(candidate.to_string(), connection);
                    self.maintenance = Some(candidate.to_string());
                    return self.conn(candidate).await;
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            CliError::precondition("no maintenance database (postgres, template1) is reachable")
        }))
    }

    pub async fn close(mut self) {
        for (_, connection) in self.connections.drain() {
            let _ = connection.close().await;
        }
    }

    pub async fn execute(&mut self, request: DbRequest) -> Result<Value> {
        match request {
            DbRequest::ServerFacts => Ok(json!(self.server_facts().await?)),
            DbRequest::Settings { names } => Ok(json!(self.settings(&names).await?)),
            DbRequest::FileSettings { names } => Ok(json!(self.file_settings(&names).await?)),
            DbRequest::CreateDatabase { database } => {
                self.create_database(&database).await?;
                Ok(Value::Null)
            }
            DbRequest::InspectDatabase { database } => {
                Ok(json!(self.inspect_database(&database).await?))
            }
            DbRequest::InstallExtension { database, expect } => {
                Ok(json!(self.install_extension(&database, &expect).await?))
            }
            DbRequest::UninstallExtension {
                database,
                drop_columns,
                drop_destinations,
                lock_timeout_ms,
            } => Ok(json!(
                self.uninstall_extension(
                    &database,
                    drop_columns,
                    drop_destinations,
                    lock_timeout_ms
                )
                .await?
            )),
            DbRequest::ListDatabases => Ok(json!(self.list_databases().await?)),
            DbRequest::ReloadConfig { database } => {
                self.reload_config(&database).await?;
                Ok(Value::Null)
            }
            DbRequest::RefreshModels { database } => {
                Ok(json!(self.refresh_models(&database).await?))
            }
            DbRequest::StartWorker { database } => Ok(json!(self.start_worker(&database).await?)),
            DbRequest::HeartbeatSample { database } => {
                Ok(json!(self.heartbeat_sample(&database).await?))
            }
            DbRequest::Reconnect => {
                for (_, connection) in self.connections.drain() {
                    let _ = connection.close().await;
                }
                Ok(Value::Null)
            }
        }
    }

    async fn server_facts(&mut self) -> Result<ServerFacts> {
        let conn = self.maintenance_conn().await?;
        let row = sqlx::query(sql::SERVER_FACTS).fetch_one(&mut *conn).await?;
        let mut facts = ServerFacts {
            version_num: row.try_get("version_num")?,
            version: row.try_get("version")?,
            data_directory: None,
            config_file: None,
            hba_file: None,
            port: row.try_get("port")?,
            postmaster_start_time: row.try_get("start_time")?,
            postmaster_start_exact: row.try_get("start_exact").ok(),
            system_identifier: None,
        };
        // Paths need superuser or pg_read_all_settings. Without them the
        // report loses the offline-validation checks and says so, rather than
        // failing outright.
        if let Ok(paths) = sqlx::query(sql::SERVER_PATHS).fetch_one(&mut *conn).await {
            facts.data_directory = paths.try_get("data_directory").ok();
            facts.config_file = paths.try_get("config_file").ok();
            facts.hba_file = paths.try_get("hba_file").ok();
        }
        if let Ok(row) = sqlx::query(sql::SYSTEM_IDENTIFIER)
            .fetch_one(&mut *conn)
            .await
        {
            facts.system_identifier = row.try_get("system_identifier").ok();
        }
        Ok(facts)
    }

    async fn settings(&mut self, names: &[String]) -> Result<Vec<SettingRow>> {
        let conn = self.maintenance_conn().await?;
        let rows = sqlx::query(sql::SETTINGS)
            .bind(names)
            .fetch_all(&mut *conn)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(SettingRow {
                    name: row.try_get("name")?,
                    setting: row
                        .try_get::<Option<String>, _>("setting")?
                        .unwrap_or_default(),
                    context: row.try_get("context")?,
                    source: row.try_get("source")?,
                    sourcefile: row.try_get("sourcefile")?,
                    sourceline: row.try_get("sourceline")?,
                    pending_restart: row.try_get("pending_restart")?,
                })
            })
            .collect()
    }

    async fn file_settings(&mut self, names: &[String]) -> Result<Vec<FileSettingRow>> {
        let conn = self.maintenance_conn().await?;
        let rows = sqlx::query(sql::FILE_SETTINGS)
            .bind(names)
            .fetch_all(&mut *conn)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(FileSettingRow {
                    name: row
                        .try_get::<Option<String>, _>("name")?
                        .unwrap_or_default(),
                    setting: row
                        .try_get::<Option<String>, _>("setting")?
                        .unwrap_or_default(),
                    sourcefile: row.try_get("sourcefile")?,
                    sourceline: row.try_get("sourceline")?,
                    applied: row.try_get("applied")?,
                    error: row.try_get("error")?,
                })
            })
            .collect()
    }

    async fn database_exists(&mut self, database: &str) -> Result<bool> {
        let conn = self.maintenance_conn().await?;
        let row = sqlx::query(sql::DATABASE_EXISTS)
            .bind(database)
            .fetch_one(&mut *conn)
            .await?;
        Ok(row.try_get(0)?)
    }

    async fn create_database(&mut self, database: &str) -> Result<()> {
        // The one place the CLI renders an identifier into SQL: CREATE
        // DATABASE cannot be parameterized and cannot run in a transaction.
        let identifier = validate::quote_identifier(database)?;
        let statement = format!("CREATE DATABASE {identifier}");
        let conn = self.maintenance_conn().await?;
        conn.execute(statement.as_str()).await.map_err(|e| {
            CliError::apply(format!(
                "CREATE DATABASE {database:?} failed: {}",
                redact(&e.to_string())
            ))
        })?;
        Ok(())
    }

    async fn available_extensions(conn: &mut PgConnection) -> Result<Vec<ExtensionAvailability>> {
        let rows = sqlx::query(sql::AVAILABLE_EXTENSIONS)
            .fetch_all(&mut *conn)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ExtensionAvailability {
                    name: row.try_get("name")?,
                    default_version: row
                        .try_get::<Option<String>, _>("default_version")?
                        .unwrap_or_default(),
                    installed_version: row.try_get("installed_version")?,
                })
            })
            .collect()
    }

    async fn inspect_database(&mut self, database: &str) -> Result<DatabaseFacts> {
        if !self.database_exists(database).await? {
            return Ok(DatabaseFacts::absent(database));
        }
        let mut facts = DatabaseFacts::absent(database);
        facts.exists = true;

        let conn = match self.conn(database).await {
            Ok(conn) => conn,
            Err(e) => {
                facts.unreachable = Some(e.to_string());
                return Ok(facts);
            }
        };

        facts.available = Self::available_extensions(&mut *conn).await?;
        facts.vector_version = facts
            .availability("vector")
            .and_then(|a| a.installed_version.clone());
        let installed = facts
            .availability("postvec")
            .and_then(|a| a.installed_version.clone());
        let Some(catalog_version) = installed else {
            // No extension: nothing further is observable, and that is a
            // legitimate state (the package is installed, the database is not
            // set up yet).
            return Ok(facts);
        };

        let library_version: String = sqlx::query(sql::LIBRARY_VERSION)
            .fetch_one(&mut *conn)
            .await?
            .try_get("version")?;
        let build_info = Self::read_build_info(&mut *conn).await?;
        facts.postvec = Some(ExtensionFacts {
            catalog_version,
            library_version,
            build_info,
        });

        facts.worker = Some(Self::read_worker(&mut *conn).await?);
        facts.queue = Some(Self::read_queue(&mut *conn).await?);
        facts.registry = Self::read_registry(&mut *conn).await?;
        facts.models = Self::read_model_cache(&mut *conn).await?;
        facts.migrations = Self::read_migrations(&mut *conn).await?;
        Ok(facts)
    }

    async fn read_build_info(conn: &mut PgConnection) -> Result<Option<BuildInfo>> {
        let present: bool = sqlx::query(sql::HAS_BUILD_INFO)
            .fetch_one(&mut *conn)
            .await?
            .try_get("present")?;
        if !present {
            return Ok(None);
        }
        let raw: Value = sqlx::query(sql::BUILD_INFO)
            .fetch_one(&mut *conn)
            .await?
            .try_get("info")?;
        // Tolerant: an older or newer extension may report more fields, and a
        // missing one should degrade the report, not fail the command.
        let embedded = raw
            .pointer("/features/embedded")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let model_backends = raw
            .pointer("/features/model_backends")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            });
        Ok(Some(BuildInfo {
            version: raw
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            diagnostics_api: raw
                .get("diagnostics_api")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            embedded,
            model_backends,
        }))
    }

    async fn read_worker(conn: &mut PgConnection) -> Result<WorkerFacts> {
        let Some(row) = sqlx::query(sql::HEARTBEAT)
            .fetch_optional(&mut *conn)
            .await?
        else {
            return Ok(WorkerFacts::default());
        };
        let pid: Option<i32> = row.try_get("pid")?;
        let mut worker = WorkerFacts {
            pid,
            last_beat_age_s: row.try_get("age_s")?,
            predates_restart: row
                .try_get::<Option<bool>, _>("predates_restart")?
                .unwrap_or(false),
            started_at: row.try_get("started_at")?,
            last_error: row.try_get("last_error")?,
            errors: row.try_get("errors")?,
            jobs_embedded: row.try_get("jobs_embedded")?,
            jobs_dead_lettered: row.try_get("jobs_dead")?,
            model_refreshes: row.try_get("model_refreshes")?,
            pid_is_live: false,
            second_beat_age_s: None,
            advanced: None,
        };
        if let Some(pid) = pid {
            worker.pid_is_live = sqlx::query(sql::PID_IS_LIVE)
                .bind(pid)
                .fetch_one(&mut *conn)
                .await?
                .try_get(0)?;
        }
        Ok(worker)
    }

    async fn read_queue(conn: &mut PgConnection) -> Result<QueueFacts> {
        let totals = sqlx::query(sql::QUEUE_TOTALS).fetch_one(&mut *conn).await?;
        let dead = sqlx::query(sql::DEAD_LETTERS).fetch_one(&mut *conn).await?;
        Ok(QueueFacts {
            pending: totals.try_get("pending")?,
            claimed: totals.try_get("claimed")?,
            oldest_pending_s: totals.try_get("oldest_pending_s")?,
            dead: dead.try_get("dead")?,
            dead_reasons: dead.try_get("reasons")?,
        })
    }

    async fn read_registry(conn: &mut PgConnection) -> Result<Vec<RegistryEntryFacts>> {
        let rows = sqlx::query(sql::REGISTRY_ENTRIES)
            .fetch_all(&mut *conn)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RegistryEntryFacts {
                    registry_id: row.try_get("id")?,
                    relation: row.try_get("relation")?,
                    source_column: row.try_get("source_column")?,
                    vector_column: row.try_get("vector_column")?,
                    model: row.try_get("model")?,
                    dim: row.try_get("dim")?,
                    state: row.try_get("state")?,
                    pending_jobs: row.try_get("pending_jobs")?,
                    dead_jobs: row.try_get("dead_jobs")?,
                    has_vector_index: row.try_get("has_vector_index")?,
                    index_mode: row.try_get("index_mode")?,
                    index_error: row.try_get("index_error")?,
                    has_expected_opclass_index: row.try_get("has_expected_opclass_index")?,
                    last_error: row.try_get("last_error")?,
                    relation_exists: row.try_get("relation_exists")?,
                    source_column_exists: row.try_get("source_column_exists")?,
                    vector_column_exists: row.try_get("vector_column_exists")?,
                    trigger_count: row.try_get("trigger_count")?,
                    destination: row.try_get("destination")?,
                })
            })
            .collect()
    }

    async fn read_model_cache(conn: &mut PgConnection) -> Result<ModelCacheFacts> {
        let row = sqlx::query(sql::MODEL_CACHE).fetch_one(&mut *conn).await?;
        Ok(ModelCacheFacts {
            count: row.try_get("count")?,
            newest_last_seen_s: row.try_get("newest_last_seen_s")?,
            names: row.try_get("names")?,
        })
    }

    async fn read_migrations(conn: &mut PgConnection) -> Result<Vec<MigrationFacts>> {
        let rows = sqlx::query(sql::OPEN_MIGRATIONS)
            .fetch_all(&mut *conn)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(MigrationFacts {
                    id: row.try_get("id")?,
                    registry_id: row.try_get("registry_id")?,
                    state: row.try_get("state")?,
                    rows_done: row.try_get("rows_done")?,
                    rows_total: row.try_get("rows_total")?,
                    error: row.try_get("error")?,
                    age_s: row.try_get("age_s")?,
                    retry_failures: row.try_get("retry_failures")?,
                })
            })
            .collect()
    }

    /// `CREATE EXTENSION` and capability verification in one transaction: a
    /// library that cannot satisfy the requested mode is never committed.
    async fn install_extension(
        &mut self,
        database: &str,
        expect: &ExtensionExpectation,
    ) -> Result<InstallOutcome> {
        let conn = self.conn(database).await?;
        let mut tx = conn.begin().await?;

        let before: Option<String> = sqlx::query(sql::EXTENSION_PRESENT)
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.try_get("extversion"))
            .transpose()?;

        tx.execute(sql::CREATE_EXTENSION).await.map_err(|e| {
            CliError::apply(format!(
                "CREATE EXTENSION postvec in {database:?} failed: {}",
                redact(&e.to_string())
            ))
            .with_fix(
                "postvec is an untrusted extension: the connecting role must be a superuser, \
                 and pgvector >= 0.8 must be installed in the cluster",
            )
        })?;

        let catalog_version: String = sqlx::query(sql::EXTENSION_PRESENT)
            .fetch_one(&mut *tx)
            .await?
            .try_get("extversion")?;
        let library_version: String = sqlx::query(sql::LIBRARY_VERSION)
            .fetch_one(&mut *tx)
            .await?
            .try_get("version")?;
        let build_info = Self::read_build_info(&mut tx).await?;
        let vector_version = Self::available_extensions(&mut tx)
            .await?
            .into_iter()
            .find(|a| a.name == "vector")
            .and_then(|a| a.installed_version);

        // Verify before committing. Dropping the transaction rolls back.
        if catalog_version != library_version {
            return Err(CliError::apply(format!(
                "postvec version skew in {database:?}: installed SQL is {catalog_version} but \
                 the loaded library is {library_version}"
            ))
            .with_fix(
                "restart the cluster so the new library is loaded, then run \
                 `ALTER EXTENSION postvec UPDATE` in this database",
            ));
        }
        if expect.require_embedded_build {
            match &build_info {
                Some(info) if info.embedded => {}
                Some(_) => {
                    return Err(CliError::precondition(format!(
                        "the installed postvec library in {database:?} was built without the \
                         'embedded' feature, so embedded mode cannot work"
                    ))
                    .with_fix(
                        "install a package built with --features embedded, or run setup \
                         without --embedded",
                    ))
                }
                None => {
                    return Err(CliError::precondition(format!(
                        "the postvec extension in {database:?} is too old to report its build \
                         features, so embedded mode cannot be verified"
                    ))
                    .with_fix(
                        "upgrade postvec to a version providing postvec.build_info(), or run \
                         setup without --embedded",
                    ))
                }
            }
        }

        tx.commit().await?;
        Ok(InstallOutcome {
            created: before.is_none(),
            catalog_version,
            library_version,
            build_info,
            vector_version,
        })
    }

    /// Runtime cleanup plus `DROP EXTENSION`, in one transaction with a lock
    /// timeout: a blocked `ALTER TABLE` rolls the whole thing back and leaves
    /// the extension usable rather than half-removed.
    async fn list_databases(&mut self) -> Result<Vec<DatabaseListing>> {
        let conn = self.maintenance_conn().await?;
        let rows = sqlx::query(sql::LIST_DATABASES)
            .fetch_all(&mut *conn)
            .await?;
        rows.iter()
            .map(|row| {
                Ok(DatabaseListing {
                    name: row.try_get("datname")?,
                    allow_conn: row.try_get("allow_conn")?,
                    is_template: row.try_get("is_template")?,
                })
            })
            .collect()
    }

    async fn uninstall_extension(
        &mut self,
        database: &str,
        drop_columns: bool,
        drop_destinations: bool,
        lock_timeout_ms: u32,
    ) -> Result<UninstallOutcome> {
        let conn = self.conn(database).await?;
        let present: bool = sqlx::query(sql::EXTENSION_PRESENT)
            .fetch_optional(&mut *conn)
            .await?
            .is_some();
        if !present {
            return Ok(UninstallOutcome {
                was_present: false,
                cleaned_entries: 0,
                retained_destinations: Vec::new(),
            });
        }
        let has_uninstall: bool = sqlx::query(sql::HAS_UNINSTALL)
            .fetch_one(&mut *conn)
            .await?
            .try_get("present")?;
        if !has_uninstall {
            return Err(CliError::precondition(format!(
                "the postvec extension in {database:?} has no postvec.uninstall() function"
            ))
            .with_fix(
                "upgrade postvec, or remove the generated triggers with \
                 postvec.disable() per entry before dropping the extension; the CLI will not \
                 use DROP EXTENSION ... CASCADE",
            ));
        }

        let mut tx = conn.begin().await?;
        // SET LOCAL: scoped to this transaction, gone on rollback.
        tx.execute(format!("SET LOCAL lock_timeout = {lock_timeout_ms}").as_str())
            .await?;
        // What was asked to go, so the catalog can be checked afterwards:
        // a failed ownership proof keeps a destination with only a WARNING.
        let destinations: Vec<(String, String)> = if drop_destinations {
            sqlx::query(sql::REGISTRY_DESTINATIONS)
                .fetch_all(&mut *tx)
                .await?
                .iter()
                .map(|row| {
                    Ok((
                        row.try_get("destination_schema")?,
                        row.try_get("destination_table")?,
                    ))
                })
                .collect::<Result<_>>()?
        } else {
            Vec::new()
        };
        let cleaned_entries: i64 = sqlx::query(sql::UNINSTALL_ENTRIES)
            .bind(drop_columns)
            .bind(drop_destinations)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                CliError::apply(format!(
                    "postvec.uninstall() in {database:?} failed: {}",
                    redact(&e.to_string())
                ))
                .with_fix(
                    "postvec.uninstall() requires a superuser; if it timed out, a concurrent \
                     transaction holds a lock on an enabled table — retry when it is idle",
                )
            })?
            .try_get("cleaned")?;
        let mut retained_destinations = Vec::new();
        for (schema, table) in &destinations {
            let present: bool = sqlx::query(sql::RELATION_EXISTS)
                .bind(schema)
                .bind(table)
                .fetch_one(&mut *tx)
                .await?
                .try_get("present")?;
            if present {
                retained_destinations.push(format!("{schema}.{table}"));
            }
        }
        tx.execute(sql::DROP_EXTENSION).await.map_err(|e| {
            CliError::apply(format!(
                "DROP EXTENSION postvec in {database:?} failed: {}",
                redact(&e.to_string())
            ))
            .with_fix(
                "an object still depends on postvec; identify it with \
                 `\\dx+ postvec` and remove it — the CLI will not use CASCADE",
            )
        })?;
        tx.commit().await?;
        Ok(UninstallOutcome {
            was_present: true,
            cleaned_entries,
            retained_destinations,
        })
    }

    async fn reload_config(&mut self, database: &str) -> Result<()> {
        let conn = self.conn(database).await?;
        sqlx::query(sql::RELOAD_CONF)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| {
                CliError::apply(format!(
                    "pg_reload_conf() failed: {}",
                    redact(&e.to_string())
                ))
                .with_fix("reloading the configuration requires a superuser")
            })?;
        Ok(())
    }

    async fn start_worker(&mut self, database: &str) -> Result<bool> {
        let conn = self.conn(database).await?;
        let row = sqlx::query(sql::START_WORKER)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| {
                CliError::apply(format!(
                    "postvec.start_worker() in {database:?} failed: {}",
                    redact(&e.to_string())
                ))
                .with_fix("read the PostgreSQL log; max_worker_processes may be exhausted")
            })?;
        Ok(row.try_get("started")?)
    }

    async fn refresh_models(&mut self, database: &str) -> Result<i64> {
        let conn = self.conn(database).await?;
        let row = sqlx::query(sql::REFRESH_MODELS)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| {
                CliError::apply(format!(
                    "postvec.refresh_models() in {database:?} failed: {}",
                    redact(&e.to_string())
                ))
                .with_fix(
                    "the configured HTTP discovery endpoint must answer GET /config; in \
                     embedded mode the engine may still be loading models",
                )
            })?;
        let models: i64 = row.try_get::<i64, _>("models").unwrap_or(0);
        Ok(models)
    }

    async fn heartbeat_sample(&mut self, database: &str) -> Result<HeartbeatSample> {
        let conn = self.conn(database).await?;
        let Some(row) = sqlx::query(sql::HEARTBEAT_SAMPLE)
            .fetch_optional(&mut *conn)
            .await?
        else {
            return Ok(HeartbeatSample {
                pid: None,
                age_s: None,
            });
        };
        Ok(HeartbeatSample {
            pid: row.try_get("pid")?,
            age_s: row.try_get("age_s")?,
        })
    }
}

/// Why a connection failed, as far as the driver's error can tell us.
///
/// What the operator has to do differs completely between "nothing is
/// listening", "the server rejected who I am" and "that database is not
/// there", so the message has to read the error. Classifying only by
/// target kind (socket vs TCP) would tell a stopped cluster to check
/// peer authentication and re-run as root.
#[derive(Debug, PartialEq, Eq)]
enum ConnectFailure {
    /// Nothing is accepting connections: no socket file, or the port refused
    /// the connection. For a socket target this is almost always a stopped
    /// cluster.
    Unreachable,
    /// The server answered and refused the identity (SQLSTATE class 28).
    Rejected,
    /// The server answered; that database does not exist (SQLSTATE 3D000).
    NoSuchDatabase,
    /// Anything else — say less rather than guess.
    Other,
}

fn classify(error: &sqlx::Error) -> ConnectFailure {
    match error {
        // A missing socket file is `NotFound`; a dead listener on a TCP port
        // is `ConnectionRefused`. Both mean "there is nothing there", which
        // no amount of credentials can fix.
        sqlx::Error::Io(io) => match io.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ConnectFailure::Unreachable
            }
            _ => ConnectFailure::Other,
        },
        // The server replied, so it is running: the SQLSTATE says why it
        // said no. Class 28 is invalid authorization; 3D000 is an unknown
        // database.
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some(code) if code.starts_with("28") => ConnectFailure::Rejected,
            Some("3D000") => ConnectFailure::NoSuchDatabase,
            _ => ConnectFailure::Other,
        },
        _ => ConnectFailure::Other,
    }
}

/// Rewrite the libpq-only spelling `scheme://user[:pass]@/db?host=…` —
/// userinfo over an empty host, which psql accepts for socket connections —
/// into the equivalent URI sqlx's parser (the WHATWG `url` crate, which
/// rejects userinfo without a host) understands, by moving the credentials
/// into `user=`/`password=` query parameters. Every other shape passes
/// through untouched: this is a compatibility shim for one documented libpq
/// form, not a second URI parser.
fn normalize_database_url(url: &str) -> std::borrow::Cow<'_, str> {
    let pass = std::borrow::Cow::Borrowed(url);
    let Some((scheme, rest)) = url.split_once("://") else {
        return pass;
    };
    if !matches!(scheme, "postgres" | "postgresql") {
        return pass;
    }
    // A fragment is never meaningful in a connection URI; leave the string
    // for the real parser to reject with its own message.
    if rest.contains('#') {
        return pass;
    }
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    // The last '@' separates userinfo from host (RFC 3986); only the
    // empty-host case needs rewriting.
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return pass;
    };
    if !host.is_empty() || userinfo.is_empty() {
        return pass;
    }
    let (user, password) = match userinfo.split_once(':') {
        Some((user, password)) => (user, Some(password)),
        None => (userinfo, None),
    };
    // The moved text keeps its percent-encoding — query values are decoded
    // the same way as userinfo — except '+', which only a query reads as a
    // space.
    let escape = |s: &str| s.replace('+', "%2B");
    let mut out = String::with_capacity(url.len() + 16);
    out.push_str(scheme);
    out.push_str("://");
    if !tail.starts_with('/') {
        // `postgres:///?host=…` is the shape sqlx documents for an empty
        // host; keep the path slash so the query is unambiguous.
        out.push('/');
    }
    out.push_str(tail);
    out.push(if tail.contains('?') { '&' } else { '?' });
    out.push_str("user=");
    out.push_str(&escape(user));
    if let Some(password) = password {
        out.push_str("&password=");
        out.push_str(&escape(password));
    }
    std::borrow::Cow::Owned(out)
}

fn connect_hint(target: &DbTarget, error: &sqlx::Error) -> String {
    let failure = classify(error);
    match (target, failure) {
        (DbTarget::Socket { dir, port, .. }, ConnectFailure::Unreachable) => format!(
            "nothing is listening on {} port {port} — the cluster is almost certainly not \
             running. Start it (`sudo pg_ctlcluster <major> <name> start`) and retry; \
             `pg_lsclusters` shows the status of each one",
            dir.display()
        ),
        (DbTarget::Url(_), ConnectFailure::Unreachable) => {
            "nothing is listening at that host and port — check the server is running and \
             reachable, and that the port in the URI is right"
                .to_string()
        }
        // Only now is authentication the story: the server answered.
        (DbTarget::Socket { user, .. }, ConnectFailure::Rejected) => format!(
            "the connection uses peer authentication as OS user {user:?}; run the CLI as root \
             (it drops privileges automatically) or as {user:?}, or pass --database-url"
        ),
        (DbTarget::Url(_), ConnectFailure::Rejected) => {
            "the server rejected the credentials in the supplied URI".to_string()
        }
        (_, ConnectFailure::NoSuchDatabase) => {
            "the server is running but has no such database; create it, or name an existing \
             one — `postvec setup --database <name>` creates it when missing"
                .to_string()
        }
        (DbTarget::Socket { user, .. }, ConnectFailure::Other) => format!(
            "the connection uses peer authentication as OS user {user:?}; check the cluster is \
             running and accepting connections, or pass --database-url"
        ),
        (DbTarget::Url(_), ConnectFailure::Other) => {
            "check the host, port, database, credentials and sslmode in the supplied URI"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_targets_build_peer_authenticated_options() {
        let direct = Direct::new(
            DbTarget::Socket {
                dir: "/var/run/postgresql".into(),
                port: 5433,
                user: "postgres".into(),
            },
            None,
        );
        let options = direct.options("univec").unwrap();
        assert_eq!(options.get_port(), 5433);
        assert_eq!(options.get_username(), "postgres");
        assert_eq!(options.get_database(), Some("univec"));
    }

    #[test]
    fn url_targets_keep_their_parameters_but_switch_database() {
        let direct = Direct::new(
            DbTarget::Url("postgres://alice:s3cret@db.example:6000/other?sslmode=require".into()),
            None,
        );
        let options = direct.options("univec").unwrap();
        assert_eq!(options.get_database(), Some("univec"));
        assert_eq!(options.get_port(), 6000);
        assert_eq!(options.get_username(), "alice");
        assert!(
            matches!(options.get_ssl_mode(), sqlx::postgres::PgSslMode::Require),
            "a requested TLS mode must never be downgraded"
        );
    }

    #[test]
    fn the_libpq_empty_host_spelling_is_accepted() {
        // psql accepts `postgresql://app@/demo?host=/dir`; sqlx's URL parser
        // does not. The CLI must take the spelling operators already know.
        let direct = Direct::new(
            DbTarget::Url("postgresql://app@/other?host=/var/run/postgresql".into()),
            None,
        );
        let options = direct.options("demo").unwrap();
        assert_eq!(options.get_username(), "app");
        assert_eq!(options.get_database(), Some("demo"));
        assert_eq!(
            options.get_socket(),
            Some(&std::path::PathBuf::from("/var/run/postgresql"))
        );
    }

    #[test]
    fn empty_host_normalization_moves_the_password_too() {
        let direct = Direct::new(
            DbTarget::Url("postgres://alice:s3cret@/db?host=/tmp".into()),
            None,
        );
        let options = direct.options("db").unwrap();
        assert_eq!(options.get_username(), "alice");
        assert_eq!(
            options.get_socket(),
            Some(&std::path::PathBuf::from("/tmp"))
        );
    }

    #[test]
    fn urls_with_a_real_host_are_not_rewritten() {
        for url in [
            "postgres://alice:s3cret@db.example:6000/other?sslmode=require",
            "postgres://db.example/x",
            "postgres:///x?host=/tmp",
            "not a uri at all",
            "mysql://a@/b",
        ] {
            assert_eq!(normalize_database_url(url), url);
        }
    }

    #[test]
    fn a_malformed_url_is_a_usage_error_without_echoing_the_secret() {
        let direct = Direct::new(DbTarget::Url("not a uri at all".into()), None);
        let err = direct.options("d").unwrap_err();
        assert_eq!(err.exit(), crate::error::Exit::Usage);
        assert!(err.to_string().contains("--database-url"));
    }

    fn socket() -> DbTarget {
        DbTarget::Socket {
            dir: "/var/run/postgresql".into(),
            port: 5432,
            user: "postgres".into(),
        }
    }

    /// sqlx's own `PgDatabaseError` cannot be constructed outside the crate,
    /// so a server-side refusal is stood up through the public trait. Only
    /// `code()` is read by `classify`; the rest is the trait's minimum.
    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "SQLSTATE {}", self.0)
        }
    }
    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "refused"
        }
        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(std::borrow::Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db_error(sqlstate: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(sqlstate)))
    }

    /// 28000 and 28P01 are both invalid-authorization; matching the class
    /// rather than the exact code covers both without a list to maintain.
    fn auth_error() -> sqlx::Error {
        db_error("28P01")
    }

    #[test]
    fn sqlstates_are_classified_by_class() {
        assert_eq!(classify(&db_error("28000")), ConnectFailure::Rejected);
        assert_eq!(classify(&db_error("28P01")), ConnectFailure::Rejected);
        assert_eq!(classify(&db_error("3D000")), ConnectFailure::NoSuchDatabase);
        assert_eq!(classify(&db_error("42601")), ConnectFailure::Other);
    }

    /// A missing database is actionable in a way neither of the others is.
    #[test]
    fn a_missing_database_says_so() {
        let hint = connect_hint(&socket(), &db_error("3D000"));
        assert!(hint.contains("no such database"), "{hint}");
        assert!(!hint.contains("peer authentication"), "{hint}");
    }

    /// The server answered and refused the identity: peer authentication is
    /// genuinely the story, so the original advice still applies.
    #[test]
    fn a_rejected_identity_explains_peer_authentication() {
        let hint = connect_hint(&socket(), &auth_error());
        assert!(hint.contains("peer authentication"), "{hint}");
        assert!(hint.contains("postgres"), "{hint}");
    }

    /// A stopped cluster produced exactly the wrong advice before: no amount
    /// of re-running as root creates a socket that is not there.
    #[test]
    fn an_absent_socket_says_the_cluster_is_not_running() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
        ] {
            let hint = connect_hint(
                &socket(),
                &sqlx::Error::Io(std::io::Error::new(kind, "no such file or directory")),
            );
            assert!(hint.contains("not running"), "{kind:?}: {hint}");
            assert!(hint.contains("pg_lsclusters"), "{kind:?}: {hint}");
            assert!(
                !hint.contains("peer authentication"),
                "{kind:?} must not be blamed on authentication: {hint}"
            );
        }
    }

    /// A URL target gets the same distinction, in its own vocabulary.
    #[test]
    fn a_url_target_distinguishes_unreachable_from_rejected() {
        let unreachable = connect_hint(
            &DbTarget::Url("postgres://db.example/x".into()),
            &sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            )),
        );
        assert!(
            unreachable.contains("nothing is listening"),
            "{unreachable}"
        );
        let rejected = connect_hint(
            &DbTarget::Url("postgres://db.example/x".into()),
            &auth_error(),
        );
        assert!(rejected.contains("rejected the credentials"), "{rejected}");
    }

    /// An unclassifiable error must not be dressed up as a diagnosis.
    #[test]
    fn an_unclassified_error_stays_general() {
        assert_eq!(classify(&sqlx::Error::PoolTimedOut), ConnectFailure::Other);
        let hint = connect_hint(&socket(), &sqlx::Error::PoolTimedOut);
        assert!(hint.contains("--database-url"), "{hint}");
    }

    #[test]
    fn io_kinds_are_classified() {
        assert_eq!(
            classify(&sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "gone"
            ))),
            ConnectFailure::Unreachable
        );
        assert_eq!(
            classify(&sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "nope"
            ))),
            ConnectFailure::Other
        );
    }
}
