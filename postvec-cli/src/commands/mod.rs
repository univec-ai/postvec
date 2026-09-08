//! Command dispatch and the shared execution context.

pub mod account;
pub mod collect;
pub mod doctor;
pub mod model;
pub mod provider;
pub mod purge;
pub mod setup;
pub mod uninstall;

use crate::cli::{Cli, Command};
use crate::cluster::{self, Cluster, ClusterKind};
use crate::config::owned::OwnedPaths;
use crate::db::{agent::AgentClient, local::Direct, Db, DbTarget, ReadOnlyDb};
use crate::error::{CliError, Exit, Result};
use crate::facts::ServerFacts;
use crate::output::Output;
use crate::proc::{self, OsAccount};
use std::time::Duration;

pub async fn dispatch(cli: Cli) -> Exit {
    let output = Output::new(cli.format, cli.no_color);
    let (name, result) = match &cli.command {
        Command::Setup(args) => ("setup", setup::run(&cli, args.clone(), &output).await),
        Command::Uninstall(args) => (
            "uninstall",
            uninstall::run(&cli, args.clone(), &output).await,
        ),
        Command::Doctor(args) => ("doctor", doctor::run(&cli, args.clone(), &output).await),
        Command::Model(subcommand) => {
            let name = subcommand.name();
            let result = match subcommand {
                crate::cli::ModelCommand::Pull(args) => {
                    model::pull::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Upgrade(args) => {
                    model::pull::run_upgrade(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Ls(args) => {
                    model::ls::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Show(args) => {
                    model::show::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Rm(args) => {
                    model::rm::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Activate(args) => {
                    model::activate::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Deactivate(args) => {
                    model::deactivate::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::Prefer(args) => {
                    model::prefer::run(&cli, args.clone(), &output).await
                }
                crate::cli::ModelCommand::SetSpace(args) => {
                    model::set_space::run(&cli, args.clone(), &output).await
                }
            };
            (name, result)
        }
        Command::Provider(subcommand) => {
            let name = subcommand.name();
            let result = match subcommand {
                crate::cli::ProviderCommand::Add(args) => {
                    provider::add::run(&cli, (**args).clone(), &output).await
                }
                crate::cli::ProviderCommand::Ls(args) => {
                    provider::ls::run(&cli, args.clone(), &output).await
                }
                crate::cli::ProviderCommand::Rm(args) => {
                    provider::rm::run(&cli, args.clone(), &output).await
                }
                crate::cli::ProviderCommand::Test(args) => {
                    provider::test::run(&cli, args.clone(), &output).await
                }
            };
            (name, result)
        }
        Command::Login(args) => ("login", account::login(&cli, args.clone(), &output).await),
        Command::Logout => ("logout", account::logout(&output).await),
        Command::Whoami(args) => ("whoami", account::whoami(&cli, args.clone(), &output).await),
        // Both handled before the runtime starts.
        Command::DbAgent(_) => ("agent", Ok(Exit::Success)),
        Command::PreloadMerge(_) => ("preload-merge", Ok(Exit::Success)),
    };
    match result {
        Ok(exit) => exit,
        Err(error) => {
            output.show_error(name, &error);
            error.exit()
        }
    }
}

/// Everything a command needs: the selected cluster, a way to talk to it, and
/// the facts read at connection time.
pub struct Context {
    pub cluster: Cluster,
    pub db: Db,
    pub server: ServerFacts,
    pub timeout: Duration,
    /// True when database work goes through an operator-supplied URI rather
    /// than the selected cluster's own socket. The two can point at different
    /// servers, so any command that changes host configuration has to prove
    /// they do not — see [`Context::require_database_is_the_selected_cluster`].
    pub database_url_supplied: bool,
}

impl Context {
    /// Discover the cluster, connect, and (for an explicitly selected
    /// installation) fill in the paths only the server knows.
    pub async fn open(cli: &Cli, output: &Output) -> Result<Self> {
        // A URI on its own says nothing about this machine. Pairing it with
        // whatever cluster happens to be installed here would mix one server's
        // settings with another's package assets, ownership state and offline
        // configuration — so that pairing has to be asked for explicitly, with
        // --cluster or --pg-config, and is then verified.
        let selection_is_explicit = cli.cluster.is_some() || cli.pg_config.is_some();
        if let (Some(url), false) = (&cli.database_url, selection_is_explicit) {
            output.note(
                "inspecting the supplied connection only; pass --cluster or --pg-config to \
                 pair it with a local installation",
            );
            let mut db = Self::connect(DbTarget::Url(url.clone()), None, cli.timeout).await?;
            let server = db.server_facts().await?;
            let cluster = cluster::remote_only(&server);
            return Ok(Self {
                cluster,
                db,
                server,
                timeout: cli.timeout,
                database_url_supplied: true,
            });
        }

        let cluster = Self::discover_local(cli, output).await?;
        Self::connect_to(cli, cluster, output).await
    }

    /// Select a local cluster without opening a database session. Model
    /// commands that only need the engine root use this, then try
    /// [`Self::connect_to`] and fall back to the owned snippet.
    pub async fn discover_local(cli: &Cli, output: &Output) -> Result<Cluster> {
        let cluster = cluster::discover(
            cli.cluster.as_deref(),
            cli.pg_config.as_deref(),
            cli.config_dir.as_deref(),
            cli.timeout,
        )
        .await?;
        output.note(&format!(
            "cluster {} (PostgreSQL {} binaries at {})",
            cluster.identity.id,
            cluster.binary_major,
            cluster.bindir.display()
        ));
        Ok(cluster)
    }

    /// Open a session against an already-discovered cluster.
    pub async fn connect_to(cli: &Cli, mut cluster: Cluster, output: &Output) -> Result<Self> {
        let (target, db_identity) = Self::resolve_target(cli, &cluster);
        if let Some(account) = db_identity.as_ref().filter(|_| proc::is_root()) {
            output.note(&format!(
                "database work runs as {:?} (privileges are dropped for it)",
                account.name
            ));
        }
        let mut db = Self::connect(target, db_identity.as_ref(), cli.timeout).await?;
        let server = db.server_facts().await?;
        if cluster.kind == ClusterKind::Explicit {
            crate::cluster::explicit::apply_server_facts(&mut cluster, &server);
        }
        Ok(Self {
            cluster,
            db,
            server,
            timeout: cli.timeout,
            database_url_supplied: cli.database_url.is_some(),
        })
    }

    /// Decide how to reach the database, and as whom.
    ///
    /// An operator-supplied URI is used as given and the CLI does not touch its
    /// identity — they chose password or certificate authentication
    /// deliberately. A local socket means peer authentication, which needs the
    /// cluster owner's OS identity.
    fn resolve_target(cli: &Cli, cluster: &Cluster) -> (DbTarget, Option<OsAccount>) {
        if let Some(url) = &cli.database_url {
            return (DbTarget::Url(url.clone()), None);
        }
        let owner = cluster.owner.clone();
        let user = owner
            .as_ref()
            .map(|account| account.name.clone())
            // Without a known owner, peer authentication uses whoever we are.
            .unwrap_or_else(current_account_name);
        let target = DbTarget::Socket {
            dir: cluster.socket_dir.clone(),
            port: cluster.port,
            user,
        };
        // Privileges only need dropping when we are somebody else — normally
        // root, because writing /etc/postgresql requires it.
        let identity = owner.filter(|account| !account.is_current());
        (target, identity)
    }

    async fn connect(
        target: DbTarget,
        identity: Option<&OsAccount>,
        timeout: Duration,
    ) -> Result<Db> {
        match identity {
            Some(account) if proc::is_root() => Ok(Db::Agent(
                AgentClient::spawn(
                    account,
                    target,
                    timeout,
                    timeout.max(Duration::from_secs(30)),
                )
                .await?,
            )),
            // Not root: connect directly and let PostgreSQL decide whether this
            // identity is allowed. A clear authentication error beats a
            // privilege-drop error the caller cannot act on.
            _ => Ok(Db::Direct(Direct::new(target, Some(timeout)))),
        }
    }

    /// Whether the connected server is the instance the selected cluster's own
    /// endpoint reaches.
    ///
    /// Local discovery and `--database-url` are independent: discovery owns the
    /// host side, the URI owns the database side. Nothing else stops `setup`
    /// from installing the extension into a server in another datacentre and
    /// then rewriting the configuration of — and restarting — the cluster on
    /// this machine.
    ///
    /// The proof is a **second connection through the cluster's own socket**,
    /// compared with the supplied one on two values:
    ///
    /// - the **system identifier**, which identifies a replication *lineage*. A
    ///   physical standby, or any restored copy, carries its primary's — so on
    ///   its own it would match a primary against its own standby, which is the
    ///   pairing most likely to reconfigure the wrong host;
    /// - the **exact postmaster start time**, which identifies the instance.
    ///
    /// It has to be a second connection. The obvious cheaper source, line 3 of
    /// `postmaster.pid`, is `MyStartTime`: whole seconds, captured earlier in
    /// startup than the `PgStartTime` that `pg_postmaster_start_time()`
    /// returns. Comparing the two would reject the *right* postmaster whenever
    /// startup crossed a second boundary, and would accept a different
    /// same-lineage postmaster that happened to start in the same second.
    /// Asking both connections the same question is the only way to get an
    /// answer that means what it says.
    pub async fn prove_database_is_the_selected_cluster(&self) -> IdentityProof {
        if !self.database_url_supplied || self.cluster.kind == ClusterKind::Remote {
            // A socket target is the cluster's own socket; a remote-only
            // context has no host side to disagree with.
            return IdentityProof::NotApplicable;
        }
        if self.cluster.kind == ClusterKind::Explicit {
            // The port and socket directory of an explicitly selected
            // installation are defaults, not discoveries — and one of them is
            // filled in from the supplied connection itself, so probing them
            // would ask the same server twice.
            return IdentityProof::Unprovable {
                reason: "an installation selected with --pg-config has no independently known \
                         local endpoint to compare against"
                    .to_string(),
                fix: "drop --database-url so the command uses that installation's own socket"
                    .to_string(),
            };
        }
        let Some(connected_identifier) = self.server.system_identifier.clone() else {
            return IdentityProof::Unprovable {
                reason: "the connecting role may not read the server's system identifier"
                    .to_string(),
                fix: "connect as a superuser (pg_control_system() is superuser-only by \
                      default), or drop --database-url so the command uses the cluster's own \
                      socket"
                    .to_string(),
            };
        };
        let Some(connected_start) = self.server.postmaster_start_exact.clone() else {
            return IdentityProof::Unprovable {
                reason: "the server did not report its postmaster start time".to_string(),
                fix: "drop --database-url so the command uses the cluster's own socket".to_string(),
            };
        };

        let local = match self.local_server_facts().await {
            Ok(facts) => facts,
            Err(error) => {
                return IdentityProof::Unprovable {
                    reason: format!(
                        "the cluster could not be contacted through its own socket ({}): {error}",
                        self.cluster.socket_dir.display()
                    ),
                    fix: "start the cluster, run as root or as the cluster owner, or drop \
                          --database-url"
                        .to_string(),
                }
            }
        };
        let (Some(local_identifier), Some(local_start)) =
            (local.system_identifier, local.postmaster_start_exact)
        else {
            return IdentityProof::Unprovable {
                reason: "the cluster's own connection did not report its identity (the role it \
                         authenticated as may not read it)"
                    .to_string(),
                fix: "run as root or as the cluster owner, or drop --database-url".to_string(),
            };
        };
        compare_identity(
            &connected_identifier,
            &connected_start,
            &local_identifier,
            &local_start,
        )
    }

    /// Open a short-lived connection through the selected cluster's own socket,
    /// purely to read its identity. Independent of the supplied URI by
    /// construction.
    async fn local_server_facts(&self) -> Result<ServerFacts> {
        let owner = self.cluster.owner.clone();
        let target = DbTarget::Socket {
            dir: self.cluster.socket_dir.clone(),
            port: self.cluster.port,
            user: owner
                .as_ref()
                .map(|account| account.name.clone())
                .unwrap_or_else(current_account_name),
        };
        let identity = owner.filter(|account| !account.is_current());
        let mut db = Self::connect(target, identity.as_ref(), self.timeout).await?;
        let facts = db.server_facts().await;
        db.close().await;
        facts
    }

    /// The mutating-command form: anything short of proof is a refusal.
    pub async fn require_database_is_the_selected_cluster(&self) -> Result<()> {
        match self.prove_database_is_the_selected_cluster().await {
            IdentityProof::NotApplicable | IdentityProof::Proven => Ok(()),
            IdentityProof::Mismatch { detail } => Err(CliError::precondition(format!(
                "--database-url points at a different server than cluster {}: {detail}",
                self.cluster.identity.id
            ))
            .with_fix(
                "changing host configuration for one server while installing into another \
                 would restart the wrong cluster; drop --database-url to use the cluster's own \
                 socket, or run the command on the host that serves that URI",
            )),
            IdentityProof::Unprovable { reason, fix } => Err(CliError::precondition(format!(
                "cannot prove that --database-url points at cluster {}: {reason}",
                self.cluster.identity.id
            ))
            .with_fix(fix)),
        }
    }

    /// A read-only handle. `doctor` holds only this, which makes "doctor never
    /// writes" a property of the type rather than of review discipline.
    pub fn read_only(&mut self) -> ReadOnlyDb<'_> {
        ReadOnlyDb::new(&mut self.db)
    }

    pub fn owned_paths(&self) -> Result<OwnedPaths> {
        self.cluster.owned_paths()
    }

    /// Where the operator should look for launcher-side engine errors. The
    /// launcher has no database connection, so those exist only in the server
    /// log. Printed as remediation, never executed.
    pub fn log_command(&self) -> Option<String> {
        match self.cluster.kind {
            ClusterKind::Debian => Some(format!(
                "journalctl -u postgresql@{}-{}.service --since -10m --grep postvec",
                self.cluster.identity.major, self.cluster.identity.name
            )),
            ClusterKind::Explicit | ClusterKind::Remote => None,
        }
    }

    /// How long the cluster has been up, from the postmaster start time.
    pub fn uptime(&self) -> Option<Duration> {
        parse_postmaster_time(&self.server.postmaster_start_time).and_then(|started| {
            chrono::Utc::now()
                .signed_duration_since(started)
                .to_std()
                .ok()
        })
    }

    pub async fn close(self) {
        self.db.close().await;
    }
}

/// Compare a connection's identity against the local instance's.
///
/// Split out from the IO so both outcomes can be tested — in particular the
/// primary/standby case, which is the whole reason the start time is compared
/// as well as the system identifier.
fn compare_identity(
    connected_identifier: &str,
    connected_start: &str,
    local_identifier: &str,
    local_start: &str,
) -> IdentityProof {
    if connected_identifier != local_identifier {
        return IdentityProof::Mismatch {
            detail: format!(
                "different replication lineage: the connected server's system identifier is \
                 {connected_identifier}, the cluster's is {local_identifier}"
            ),
        };
    }
    // Same lineage is exactly where a primary and its standby agree, so the
    // running instance has to be identified separately. Both sides answer the
    // same question here, so this is an exact comparison of one value.
    if connected_start != local_start {
        return IdentityProof::Mismatch {
            detail: format!(
                "same replication lineage but a different running instance: the connected \
                 postmaster started at {connected_start} UTC, the cluster's at {local_start} \
                 UTC — this is what a primary and its own standby look like"
            ),
        };
    }
    IdentityProof::Proven
}

/// The outcome of checking that a supplied URI and the selected cluster are the
/// same running instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityProof {
    /// No URI, or no host side: there is nothing that could disagree.
    NotApplicable,
    Proven,
    Mismatch {
        detail: String,
    },
    Unprovable {
        reason: String,
        fix: String,
    },
}

/// PostgreSQL renders `timestamptz` as `2026-07-30 10:00:00.123+00`, whose
/// offset has no colon and whose fractional part is optional.
fn parse_postmaster_time(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f",
    ] {
        if let Ok(parsed) = chrono::DateTime::parse_from_str(raw.trim(), format) {
            return Some(parsed);
        }
    }
    None
}

fn current_account_name() -> String {
    // Best effort: the connection error names the account if this is wrong.
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "postgres".to_string())
}

/// Require that the caller can write host configuration before a plan promises
/// to do so.
pub fn require_host_privileges(cluster: &Cluster) -> Result<()> {
    if proc::is_root() {
        return Ok(());
    }
    let conf_dir = cluster.conf_dir.as_ref();
    let writable = conf_dir
        .map(|dir| {
            // A cheap, honest test: can this process create a file here?
            let probe = dir.join(".postvec-write-probe");
            let result = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe);
            let ok = result.is_ok();
            let _ = std::fs::remove_file(&probe);
            ok
        })
        .unwrap_or(false);
    if writable {
        Ok(())
    } else {
        Err(CliError::precondition(format!(
            "changing cluster configuration needs write access to {}",
            conf_dir
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "the cluster's configuration directory".to_string())
        ))
        .with_fix("rerun with sudo"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ClusterIdentity;
    use std::path::PathBuf;

    fn cluster(owner: Option<OsAccount>) -> Cluster {
        Cluster {
            identity: ClusterIdentity {
                id: "18/main".into(),
                major: 18,
                name: "main".into(),
            },
            kind: ClusterKind::Debian,
            owner,
            pg_config: PathBuf::from("/usr/lib/postgresql/18/bin/pg_config"),
            bindir: PathBuf::from("/usr/lib/postgresql/18/bin"),
            sharedir: PathBuf::from("/usr/share/postgresql/18"),
            pkglibdir: PathBuf::from("/usr/lib/postgresql/18/lib"),
            binary_major: 18,
            conf_dir: Some(PathBuf::from("/etc/postgresql/18/main/conf.d")),
            config_file: Some(PathBuf::from("/etc/postgresql/18/main/postgresql.conf")),
            data_dir: Some(PathBuf::from("/var/lib/postgresql/18/main")),
            port: 5432,
            socket_dir: PathBuf::from("/var/run/postgresql"),
            running: true,
            restart: None,
        }
    }

    fn cli() -> Cli {
        use clap::Parser;
        Cli::try_parse_from(["postvec", "doctor"]).unwrap()
    }

    #[test]
    fn a_supplied_uri_is_used_verbatim_and_keeps_its_identity() {
        let mut cli = cli();
        cli.database_url = Some("postgres://alice:s3cret@db:5432/univec".into());
        let (target, identity) = Context::resolve_target(&cli, &cluster(None));
        assert!(matches!(target, DbTarget::Url(url) if url.contains("alice")));
        assert!(
            identity.is_none(),
            "the operator chose their own authentication; do not change identity"
        );
    }

    #[test]
    fn a_local_socket_target_uses_the_cluster_owner_for_peer_authentication() {
        let owner = OsAccount {
            name: "postgres".into(),
            uid: 999_999,
            gid: 999_999,
            groups: vec![999_999],
        };
        let (target, identity) = Context::resolve_target(&cli(), &cluster(Some(owner)));
        match target {
            DbTarget::Socket { dir, port, user } => {
                assert_eq!(dir, PathBuf::from("/var/run/postgresql"));
                assert_eq!(port, 5432);
                assert_eq!(user, "postgres");
            }
            other => panic!("expected a socket target, got {other:?}"),
        }
        assert_eq!(
            identity.map(|account| account.name),
            Some("postgres".to_string()),
            "a different account means privileges must be dropped"
        );
    }

    fn context_for(server: ServerFacts, cluster: Cluster, url_supplied: bool) -> Context {
        Context {
            cluster,
            db: Db::Direct(crate::db::local::Direct::new(
                DbTarget::Url("postgres://x/y".into()),
                None,
            )),
            server,
            timeout: Duration::from_secs(1),
            database_url_supplied: url_supplied,
        }
    }

    fn server_at(
        data_dir: Option<&str>,
        config: Option<&str>,
        port: i32,
        system_identifier: Option<&str>,
    ) -> ServerFacts {
        ServerFacts {
            version_num: 180_004,
            version: "18.4".into(),
            data_directory: data_dir.map(str::to_string),
            config_file: config.map(str::to_string),
            hba_file: None,
            port,
            postmaster_start_time: "2026-07-30 10:00:00+00".into(),
            postmaster_start_exact: None,
            system_identifier: system_identifier.map(str::to_string),
        }
    }

    /// The case the system identifier alone cannot catch, and the reason the
    /// postmaster start time is compared too: a physical standby carries its
    /// primary's identifier, so "same lineage" is not "same instance". This is
    /// also the most plausible way to reconfigure the wrong host — a URI
    /// pointing at the primary, run on the standby.
    #[test]
    fn a_standby_is_not_its_primary() {
        let lineage = "7558833644402126636";
        // The local cluster is the standby; the URI reached the primary. Same
        // identifier, different postmaster.
        let proof = compare_identity(
            lineage,
            "2026-07-24 09:00:00.123456",
            lineage,
            "2026-07-29 16:18:39.706537",
        );
        match proof {
            IdentityProof::Mismatch { detail } => {
                assert!(detail.contains("same replication lineage"), "{detail}");
                assert!(detail.contains("standby"), "{detail}");
            }
            other => panic!("a standby must not pass as its primary: {other:?}"),
        }
    }

    /// Both sides answer the *same* question, so the comparison is exact.
    /// Comparing a whole-second `postmaster.pid` timestamp against
    /// `pg_postmaster_start_time()` would reject this pairing outright, because
    /// PostgreSQL captures those two values at different moments.
    #[test]
    fn sub_second_precision_is_compared_exactly() {
        let lineage = "7558833644402126636";
        assert_eq!(
            compare_identity(
                lineage,
                "2026-07-29 16:18:39.706537",
                lineage,
                "2026-07-29 16:18:39.706537",
            ),
            IdentityProof::Proven
        );
        // Two postmasters that started in the same second are still two
        // postmasters.
        assert!(matches!(
            compare_identity(
                lineage,
                "2026-07-29 16:18:39.706537",
                lineage,
                "2026-07-29 16:18:39.999999",
            ),
            IdentityProof::Mismatch { .. }
        ));
    }

    #[test]
    fn a_different_lineage_is_rejected_on_the_identifier_alone() {
        assert!(matches!(
            compare_identity(
                "7000000000000000001",
                "2026-07-29 16:18:39.706537",
                "7558833644402126636",
                "2026-07-29 16:18:39.706537",
            ),
            IdentityProof::Mismatch { detail } if detail.contains("different replication lineage")
        ));
    }

    /// A server that will not reveal its identity cannot be paired with a local
    /// cluster: guessing wrong means restarting the wrong database.
    #[tokio::test]
    async fn an_unprovable_target_is_refused_rather_than_assumed() {
        let context = context_for(
            server_at(Some("/var/lib/postgresql/18/main"), None, 5432, None),
            cluster(None),
            true,
        );
        match context.prove_database_is_the_selected_cluster().await {
            IdentityProof::Unprovable { reason, fix } => {
                assert!(reason.contains("system identifier"), "{reason}");
                assert!(fix.contains("--database-url"), "{fix}");
            }
            other => panic!("expected an unprovable outcome, got {other:?}"),
        }
        let error = context
            .require_database_is_the_selected_cluster()
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cannot prove"));
    }

    /// Identical default paths are not evidence; without the identity values
    /// the command still refuses.
    #[tokio::test]
    async fn identical_default_paths_do_not_establish_identity() {
        let context = context_for(
            server_at(
                Some("/var/lib/postgresql/18/main"),
                Some("/etc/postgresql/18/main/postgresql.conf"),
                5432,
                None,
            ),
            cluster(None),
            true,
        );
        assert!(!matches!(
            context.prove_database_is_the_selected_cluster().await,
            IdentityProof::Proven
        ));
    }

    #[tokio::test]
    async fn a_socket_target_needs_no_proof() {
        // Without --database-url the connection *is* the cluster's own socket.
        let context = context_for(server_at(None, None, 5432, None), cluster(None), false);
        assert_eq!(
            context.prove_database_is_the_selected_cluster().await,
            IdentityProof::NotApplicable
        );
    }

    /// A remote-only context has no host side to disagree with.
    #[tokio::test]
    async fn a_remote_only_context_needs_no_proof() {
        let server = server_at(None, None, 5432, None);
        let context = context_for(server.clone(), crate::cluster::remote_only(&server), true);
        assert_eq!(
            context.prove_database_is_the_selected_cluster().await,
            IdentityProof::NotApplicable
        );
    }

    #[test]
    fn no_privilege_drop_is_needed_when_we_are_already_the_owner() {
        let me = OsAccount {
            name: "me".into(),
            uid: proc::current_uid(),
            gid: 0,
            groups: vec![],
        };
        let (_, identity) = Context::resolve_target(&cli(), &cluster(Some(me)));
        assert!(identity.is_none());
    }

    #[test]
    fn postmaster_timestamps_parse_in_postgresqls_own_rendering() {
        assert!(parse_postmaster_time("2026-07-30 10:00:00.123456+00").is_some());
        assert!(parse_postmaster_time("2026-07-30 10:00:00+00").is_some());
        assert!(parse_postmaster_time("2026-07-30 10:00:00+02:00").is_some());
        assert!(parse_postmaster_time("not a timestamp").is_none());
    }
}
