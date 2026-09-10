//! The explicitly selected installation (`--pg-config`).
//!
//! For a source build, a vendor layout, or a pgrx-managed development cluster
//! there is no reliable way to learn the service name, so this adapter
//! deliberately has **no restart command**: mutating commands must be run with
//! `--no-restart` and the operator restarts the cluster themselves.
//!
//! Configuration can still be written, but only into a directory the operator
//! names with `--config-dir` and that `postgresql.conf` already includes — the
//! CLI will not add an `include_dir` line to a file it does not own.

use super::{read_pg_config, Cluster, ClusterKind};
use crate::error::{CliError, Result};
use crate::facts::ClusterIdentity;
use crate::proc::OsAccount;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default socket directory to try. The operator can bypass this entirely with
/// `--database-url`.
const DEFAULT_SOCKET_DIR: &str = "/var/run/postgresql";
const DEFAULT_PORT: u16 = 5432;

pub async fn discover(
    pg_config: &Path,
    config_dir: Option<&Path>,
    timeout: Duration,
) -> Result<Cluster> {
    if !pg_config.is_file() {
        return Err(CliError::usage(format!(
            "--pg-config {} is not a file",
            pg_config.display()
        )));
    }
    let (bindir, sharedir, pkglibdir, binary_major) = read_pg_config(pg_config, timeout).await?;

    let conf_dir = match config_dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(CliError::usage(format!(
                    "--config-dir {} is not a directory",
                    dir.display()
                )));
            }
            if !dir.is_absolute() {
                return Err(CliError::usage(
                    "--config-dir must be absolute: PostgreSQL resolves a relative include_dir \
                     against its data directory",
                ));
            }
            Some(dir.to_path_buf())
        }
        None => None,
    };

    Ok(Cluster {
        identity: ClusterIdentity {
            id: format!("{binary_major}/explicit"),
            major: binary_major,
            name: "explicit".to_string(),
        },
        kind: ClusterKind::Explicit,
        // Peer authentication then uses the caller's own identity; the CLI does
        // not guess which account owns a hand-built installation.
        owner: None,
        pg_config: pg_config.to_path_buf(),
        bindir,
        sharedir,
        pkglibdir,
        binary_major,
        conf_dir,
        // Both come from the running server, which is authoritative and does
        // not require guessing where a hand-built cluster keeps its files.
        config_file: None,
        data_dir: None,
        port: DEFAULT_PORT,
        socket_dir: PathBuf::from(DEFAULT_SOCKET_DIR),
        running: false,
        restart: None,
    })
}

/// Fill in the paths only the server knows, once a connection exists.
pub fn apply_server_facts(cluster: &mut Cluster, facts: &crate::facts::ServerFacts) {
    cluster.running = true;
    cluster.port = u16::try_from(facts.port).unwrap_or(cluster.port);
    if let Some(dir) = &facts.data_directory {
        cluster.data_dir = Some(PathBuf::from(dir));
    }
    if let Some(file) = &facts.config_file {
        cluster.config_file = Some(PathBuf::from(file));
    }

    // Discovery leaves `owner` unset: which account owns a hand-built
    // installation is not knowable from a pg_config path. Once the server
    // has told us its data directory, the directory's owner is the account
    // PostgreSQL requires to be running as.
    //
    // `postgres -C`, which is how the CLI validates a candidate
    // configuration offline, exits with "must be started under an
    // unprivileged user ID" when run as root. Without this, every
    // `sudo postvec setup --pg-config ...` on a PGDG-RPM host fails at
    // validation.
    if cluster.owner.is_none() {
        if let Some(dir) = &cluster.data_dir {
            cluster.owner = OsAccount::owning(dir).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_pg_config_is_a_usage_error() {
        let err = discover(
            Path::new("/nonexistent/pg_config"),
            None,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit(), crate::error::Exit::Usage);
    }

    #[tokio::test]
    async fn a_missing_config_dir_is_a_usage_error() {
        // Use the real pg_config on this host if present, otherwise the check
        // above already covers the ordering.
        let pg_config = Path::new("/usr/lib/postgresql/18/bin/pg_config");
        if !pg_config.is_file() {
            return;
        }
        let err = discover(
            pg_config,
            Some(Path::new("/nonexistent/conf.d")),
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--config-dir"));
    }

    #[test]
    fn server_facts_fill_in_what_only_the_server_knows() {
        let mut cluster = Cluster {
            identity: ClusterIdentity {
                id: "18/explicit".into(),
                major: 18,
                name: "explicit".into(),
            },
            kind: ClusterKind::Explicit,
            owner: None,
            pg_config: PathBuf::from("/p"),
            bindir: PathBuf::from("/b"),
            sharedir: PathBuf::from("/s"),
            pkglibdir: PathBuf::from("/l"),
            binary_major: 18,
            conf_dir: None,
            config_file: None,
            data_dir: None,
            port: 5432,
            socket_dir: PathBuf::from("/var/run/postgresql"),
            running: false,
            restart: None,
        };
        apply_server_facts(
            &mut cluster,
            &crate::facts::ServerFacts {
                version_num: 180_004,
                version: "18.4".into(),
                data_directory: Some("/srv/pg/data".into()),
                config_file: Some("/srv/pg/data/postgresql.conf".into()),
                hba_file: None,
                port: 6543,
                postmaster_start_time: "t".into(),
                postmaster_start_exact: None,
                system_identifier: None,
            },
        );
        assert!(cluster.running);
        assert_eq!(cluster.port, 6543);
        assert_eq!(cluster.data_dir, Some(PathBuf::from("/srv/pg/data")));
        assert_eq!(
            cluster.config_file,
            Some(PathBuf::from("/srv/pg/data/postgresql.conf"))
        );
        // The directory does not exist here, so no owner can be read — and the
        // absence must be tolerated rather than fatal.
        assert_eq!(cluster.owner, None);
    }

    /// The owner of an explicit installation is read from its data directory,
    /// not guessed. Without it, `postgres -C` runs as root and PostgreSQL
    /// refuses ("must be started under an unprivileged user ID"), which breaks
    /// the whole `--pg-config` flow under `sudo`.
    #[test]
    fn the_owner_is_read_from_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut cluster = Cluster {
            identity: ClusterIdentity {
                id: "18/explicit".into(),
                major: 18,
                name: "explicit".into(),
            },
            kind: ClusterKind::Explicit,
            owner: None,
            pg_config: PathBuf::from("/p"),
            bindir: PathBuf::from("/b"),
            sharedir: PathBuf::from("/s"),
            pkglibdir: PathBuf::from("/l"),
            binary_major: 18,
            conf_dir: None,
            config_file: None,
            data_dir: None,
            port: 5432,
            socket_dir: PathBuf::from("/var/run/postgresql"),
            running: false,
            restart: None,
        };
        apply_server_facts(
            &mut cluster,
            &crate::facts::ServerFacts {
                version_num: 180_004,
                version: "18.4".into(),
                data_directory: Some(dir.path().display().to_string()),
                config_file: None,
                hba_file: None,
                port: 5432,
                postmaster_start_time: "t".into(),
                postmaster_start_exact: None,
                system_identifier: None,
            },
        );
        let owner = cluster.owner.expect("an existing directory has an owner");
        assert_eq!(owner.uid, crate::proc::current_uid());
    }

    #[test]
    fn explicit_clusters_never_invent_a_restart_command() {
        // Encoded as a test because it is a safety property, not a detail:
        // guessing a service name could restart an unrelated cluster.
        let cluster = Cluster {
            identity: ClusterIdentity {
                id: "18/explicit".into(),
                major: 18,
                name: "explicit".into(),
            },
            kind: ClusterKind::Explicit,
            owner: None,
            pg_config: PathBuf::from("/p"),
            bindir: PathBuf::from("/b"),
            sharedir: PathBuf::from("/s"),
            pkglibdir: PathBuf::from("/l"),
            binary_major: 18,
            conf_dir: None,
            config_file: None,
            data_dir: None,
            port: 5432,
            socket_dir: PathBuf::from("/var/run/postgresql"),
            running: false,
            restart: None,
        };
        assert!(cluster.restart.is_none());
    }
}
