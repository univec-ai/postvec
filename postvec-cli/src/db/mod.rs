//! Database access, and the privilege boundary around it.
//!
//! The CLI normally runs as root (it writes `/etc/postgresql`), while local
//! `peer` authentication expects the cluster owner's OS identity. Rather than
//! guess a password or shell out to `psql`, database work is executed by a
//! short-lived child of the same binary that has irreversibly dropped to the
//! cluster owner ([`agent`]).
//!
//! Both sides speak the same [`DbRequest`]/reply protocol, and the child's
//! implementation *is* [`local::execute`] — so there is exactly one
//! implementation of every query, whichever identity runs it.

pub mod agent;
pub mod local;
pub mod sql;

use crate::error::{CliError, Result};
use crate::facts::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// How to reach the cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbTarget {
    /// Unix socket with peer authentication; the identity is the process's own.
    Socket {
        dir: PathBuf,
        port: u16,
        user: String,
    },
    /// An operator-supplied URI. Used verbatim except for the database name,
    /// which is always set through the driver's option rather than by string
    /// surgery.
    Url(String),
}

impl DbTarget {
    /// A description safe to print: a URI's userinfo and parameters never
    /// appear.
    pub fn describe(&self) -> String {
        match self {
            DbTarget::Socket { dir, port, user } => {
                format!("socket {} port {port} as {user}", dir.display())
            }
            DbTarget::Url(url) => match url::Url::parse(url) {
                Ok(u) => format!(
                    "{}://{}{}",
                    u.scheme(),
                    u.host_str().unwrap_or("?"),
                    u.port().map(|p| format!(":{p}")).unwrap_or_default()
                ),
                Err(_) => "supplied connection URI".to_string(),
            },
        }
    }
}

/// The database name the CLI connects to for cluster-wide work. `postgres`
/// exists in virtually every cluster; `template1` is the documented fallback.
pub const MAINTENANCE_DATABASES: [&str; 2] = ["postgres", "template1"];

/// What the client requires of the extension it just installed, checked before
/// the installing transaction commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionExpectation {
    /// Embedded mode needs an `embedded`-featured library; committing a thin
    /// one would produce a cluster that parks its worker on every restart.
    pub require_embedded_build: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallOutcome {
    /// False when the extension was already present (an idempotent rerun).
    pub created: bool,
    pub catalog_version: String,
    pub library_version: String,
    pub build_info: Option<BuildInfo>,
    pub vector_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UninstallOutcome {
    /// False when the extension was already absent (an idempotent rerun).
    pub was_present: bool,
    /// Registry entries `postvec.uninstall()` tore down.
    pub cleaned_entries: i64,
    /// Chunk destinations (`schema.table`) that were asked to be dropped but
    /// survived: `postvec.uninstall()` keeps a destination whose ownership
    /// proof fails and only warns, so the CLI checks the catalog afterwards
    /// rather than trusting a count.
    #[serde(default)]
    pub retained_destinations: Vec<String>,
}

/// One `pg_database` row, for `uninstall --all`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseListing {
    pub name: String,
    pub allow_conn: bool,
    pub is_template: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatSample {
    pub pid: Option<i32>,
    pub age_s: Option<f64>,
}

/// One unit of database work. Each variant is self-contained: any transaction
/// a variant needs begins and ends inside it, so the protocol never has to
/// keep a transaction open across a round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum DbRequest {
    /// Server version and paths. Also proves the connection works.
    ServerFacts,
    /// `pg_settings` rows for the named settings.
    Settings { names: Vec<String> },
    /// `pg_file_settings` rows for the named settings, plus every row with a
    /// parse error.
    FileSettings { names: Vec<String> },
    /// `CREATE DATABASE`. Cannot run inside a transaction.
    CreateDatabase { database: String },
    /// Everything `doctor` needs from inside one database.
    InspectDatabase { database: String },
    /// `CREATE EXTENSION ... CASCADE` plus in-transaction capability
    /// verification.
    InstallExtension {
        database: String,
        expect: ExtensionExpectation,
    },
    /// `postvec.uninstall()` plus `DROP EXTENSION`, in one transaction.
    UninstallExtension {
        database: String,
        drop_columns: bool,
        #[serde(default)]
        drop_destinations: bool,
        lock_timeout_ms: u32,
    },
    /// Every database in the cluster, templates and non-connectable ones
    /// included — the caller decides what to do about those.
    ListDatabases,
    /// `postvec.refresh_models()`. Mutating: `setup` smoke checks only.
    RefreshModels { database: String },
    /// A single heartbeat reading, for advancement sampling.
    HeartbeatSample { database: String },
    /// `SELECT pg_reload_conf()`. Applies SIGHUP-context settings without an
    /// outage. Mutating: never issued by `doctor`, which uses `postgres -C` to
    /// read a candidate configuration instead.
    ReloadConfig { database: String },
    /// Drop every cached connection. Needed after a restart: the pooled
    /// connections are dead, and reusing one would report the old postmaster.
    Reconnect,
}

impl DbRequest {
    /// Whether this request writes. Used to assert, in one place, that
    /// `doctor` never issues a mutating request.
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            DbRequest::CreateDatabase { .. }
                | DbRequest::InstallExtension { .. }
                | DbRequest::UninstallExtension { .. }
                | DbRequest::RefreshModels { .. }
                | DbRequest::ReloadConfig { .. }
        )
    }
}

/// Reply envelope. Errors cross the pipe as text because the child cannot
/// meaningfully reconstruct a driver error type on the other side, and because
/// the message has already been redacted.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DbReply {
    Ok(serde_json::Value),
    Err { message: String, kind: String },
}

/// Either an in-process connection or a privilege-dropped child.
pub enum Db {
    Direct(local::Direct),
    Agent(agent::AgentClient),
}

impl Db {
    async fn request<T: DeserializeOwned>(&mut self, req: DbRequest) -> Result<T> {
        let value = match self {
            Db::Direct(direct) => direct.execute(req).await?,
            Db::Agent(client) => client.execute(req).await?,
        };
        serde_json::from_value(value)
            .map_err(|e| CliError::internal(format!("unexpected database reply shape: {e}")))
    }

    pub async fn server_facts(&mut self) -> Result<ServerFacts> {
        self.request(DbRequest::ServerFacts).await
    }

    pub async fn settings(&mut self, names: &[&str]) -> Result<SettingsSnapshot> {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let rows: Vec<SettingRow> = self
            .request(DbRequest::Settings {
                names: names.clone(),
            })
            .await?;
        // pg_file_settings needs elevated privileges; a report without it is
        // still useful, so its absence degrades rather than fails.
        let file_rows: Vec<FileSettingRow> = self
            .request(DbRequest::FileSettings { names })
            .await
            .unwrap_or_default();
        Ok(SettingsSnapshot { rows, file_rows })
    }

    pub async fn create_database(&mut self, database: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(DbRequest::CreateDatabase {
                database: database.to_string(),
            })
            .await?;
        Ok(())
    }

    pub async fn inspect_database(&mut self, database: &str) -> Result<DatabaseFacts> {
        self.request(DbRequest::InspectDatabase {
            database: database.to_string(),
        })
        .await
    }

    pub async fn install_extension(
        &mut self,
        database: &str,
        expect: ExtensionExpectation,
    ) -> Result<InstallOutcome> {
        self.request(DbRequest::InstallExtension {
            database: database.to_string(),
            expect,
        })
        .await
    }

    pub async fn uninstall_extension(
        &mut self,
        database: &str,
        drop_columns: bool,
        drop_destinations: bool,
        lock_timeout: Duration,
    ) -> Result<UninstallOutcome> {
        self.request(DbRequest::UninstallExtension {
            database: database.to_string(),
            drop_columns,
            drop_destinations,
            lock_timeout_ms: lock_timeout.as_millis().min(u32::MAX as u128) as u32,
        })
        .await
    }

    pub async fn list_databases(&mut self) -> Result<Vec<DatabaseListing>> {
        self.request(DbRequest::ListDatabases).await
    }

    pub async fn reload_config(&mut self, database: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(DbRequest::ReloadConfig {
                database: database.to_string(),
            })
            .await?;
        Ok(())
    }

    pub async fn refresh_models(&mut self, database: &str) -> Result<i64> {
        self.request(DbRequest::RefreshModels {
            database: database.to_string(),
        })
        .await
    }

    pub async fn heartbeat_sample(&mut self, database: &str) -> Result<HeartbeatSample> {
        self.request(DbRequest::HeartbeatSample {
            database: database.to_string(),
        })
        .await
    }

    /// Forget every cached connection, so the next request reconnects.
    pub async fn reconnect(&mut self) -> Result<()> {
        let _: serde_json::Value = self.request(DbRequest::Reconnect).await?;
        Ok(())
    }

    /// Close the connection (or reap the child) explicitly, so a failure to
    /// shut down cleanly is reported rather than silently ignored at drop.
    pub async fn close(self) {
        match self {
            Db::Direct(direct) => direct.close().await,
            Db::Agent(client) => client.close().await,
        }
    }
}

/// A read-only wrapper that refuses mutating requests.
///
/// `doctor` holds this instead of a [`Db`], which turns "doctor must not write"
/// from a review rule into a type-level one.
pub struct ReadOnlyDb<'a>(&'a mut Db);

impl ReadOnlyDb<'_> {
    pub fn new(db: &mut Db) -> ReadOnlyDb<'_> {
        ReadOnlyDb(db)
    }

    pub async fn inspect_database(&mut self, database: &str) -> Result<DatabaseFacts> {
        self.0.inspect_database(database).await
    }

    pub async fn heartbeat_sample(&mut self, database: &str) -> Result<HeartbeatSample> {
        self.0.heartbeat_sample(database).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutating_requests_are_classified() {
        assert!(!DbRequest::ServerFacts.is_mutating());
        assert!(!DbRequest::Reconnect.is_mutating());
        assert!(!DbRequest::InspectDatabase {
            database: "d".into()
        }
        .is_mutating());
        assert!(!DbRequest::HeartbeatSample {
            database: "d".into()
        }
        .is_mutating());
        assert!(DbRequest::CreateDatabase {
            database: "d".into()
        }
        .is_mutating());
        assert!(DbRequest::RefreshModels {
            database: "d".into()
        }
        .is_mutating());
        assert!(DbRequest::UninstallExtension {
            database: "d".into(),
            drop_columns: false,
            drop_destinations: false,
            lock_timeout_ms: 1000
        }
        .is_mutating());
        assert!(!DbRequest::ListDatabases.is_mutating());
    }

    #[test]
    fn requests_round_trip_through_the_protocol() {
        let req = DbRequest::InstallExtension {
            database: "univec".into(),
            expect: ExtensionExpectation {
                require_embedded_build: true,
            },
        };
        let encoded = serde_json::to_string(&req).unwrap();
        assert!(encoded.contains("\"op\":\"install-extension\""));
        let decoded: DbRequest = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            DbRequest::InstallExtension { ref database, .. } if database == "univec"
        ));
    }

    #[test]
    fn targets_describe_themselves_without_secrets() {
        let socket = DbTarget::Socket {
            dir: PathBuf::from("/var/run/postgresql"),
            port: 5432,
            user: "postgres".into(),
        };
        assert_eq!(
            socket.describe(),
            "socket /var/run/postgresql port 5432 as postgres"
        );
        let url = DbTarget::Url("postgres://alice:s3cret@db.example:5433/univec?x=1".into());
        let described = url.describe();
        assert_eq!(described, "postgres://db.example:5433");
        assert!(!described.contains("s3cret"));
        assert!(!described.contains("alice"));
    }
}
