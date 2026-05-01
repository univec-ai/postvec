//! The `postgresql-common` (Debian/Ubuntu/PGDG) cluster adapter.
//!
//! This layout is the one the CLI can fully automate, because every piece it
//! needs is discoverable rather than guessed: `pg_lsclusters` reports the
//! version, name, port, status, owner and data directory; the configuration
//! lives at a fixed path with a `conf.d`; and there is a known service unit.

use super::{is_supported_major, read_pg_config, Cluster, ClusterKind, RestartCommand};
use crate::error::{CliError, Result};
use crate::facts::ClusterIdentity;
use crate::proc::{self, Cmd, OsAccount};
use std::path::{Path, PathBuf};
use std::time::Duration;

const PG_LSCLUSTERS: &str = "/usr/bin/pg_lsclusters";
const PG_CONFTOOL: &str = "/usr/bin/pg_conftool";
const PG_CTLCLUSTER: &str = "/usr/bin/pg_ctlcluster";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// systemd's own marker for "this is a systemd system".
const SYSTEMD_RUNTIME: &str = "/run/systemd/system";
/// postgresql-common's socket directory.
const DEFAULT_SOCKET_DIR: &str = "/var/run/postgresql";

/// One `pg_lsclusters` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterListing {
    pub major: u32,
    pub name: String,
    pub port: u16,
    pub status: String,
    pub owner: String,
    pub data_dir: PathBuf,
}

impl ClusterListing {
    pub fn id(&self) -> String {
        format!("{}/{}", self.major, self.name)
    }

    pub fn is_online(&self) -> bool {
        self.status.eq_ignore_ascii_case("online")
    }
}

/// Parse `pg_lsclusters --no-header` output.
///
/// Columns: Ver, Cluster, Port, Status, Owner, Data directory, Log file. The
/// status column can be multi-word (`down,binaries_missing`) but never contains
/// spaces, and paths may, so the tail is split from the right.
pub fn parse_lsclusters(stdout: &str) -> Vec<ClusterListing> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("Ver") {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            continue;
        }
        let Ok(major) = fields[0].split('.').next().unwrap_or("").parse::<u32>() else {
            continue;
        };
        let Ok(port) = fields[2].parse::<u16>() else {
            continue;
        };
        // The data directory is the second-to-last field when a log file is
        // present, and the last otherwise. Both are absolute paths, so take
        // the first path-looking field after the owner.
        let data_dir = fields[5..]
            .iter()
            .find(|f| f.starts_with('/'))
            .map(PathBuf::from)
            .unwrap_or_default();
        out.push(ClusterListing {
            major,
            name: fields[1].to_string(),
            port,
            status: fields[3].to_string(),
            owner: fields[4].to_string(),
            data_dir,
        });
    }
    out
}

/// Choose the cluster to act on.
///
/// Auto-selection happens only when exactly one supported cluster is online.
/// Anything else is a usage error that lists what was found — silently picking
/// the newest major would eventually reconfigure the wrong database server.
///
/// `allow_offline` relaxes *only* the online requirement, and only for
/// read-only diagnosis (`doctor`): a stopped cluster is the thing an operator
/// most wants diagnosed, and refusing to look at it is unhelpful when the
/// filesystem and `postgres -C` can still answer most of the question.
/// Ambiguity is never relaxed — with more than one candidate it is still a
/// usage error, because guessing which stopped server was meant is exactly the
/// mistake this function exists to prevent. Mutating commands always pass
/// `false`.
pub fn select<'a>(
    listings: &'a [ClusterListing],
    requested: Option<&str>,
    allow_offline: bool,
) -> Result<&'a ClusterListing> {
    let describe = |items: &[&ClusterListing]| {
        items
            .iter()
            .map(|c| format!("{} ({}, port {})", c.id(), c.status, c.port))
            .collect::<Vec<_>>()
            .join(", ")
    };

    if let Some(requested) = requested {
        let requested = requested.trim();
        return listings
            .iter()
            .find(|c| c.id() == requested)
            .ok_or_else(|| {
                let all: Vec<&ClusterListing> = listings.iter().collect();
                CliError::usage(format!(
                    "no cluster {requested:?}; found: {}",
                    if all.is_empty() {
                        "none".to_string()
                    } else {
                        describe(&all)
                    }
                ))
            });
    }

    let supported: Vec<&ClusterListing> = listings
        .iter()
        .filter(|c| is_supported_major(c.major))
        .collect();
    let online: Vec<&ClusterListing> = supported
        .iter()
        .copied()
        .filter(|c| c.is_online())
        .collect();
    match online.as_slice() {
        [only] => Ok(only),
        [] => {
            if supported.is_empty() {
                Err(CliError::usage(format!(
                    "no PostgreSQL {} cluster found{}",
                    SUPPORTED_MAJORS_TEXT,
                    if listings.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; pg_lsclusters reports: {}",
                            describe(&listings.iter().collect::<Vec<_>>())
                        )
                    }
                )))
            } else if allow_offline {
                // Read-only diagnosis of a stopped cluster: one candidate is
                // unambiguous, several are not.
                match supported.as_slice() {
                    [only] => Ok(only),
                    many => Err(CliError::usage(format!(
                        "several supported clusters exist and none is online: {}",
                        describe(many)
                    ))
                    .with_fix("choose one with --cluster <major>/<name>")),
                }
            } else {
                // With exactly one candidate the tool already knows which
                // cluster is meant, so it names it and the command that
                // starts it. A `<major>/<name>` placeholder makes the reader
                // go and look up what to substitute — for the overwhelmingly
                // common single-cluster host, that is a lookup the message
                // can just do for them.
                let fix = match supported.as_slice() {
                    [only] => format!(
                        "start it with `sudo pg_ctlcluster {} {} start`, or inspect it as it is \
                         with `--cluster {}` (commands that only read can work on a stopped \
                         cluster; anything needing a connection cannot)",
                        only.major,
                        only.name,
                        only.id()
                    ),
                    _ => "start one of them, or name it explicitly with --cluster <major>/<name>"
                        .to_string(),
                };
                Err(CliError::usage(format!(
                    "no supported cluster is online; found: {}",
                    describe(&supported)
                ))
                .with_fix(fix))
            }
        }
        many => Err(
            CliError::usage(format!("several clusters are online: {}", describe(many)))
                .with_fix("choose one with --cluster <major>/<name>"),
        ),
    }
}

const SUPPORTED_MAJORS_TEXT: &str = "16, 17 or 18";

/// Every cluster `pg_lsclusters` reports; empty when postgresql-common is
/// not installed (there is nothing to enumerate). **Strict**: a row that
/// does not parse is an error, because a caller deciding what is safe to
/// delete cannot claim to have examined every cluster otherwise.
pub async fn list_clusters(timeout: Duration) -> Result<Vec<ClusterListing>> {
    if !Path::new(PG_LSCLUSTERS).is_file() {
        return Ok(Vec::new());
    }
    let output = proc::run_ok(&Cmd::new(PG_LSCLUSTERS).arg("--no-header"), timeout).await?;
    parse_lsclusters_strict(&output.stdout)
}

/// [`parse_lsclusters`] that refuses instead of skipping a malformed row.
pub fn parse_lsclusters_strict(stdout: &str) -> Result<Vec<ClusterListing>> {
    let rows: Vec<&str> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("Ver"))
        .collect();
    let parsed = parse_lsclusters(stdout);
    if parsed.len() != rows.len() {
        return Err(CliError::precondition(format!(
            "pg_lsclusters reported {} row(s) but only {} could be parsed",
            rows.len(),
            parsed.len()
        ))
        .with_fix("inspect `pg_lsclusters` by hand; a malformed row hides a cluster"));
    }
    Ok(parsed)
}

pub async fn discover(
    requested: Option<&str>,
    timeout: Duration,
    allow_offline: bool,
) -> Result<Cluster> {
    if !Path::new(PG_LSCLUSTERS).is_file() {
        return Err(CliError::precondition(
            "this host has no pg_lsclusters, so its PostgreSQL layout cannot be discovered",
        )
        .with_fix(
            "install postgresql-common, or select the installation explicitly with \
             --pg-config <path> (and --config-dir for configuration changes)",
        ));
    }
    let output = proc::run_ok(&Cmd::new(PG_LSCLUSTERS).arg("--no-header"), timeout).await?;
    let listings = parse_lsclusters(&output.stdout);
    let listing = select(&listings, requested, allow_offline)?;

    if !is_supported_major(listing.major) {
        return Err(CliError::precondition(format!(
            "cluster {} runs PostgreSQL {}, but postvec supports {SUPPORTED_MAJORS_TEXT}",
            listing.id(),
            listing.major
        )));
    }

    let pg_config = PathBuf::from(format!(
        "/usr/lib/postgresql/{}/bin/pg_config",
        listing.major
    ));
    if !pg_config.is_file() {
        return Err(CliError::precondition(format!(
            "{} is missing, so PostgreSQL {}'s directories cannot be resolved",
            pg_config.display(),
            listing.major
        ))
        .with_fix(format!(
            "install postgresql-{} (or postgresql-server-dev-{})",
            listing.major, listing.major
        )));
    }
    let (bindir, sharedir, pkglibdir, binary_major) = read_pg_config(&pg_config, timeout).await?;

    let etc = PathBuf::from(format!(
        "/etc/postgresql/{}/{}",
        listing.major, listing.name
    ));
    let config_file = etc.join("postgresql.conf");
    let conf_dir = etc.join("conf.d");

    // A cluster whose owner account has vanished is broken, but reporting that
    // is doctor's job, not discovery's.
    let owner = OsAccount::lookup(&listing.owner).ok();

    Ok(Cluster {
        identity: ClusterIdentity {
            id: listing.id(),
            major: listing.major,
            name: listing.name.clone(),
        },
        kind: ClusterKind::Debian,
        owner,
        pg_config,
        bindir,
        sharedir,
        pkglibdir,
        binary_major,
        conf_dir: conf_dir.is_dir().then_some(conf_dir),
        config_file: config_file.is_file().then_some(config_file),
        data_dir: Some(listing.data_dir.clone()),
        port: listing.port,
        socket_dir: socket_dir(listing.major, &listing.name, timeout).await,
        running: listing.is_online(),
        restart: Some(restart_command(listing.major, &listing.name)),
    })
}

/// Prefer the cluster-specific systemd unit; fall back to `pg_ctlcluster` on
/// hosts without systemd (containers, mostly).
fn restart_command(major: u32, name: &str) -> RestartCommand {
    let unit = format!("postgresql@{major}-{name}.service");
    if Path::new(SYSTEMD_RUNTIME).is_dir() && Path::new(SYSTEMCTL).is_file() {
        RestartCommand {
            program: PathBuf::from(SYSTEMCTL),
            args: vec!["restart".to_string(), unit.clone()],
            label: unit,
        }
    } else {
        RestartCommand {
            program: PathBuf::from(PG_CTLCLUSTER),
            args: vec![
                major.to_string(),
                name.to_string(),
                "restart".to_string(),
                "--skip-systemctl-redirect".to_string(),
            ],
            label: format!("pg_ctlcluster {major} {name}"),
        }
    }
}

/// Read `unix_socket_directories` from the cluster's configuration files
/// without needing a connection, and take the first entry.
async fn socket_dir(major: u32, name: &str, timeout: Duration) -> PathBuf {
    if Path::new(PG_CONFTOOL).is_file() {
        let cmd = Cmd::new(PG_CONFTOOL)
            .arg(major.to_string())
            .arg(name)
            .arg("show")
            .arg("unix_socket_directories");
        if let Ok(output) = proc::run(&cmd, timeout).await {
            if output.ok() {
                if let Some(dir) = parse_socket_directories(output.first_line()) {
                    return dir;
                }
            }
        }
    }
    PathBuf::from(DEFAULT_SOCKET_DIR)
}

/// `pg_conftool show` prints `name = 'a, b'`, or just the value, depending on
/// version. Accept both and take the first directory.
pub fn parse_socket_directories(raw: &str) -> Option<PathBuf> {
    let value = raw.split_once('=').map(|(_, v)| v).unwrap_or(raw).trim();
    let value = value.trim_matches('\'').trim_matches('"');
    value
        .split(',')
        .map(str::trim)
        .find(|entry| entry.starts_with('/'))
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_OUTPUT: &str = "\
18  main    5432 online postgres /var/lib/postgresql/18/main /var/log/postgresql/postgresql-18-main.log
";

    #[test]
    fn parses_a_real_listing() {
        let listings = parse_lsclusters(REAL_OUTPUT);
        assert_eq!(listings.len(), 1);
        let c = &listings[0];
        assert_eq!(c.major, 18);
        assert_eq!(c.name, "main");
        assert_eq!(c.port, 5432);
        assert_eq!(c.owner, "postgres");
        assert_eq!(c.data_dir, PathBuf::from("/var/lib/postgresql/18/main"));
        assert!(c.is_online());
        assert_eq!(c.id(), "18/main");
    }

    #[test]
    fn parses_headers_multiple_rows_and_down_clusters() {
        let stdout = "\
Ver Cluster Port Status Owner    Data directory              Log file
16  main    5433 down   postgres /var/lib/postgresql/16/main /var/log/postgresql/postgresql-16-main.log
17  alt     5434 online,recovery postgres /var/lib/postgresql/17/alt /var/log/x.log
18  main    5432 online postgres /var/lib/postgresql/18/main /var/log/y.log
";
        let listings = parse_lsclusters(stdout);
        assert_eq!(listings.len(), 3);
        assert!(!listings[0].is_online());
        assert_eq!(listings[1].status, "online,recovery");
        assert!(!listings[1].is_online(), "recovery is not plain online");
        assert!(listings[2].is_online());
    }

    #[test]
    fn ignores_unparseable_rows() {
        assert!(parse_lsclusters("garbage\n").is_empty());
        assert!(parse_lsclusters("").is_empty());
        assert!(parse_lsclusters("x y z a b c\n").is_empty(), "bad version");
    }

    fn listing(major: u32, name: &str, status: &str) -> ClusterListing {
        ClusterListing {
            major,
            name: name.into(),
            port: 5432,
            status: status.into(),
            owner: "postgres".into(),
            data_dir: PathBuf::from("/var/lib/postgresql"),
        }
    }

    #[test]
    fn selects_the_only_online_supported_cluster() {
        let listings = vec![listing(18, "main", "online"), listing(16, "old", "down")];
        assert_eq!(select(&listings, None, false).unwrap().id(), "18/main");
    }

    #[test]
    fn refuses_to_guess_between_several_online_clusters() {
        let listings = vec![listing(18, "main", "online"), listing(17, "alt", "online")];
        let err = select(&listings, None, false).unwrap_err();
        assert!(err.to_string().contains("several clusters"));
        assert!(err.to_string().contains("18/main"));
        assert!(err.to_string().contains("17/alt"));
        assert!(
            err.remediation().unwrap().contains("--cluster"),
            "the error must say how to disambiguate"
        );
    }

    #[test]
    fn honours_an_explicit_selection_even_when_down() {
        let listings = vec![listing(18, "main", "online"), listing(16, "old", "down")];
        assert_eq!(
            select(&listings, Some("16/old"), false).unwrap().id(),
            "16/old"
        );
        assert_eq!(
            select(&listings, Some(" 16/old "), false).unwrap().id(),
            "16/old"
        );
    }

    #[test]
    fn reports_what_exists_when_the_selection_is_wrong() {
        let listings = vec![listing(18, "main", "online")];
        let err = select(&listings, Some("17/main"), false).unwrap_err();
        assert!(err.to_string().contains("no cluster \"17/main\""));
        assert!(err.to_string().contains("18/main"));
    }

    #[test]
    fn reports_no_supported_cluster_distinctly_from_none_online() {
        let unsupported = vec![listing(15, "main", "online")];
        let err = select(&unsupported, None, false).unwrap_err();
        assert!(err.to_string().contains("no PostgreSQL 16, 17 or 18"));

        let offline = vec![listing(18, "main", "down")];
        let err = select(&offline, None, false).unwrap_err();
        assert!(err.to_string().contains("no supported cluster is online"));
    }

    /// With one candidate the fix names that cluster and the command that
    /// starts it, rather than a `<major>/<name>` placeholder the reader has
    /// to resolve themselves.
    #[test]
    fn a_single_offline_cluster_is_named_in_the_fix() {
        let offline = vec![listing(18, "main", "down")];
        let fix = select(&offline, None, false)
            .unwrap_err()
            .remediation()
            .expect("the offline case carries a fix")
            .to_string();
        assert!(fix.contains("pg_ctlcluster 18 main start"), "{fix}");
        assert!(fix.contains("--cluster 18/main"), "{fix}");
        assert!(
            !fix.contains("<major>/<name>"),
            "a single candidate needs no placeholder: {fix}"
        );
    }

    /// Read-only diagnosis may select the one stopped cluster; a mutating
    /// command in the same situation must still refuse.
    #[test]
    fn offline_selection_is_allowed_only_when_asked_for() {
        let offline = vec![listing(18, "main", "down")];
        assert_eq!(select(&offline, None, true).unwrap().id(), "18/main");
        assert!(
            select(&offline, None, false).is_err(),
            "mutating commands must not reach a stopped cluster by auto-selection"
        );
    }

    /// Ambiguity is never relaxed: guessing which stopped server was meant is
    /// exactly what this function exists to prevent.
    #[test]
    fn offline_selection_still_refuses_ambiguity() {
        let offline = vec![listing(18, "main", "down"), listing(17, "old", "down")];
        let err = select(&offline, None, true).unwrap_err();
        assert!(err.to_string().contains("none is online"), "{err}");
        assert!(err.remediation().unwrap().contains("--cluster"));
    }

    /// An online cluster is still preferred over a stopped one.
    #[test]
    fn offline_selection_still_prefers_a_running_cluster() {
        let mixed = vec![listing(18, "main", "down"), listing(17, "live", "online")];
        assert_eq!(select(&mixed, None, true).unwrap().id(), "17/live");
    }

    /// `allow_offline` relaxes the *online* requirement, never the supported
    /// -major one.
    #[test]
    fn offline_selection_does_not_admit_an_unsupported_major() {
        let unsupported = vec![listing(15, "main", "down")];
        let err = select(&unsupported, None, true).unwrap_err();
        assert!(
            err.to_string().contains("no PostgreSQL 16, 17 or 18"),
            "{err}"
        );
    }

    /// With several, there is nothing to name, so the placeholder is right.
    #[test]
    fn several_offline_clusters_keep_the_placeholder() {
        let offline = vec![listing(18, "main", "down"), listing(17, "old", "down")];
        let fix = select(&offline, None, false)
            .unwrap_err()
            .remediation()
            .expect("the offline case carries a fix")
            .to_string();
        assert!(fix.contains("<major>/<name>"), "{fix}");
    }

    #[test]
    fn socket_directory_parsing_accepts_both_conftool_shapes() {
        assert_eq!(
            parse_socket_directories("unix_socket_directories = '/var/run/postgresql'"),
            Some(PathBuf::from("/var/run/postgresql"))
        );
        assert_eq!(
            parse_socket_directories("/var/run/postgresql"),
            Some(PathBuf::from("/var/run/postgresql"))
        );
        assert_eq!(
            parse_socket_directories("unix_socket_directories = '/var/run/postgresql, /tmp'"),
            Some(PathBuf::from("/var/run/postgresql"))
        );
        assert_eq!(parse_socket_directories(""), None);
        assert_eq!(parse_socket_directories("name = ''"), None);
    }

    #[test]
    fn restart_uses_the_cluster_specific_unit_or_pg_ctlcluster() {
        let restart = restart_command(18, "main");
        // Whichever branch this host takes, the command must name *this*
        // cluster, never `postgresql.service` (which would restart all of them).
        assert!(restart.display().contains("18"));
        assert!(restart.display().contains("main"));
        assert!(!restart.args.iter().any(|a| a == "postgresql.service"));
    }
}
