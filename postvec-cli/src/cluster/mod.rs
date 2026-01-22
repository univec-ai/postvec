//! Cluster discovery, offline configuration validation, and restart.
//!
//! Two host shapes are supported, in decreasing order of what the CLI is
//! willing to do automatically:
//!
//! - [`debian`]: a `postgresql-common` cluster (`pg_lsclusters`). Known
//!   `conf.d`, known owner, known service unit — the CLI can write
//!   configuration and restart.
//! - [`explicit`]: selected with `--pg-config`. The CLI can inspect, and can
//!   write configuration when given an already-included `--config-dir`, but it
//!   will not invent a restart command for a service it cannot identify.

pub mod debian;
pub mod explicit;

use crate::config::owned::OwnedPaths;
use crate::db::Db;
use crate::error::{CliError, Result};
use crate::facts::{AssetFacts, ClusterIdentity, ServerFacts};
use crate::proc::{self, Cmd, OsAccount};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// PostgreSQL majors postvec builds for (`postvec/Cargo.toml` features
/// `pg16`/`pg17`/`pg18`).
pub const SUPPORTED_MAJORS: [u32; 3] = [16, 17, 18];

pub fn is_supported_major(major: u32) -> bool {
    SUPPORTED_MAJORS.contains(&major)
}

/// How the cluster was selected — it determines what the CLI may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ClusterKind {
    Debian,
    Explicit,
    /// Reached only through `--database-url`, with no local installation to
    /// inspect. Everything filesystem- and service-shaped is unobservable and
    /// is reported as SKIP rather than guessed at.
    Remote,
}

/// The command that restarts this cluster. Always an argv array; never a
/// string handed to a shell.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RestartCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// What to call it in prompts, e.g. `postgresql@18-main.service`.
    pub label: String,
}

impl RestartCommand {
    fn cmd(&self) -> Cmd {
        Cmd::new(&self.program).args(self.args.clone())
    }

    pub fn display(&self) -> String {
        self.cmd().display()
    }
}

/// A discovered cluster: everything the CLI needs to know about the host side.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub identity: ClusterIdentity,
    pub kind: ClusterKind,
    /// The account PostgreSQL runs as. `None` when it could not be determined,
    /// in which case database work runs with the caller's own identity.
    pub owner: Option<OsAccount>,
    pub pg_config: PathBuf,
    pub bindir: PathBuf,
    pub sharedir: PathBuf,
    pub pkglibdir: PathBuf,
    /// `pg_config --version`'s major.
    pub binary_major: u32,
    /// The `conf.d`-style directory the CLI may own, when one is known to be
    /// included by `postgresql.conf`.
    pub conf_dir: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub port: u16,
    pub socket_dir: PathBuf,
    pub running: bool,
    pub restart: Option<RestartCommand>,
}

impl Cluster {
    pub fn owned_paths(&self) -> Result<OwnedPaths> {
        let conf_dir = self.conf_dir.as_ref().ok_or_else(|| {
            CliError::precondition(format!(
                "no configuration directory is known for cluster {}",
                self.identity.id
            ))
            .with_fix(
                "for a postgresql-common cluster this is conf.d; otherwise pass --pg-config \
                 with --config-dir naming a directory already included by postgresql.conf",
            )
        })?;
        Ok(OwnedPaths::new(conf_dir, &self.identity.key()))
    }

    /// The `postgres` server binary, used for offline configuration parsing.
    pub fn postgres_binary(&self) -> PathBuf {
        self.bindir.join("postgres")
    }

    /// Locate the installed extension assets. Pure filesystem inspection.
    ///
    /// `None` when there is no local installation to look at, so the checks
    /// that depend on it can skip instead of reporting everything as missing.
    pub fn assets(&self) -> Option<AssetFacts> {
        if self.kind == ClusterKind::Remote {
            return None;
        }
        Some(self.local_assets())
    }

    fn local_assets(&self) -> AssetFacts {
        let extension_dir = self.sharedir.join("extension");
        let mut sql_versions = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&extension_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // `postvec--0.1.0.sql` is an install script; `postvec--a--b.sql`
                // is an upgrade script and says nothing about what can be
                // installed from scratch.
                if let Some(rest) = name
                    .strip_prefix("postvec--")
                    .and_then(|r| r.strip_suffix(".sql"))
                {
                    if !rest.contains("--") {
                        sql_versions.push(rest.to_string());
                    }
                }
            }
        }
        sql_versions.sort();
        AssetFacts {
            sharedir: self.sharedir.clone(),
            pkglibdir: self.pkglibdir.clone(),
            postvec_control: existing(extension_dir.join("postvec.control")),
            postvec_sql_versions: sql_versions,
            postvec_library: first_existing(&[
                self.pkglibdir.join("postvec.so"),
                self.pkglibdir.join("postvec.dylib"),
            ]),
            vector_control: existing(extension_dir.join("vector.control")),
            vector_library: first_existing(&[
                self.pkglibdir.join("vector.so"),
                self.pkglibdir.join("vector.dylib"),
            ]),
        }
    }

    /// Query the candidate configuration with `postgres -C`, without touching
    /// the running server.
    ///
    /// This is how the CLI proves that its `conf.d` snippet is included *and*
    /// wins precedence: `postgres -C` applies the same file resolution the
    /// postmaster does, including `postgresql.auto.conf` (which `ALTER SYSTEM`
    /// writes and which is read *after* `include_dir`, so it can shadow the
    /// snippet).
    ///
    /// `pg_reload_conf()` is deliberately not used for this: it would apply
    /// SIGHUP-context postvec settings to a running deployment as a side effect
    /// of a validation step.
    pub async fn query_setting_offline(
        &self,
        name: &str,
        timeout: Duration,
    ) -> Result<OfflineSetting> {
        if self.kind == ClusterKind::Remote {
            return Ok(OfflineSetting::NotObservable(
                "there is no local PostgreSQL installation to parse the configuration with"
                    .to_string(),
            ));
        }
        let (Some(data_dir), Some(config_file)) = (&self.data_dir, &self.config_file) else {
            return Ok(OfflineSetting::NotObservable(
                "the server's data_directory and config_file are not readable by this role"
                    .to_string(),
            ));
        };
        let cmd = Cmd::new(self.postgres_binary())
            .arg("-D")
            .arg(data_dir.display().to_string())
            .arg("-C")
            .arg(name)
            .arg("-c")
            .arg(format!("config_file={}", config_file.display()))
            .run_as(self.owner.as_ref());
        let output = proc::run(&cmd, timeout).await?;
        if output.ok() {
            Ok(OfflineSetting::Value(output.first_line().to_string()))
        } else if output
            .stderr
            .contains("unrecognized configuration parameter")
        {
            // A `postvec.*` setting nobody has written: the placeholder does
            // not exist, which is itself the answer.
            Ok(OfflineSetting::Unset)
        } else {
            Ok(OfflineSetting::Error(output.failure_detail()))
        }
    }

    /// Restart the cluster and prove a new postmaster is serving.
    ///
    /// A successful reconnect alone is not proof: the command may have only
    /// reloaded, or targeted a different cluster. `pg_postmaster_start_time()`
    /// must have moved forward.
    pub async fn restart_and_verify(
        &self,
        db: &mut Db,
        previous: &ServerFacts,
        deadline: Duration,
    ) -> Result<ServerFacts> {
        let restart = self.restart.as_ref().ok_or_else(|| {
            CliError::precondition(format!(
                "no restart command is known for cluster {}",
                self.identity.id
            ))
            .with_fix("rerun with --no-restart and restart the service yourself")
        })?;
        proc::run_ok(&restart.cmd(), deadline).await.map_err(|e| {
            CliError::apply(format!("restarting {} failed: {e}", restart.label))
                .with_fix("inspect the service's status and the PostgreSQL log")
        })?;
        self.wait_for_new_postmaster(db, previous, deadline).await
    }

    /// Poll until a postmaster started after `previous` answers.
    pub async fn wait_for_new_postmaster(
        &self,
        db: &mut Db,
        previous: &ServerFacts,
        deadline: Duration,
    ) -> Result<ServerFacts> {
        let started = Instant::now();
        let mut backoff = Duration::from_millis(200);
        let mut last_error = None;
        while started.elapsed() < deadline {
            // The pooled connections died with the old postmaster.
            let _ = db.reconnect().await;
            match db.server_facts().await {
                Ok(facts) if facts.postmaster_start_time != previous.postmaster_start_time => {
                    return Ok(facts)
                }
                Ok(_) => {
                    last_error = Some(
                        "the server answered but its start time did not change; the restart \
                         command may have only reloaded, or targeted another cluster"
                            .to_string(),
                    )
                }
                Err(e) => last_error = Some(e.to_string()),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
        Err(CliError::apply(format!(
            "cluster {} did not come back within {}: {}",
            self.identity.id,
            humantime::format_duration(deadline),
            last_error.unwrap_or_else(|| "no further detail".to_string())
        ))
        .with_fix(format!(
            "check the service status and the PostgreSQL log; the configuration written by \
             this command is still in place at {}",
            self.conf_dir
                .as_ref()
                .map(|d| d.join(crate::config::OWNED_FILE_NAME).display().to_string())
                .unwrap_or_else(|| "the owned snippet".to_string())
        )))
    }
}

/// Result of an offline `postgres -C` query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfflineSetting {
    Value(String),
    /// The setting is not set anywhere (custom placeholder absent).
    Unset,
    /// The configuration could not be parsed.
    Error(String),
    /// The CLI lacks the access needed to ask.
    NotObservable(String),
}

/// A cluster known only through a supplied connection URI.
///
/// `doctor --database-url …` must still work on a host with no local
/// PostgreSQL installation — a workstation, a container, a CI runner pointed at
/// a managed server. Everything that needs the host is then unobservable, which
/// the checks report as SKIP.
pub fn remote_only(server: &ServerFacts) -> Cluster {
    Cluster {
        identity: ClusterIdentity {
            id: "remote".to_string(),
            major: server.major(),
            name: "remote".to_string(),
        },
        kind: ClusterKind::Remote,
        owner: None,
        pg_config: PathBuf::new(),
        bindir: PathBuf::new(),
        sharedir: PathBuf::new(),
        pkglibdir: PathBuf::new(),
        binary_major: server.major(),
        conf_dir: None,
        config_file: server.config_file.as_ref().map(PathBuf::from),
        data_dir: server.data_directory.as_ref().map(PathBuf::from),
        port: u16::try_from(server.port).unwrap_or(5432),
        socket_dir: PathBuf::new(),
        running: true,
        restart: None,
    }
}

/// Discover the cluster the command should act on.
pub async fn discover(
    cluster: Option<&str>,
    pg_config: Option<&Path>,
    config_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Cluster> {
    discover_with(cluster, pg_config, config_dir, timeout, false).await
}

/// Discovery for **read-only diagnosis**, which may select a single supported
/// cluster that is not running.
///
/// Only `doctor` uses this. Every mutating command goes through [`discover`],
/// which still requires an online cluster, so nothing that changes a system can
/// reach a stopped one by accident.
pub async fn discover_allowing_offline(
    cluster: Option<&str>,
    pg_config: Option<&Path>,
    config_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Cluster> {
    discover_with(cluster, pg_config, config_dir, timeout, true).await
}

async fn discover_with(
    cluster: Option<&str>,
    pg_config: Option<&Path>,
    config_dir: Option<&Path>,
    timeout: Duration,
    allow_offline: bool,
) -> Result<Cluster> {
    match pg_config {
        Some(path) => explicit::discover(path, config_dir, timeout).await,
        None => {
            if config_dir.is_some() {
                return Err(CliError::usage(
                    "--config-dir is only meaningful with --pg-config",
                ));
            }
            debian::discover(cluster, timeout, allow_offline).await
        }
    }
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

/// Parse `pg_config --version` output, e.g.
/// `PostgreSQL 18.4 (Ubuntu 18.4-1.pgdg22.04+1)`.
pub fn parse_pg_config_major(version: &str) -> Option<u32> {
    let rest = version.split_whitespace().nth(1)?;
    rest.split('.')
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

/// Run `pg_config` once for the three directories and the version.
pub(crate) async fn read_pg_config(
    pg_config: &Path,
    timeout: Duration,
) -> Result<(PathBuf, PathBuf, PathBuf, u32)> {
    let output = proc::run_ok(
        &Cmd::new(pg_config)
            .arg("--bindir")
            .arg("--sharedir")
            .arg("--pkglibdir")
            .arg("--version"),
        timeout,
    )
    .await?;
    let lines: Vec<&str> = output.stdout.lines().map(str::trim).collect();
    if lines.len() < 4 {
        return Err(CliError::precondition(format!(
            "{} did not report bindir, sharedir, pkglibdir and version",
            pg_config.display()
        )));
    }
    let major = parse_pg_config_major(lines[3]).ok_or_else(|| {
        CliError::precondition(format!(
            "cannot read a PostgreSQL major version from {:?}",
            lines[3]
        ))
    })?;
    Ok((
        PathBuf::from(lines[0]),
        PathBuf::from(lines[1]),
        PathBuf::from(lines[2]),
        major,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_majors_match_the_extensions_feature_matrix() {
        assert!(is_supported_major(16));
        assert!(is_supported_major(17));
        assert!(is_supported_major(18));
        assert!(!is_supported_major(15));
        assert!(!is_supported_major(19));
    }

    #[test]
    fn pg_config_version_parsing() {
        assert_eq!(
            parse_pg_config_major("PostgreSQL 18.4 (Ubuntu 18.4-1.pgdg22.04+1)"),
            Some(18)
        );
        assert_eq!(parse_pg_config_major("PostgreSQL 16.9"), Some(16));
        assert_eq!(parse_pg_config_major("PostgreSQL 17beta1"), Some(17));
        assert_eq!(parse_pg_config_major("nonsense"), None);
        assert_eq!(parse_pg_config_major(""), None);
    }

    #[test]
    fn assets_distinguish_install_scripts_from_upgrade_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let extension = dir.path().join("share").join("extension");
        std::fs::create_dir_all(&extension).unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        for name in [
            "postvec.control",
            "postvec--0.1.0.sql",
            "postvec--0.1.0--0.2.0.sql",
            "vector.control",
        ] {
            std::fs::write(extension.join(name), "").unwrap();
        }
        std::fs::write(lib.join("postvec.so"), "").unwrap();

        let cluster = Cluster {
            identity: ClusterIdentity {
                id: "18/main".into(),
                major: 18,
                name: "main".into(),
            },
            kind: ClusterKind::Debian,
            owner: None,
            pg_config: PathBuf::from("/usr/bin/pg_config"),
            bindir: PathBuf::from("/usr/lib/postgresql/18/bin"),
            sharedir: dir.path().join("share"),
            pkglibdir: lib,
            binary_major: 18,
            conf_dir: None,
            config_file: None,
            data_dir: None,
            port: 5432,
            socket_dir: PathBuf::from("/var/run/postgresql"),
            running: true,
            restart: None,
        };
        let assets = cluster.assets().expect("a local cluster has assets");
        assert_eq!(assets.postvec_sql_versions, ["0.1.0"]);
        assert!(assets.postvec_control.is_some());
        assert!(assets.postvec_library.is_some());
        assert!(assets.vector_control.is_some());
        assert!(assets.vector_library.is_none());
    }

    #[test]
    fn restart_commands_render_as_argv() {
        let restart = RestartCommand {
            program: PathBuf::from("/usr/bin/systemctl"),
            args: vec!["restart".into(), "postgresql@18-main.service".into()],
            label: "postgresql@18-main.service".into(),
        };
        assert_eq!(
            restart.display(),
            "/usr/bin/systemctl restart postgresql@18-main.service"
        );
    }
}
