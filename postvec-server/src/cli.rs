//! Command-line surface.
//!
//! Every value-carrying flag is an `Option`, and *only* the flags are parsed
//! here: the environment and the optional configuration file are separate
//! layers merged in [`crate::config`]. That separation is what makes the
//! precedence rule (`defaults < file < env < flags`) a testable function
//! rather than a property of clap's argument table.
//!
//! `serve` is the default subcommand, so a systemd unit is
//! `ExecStart=/usr/bin/postvec-server --peers …` with no verb.

use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::PathBuf;

pub const DEFAULT_HTTP_PORT: u16 = 22222;
pub const DEFAULT_GRPC_PORT: u16 = 33333;
pub const DEFAULT_GOSSIP_PORT: u16 = 11111;
pub const DEFAULT_ADMIN_PORT: u16 = 22223;

#[derive(Debug, Parser)]
#[command(
    name = "postvec-server",
    version,
    about = "Inference node for postvec's remote (grpc) mode",
    long_about = "\
Serves postvec's remote mode: the inference gRPC contract (EmbedTexts, \
ConvertEmbeddings) plus the GET /config discovery envelope, backed by ONNX \
models already present under <root>/models.

This process has no model hub and never downloads weights. Put models on disk \
with `postvec model pull`, a shared volume, or your own copy step.

SECURITY: the gRPC port has no transport security and no authentication, and \
the discovery port is unauthenticated. Both are designed for a trusted \
private network and must not be reachable from the internet. Only the \
loopback admin port can mutate the engine.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Flags for the implicit `serve` command.
    #[command(flatten)]
    pub serve: ServeArgs,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the node (the default when no command is given).
    Serve(Box<ServeArgs>),

    /// Show the local node's models, peers and on-disk inventory drift.
    Status(StatusArgs),

    /// Load models into the running local node.
    Load(ModelArgs),

    /// Unload models from the running local node.
    Unload(ModelArgs),
}

#[derive(Debug, Args, Default)]
pub struct ServeArgs {
    /// Engine root holding libs/ and models/.
    ///
    /// [env: POSTVEC_SERVER_ROOT] [default: current directory]
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Configuration file. Optional; see --help for the search order.
    ///
    /// [env: POSTVEC_SERVER_CONFIG]
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Address the gRPC and discovery listeners bind. [default: 0.0.0.0]
    ///
    /// [env: POSTVEC_SERVER_BIND]
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,

    /// Port for the discovery/health listener. [default: 22222]
    ///
    /// [env: POSTVEC_SERVER_HTTP]
    #[arg(long, value_name = "PORT")]
    pub http: Option<u16>,

    /// Port for the gRPC inference listener. [default: 33333]
    ///
    /// [env: POSTVEC_SERVER_GRPC]
    #[arg(long, value_name = "PORT")]
    pub grpc: Option<u16>,

    /// Port for cluster gossip. Identical on every node. [default: 11111]
    ///
    /// [env: POSTVEC_SERVER_GOSSIP]
    #[arg(long, value_name = "PORT")]
    pub gossip: Option<u16>,

    /// Deprecated spelling of --gossip. Accepted; errors if the two disagree.
    #[arg(long, value_name = "PORT", hide = true)]
    pub remote: Option<u16>,

    /// Loopback-only port for the admin routes. [default: 22223]
    ///
    /// [env: POSTVEC_SERVER_ADMIN]
    #[arg(long = "admin", value_name = "PORT")]
    pub admin: Option<u16>,

    /// Comma-separated gossip peers: hosts, or host:port to override the
    /// gossip port for one entry. Empty starts a single-node cluster.
    ///
    /// [env: POSTVEC_SERVER_PEERS, then POSTVEC_SERVER_CLUSTER]
    #[arg(
        long,
        alias = "cluster",
        value_name = "LIST",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub peers: Option<Vec<String>>,

    /// Cluster tag. Nodes with different groups never merge. [default: postvec]
    ///
    /// [env: POSTVEC_SERVER_GROUP]
    #[arg(long, value_name = "NAME")]
    pub group: Option<String>,

    /// IP this node advertises to its peers. Set it on a multi-homed host.
    ///
    /// [env: POSTVEC_SERVER_ADVERTISE] [default: --bind if specific, else autodetected]
    #[arg(long, value_name = "IP")]
    pub advertise: Option<String>,

    /// Absolute URL at which this node's HTTP API is reachable.
    ///
    /// [env: POSTVEC_SERVER_FRONTEND] [default: {scheme}://{advertise}:{http}]
    #[arg(long, value_name = "URL")]
    pub frontend: Option<String>,

    /// PEM certificate for the discovery listener. [default: <root>/certs/server.crt]
    ///
    /// [env: POSTVEC_SERVER_SSL_CERT]
    #[arg(long = "ssl-cert", value_name = "PATH")]
    pub ssl_cert: Option<PathBuf>,

    /// PEM private key for the discovery listener. [default: <root>/certs/server.key]
    ///
    /// [env: POSTVEC_SERVER_SSL_KEY]
    #[arg(long = "ssl-cert-key", alias = "ssl-key", value_name = "PATH")]
    pub ssl_cert_key: Option<PathBuf>,

    /// Serve discovery over plain HTTP. For local development and CI only.
    ///
    /// [env: POSTVEC_SERVER_INSECURE]
    #[arg(long, action = ArgAction::SetTrue)]
    pub insecure: bool,

    /// Only load these models. Empty loads every enabled descriptor.
    ///
    /// [env: POSTVEC_SERVER_MODELS]
    #[arg(long, value_name = "LIST", value_delimiter = ',', num_args = 1..)]
    pub models: Option<Vec<String>>,

    /// Directory of external-provider connector files (providers.d).
    /// A path, never a credential: provider API keys live only in the
    /// (0600) TOML files under it. Empty or missing means no
    /// provider-backed models.
    ///
    /// [env: POSTVEC_SERVER_PROVIDERS_PATH] [default: <root>/providers.d]
    #[arg(long = "providers-path", value_name = "PATH")]
    pub providers_path: Option<PathBuf>,

    /// Ceiling on a request's execution budget, in milliseconds. A smaller
    /// inbound grpc-timeout always wins. [default: 30000]
    ///
    /// [env: POSTVEC_SERVER_PREDICT_TIMEOUT_MS]
    #[arg(long = "predict-timeout-ms", value_name = "N")]
    pub predict_timeout_ms: Option<u64>,

    /// Concurrently executing predictions. Bounds CPU oversubscription and
    /// coexisting response trees. [default: CPU count, clamped to 4..=16]
    ///
    /// [env: POSTVEC_SERVER_MAX_INFLIGHT]
    #[arg(long = "max-inflight", value_name = "N")]
    pub max_inflight: Option<usize>,

    /// Ceiling on resident models, dependency closures included. [default: 16]
    ///
    /// [env: POSTVEC_SERVER_MAX_RESIDENT_MODELS]
    #[arg(long = "max-resident-models", value_name = "N")]
    pub max_resident_models: Option<usize>,

    /// How long to keep serving after a shutdown signal while /ready already
    /// answers 503, so healthchecks and load balancers observe the node
    /// leaving before its socket closes. 0 disables. [default: 5000]
    ///
    /// [env: POSTVEC_SERVER_DRAIN_DELAY_MS]
    #[arg(long = "drain-delay-ms", value_name = "N")]
    pub drain_delay_ms: Option<u64>,

    /// Skip the boot-time warmup prediction.
    ///
    /// [env: POSTVEC_SERVER_WARMUP=0]
    #[arg(long = "no-warmup", action = ArgAction::SetTrue)]
    pub no_warmup: bool,

    /// Do not serve GET /metrics.
    ///
    /// [env: POSTVEC_SERVER_METRICS=0]
    #[arg(long = "no-metrics", action = ArgAction::SetTrue)]
    pub no_metrics: bool,

    /// Log filter, when RUST_LOG is unset. [default: info]
    #[arg(long = "log-level", value_name = "FILTER")]
    pub log_level: Option<String>,
}

/// Flags shared by the node-local client subcommands. They talk to
/// `127.0.0.1` and never to a peer: `status`, `load` and `unload` are
/// node-local tools, not fleet orchestration.
#[derive(Debug, Args, Default)]
pub struct LocalArgs {
    /// Admin port of the local node. [default: 22223]
    #[arg(long = "admin", value_name = "PORT")]
    pub admin: Option<u16>,

    /// Engine root, for the on-disk half of the inventory report.
    #[arg(long, value_name = "PATH")]
    pub root: Option<PathBuf>,

    /// Request timeout in seconds. [default: 10]
    #[arg(long, value_name = "N")]
    pub timeout: Option<u64>,

    /// Emit the raw JSON report instead of the human-readable one.
    #[arg(long, action = ArgAction::SetTrue)]
    pub json: bool,
}

#[derive(Debug, Args, Default)]
pub struct StatusArgs {
    #[command(flatten)]
    pub local: LocalArgs,

    /// Also read every alive peer's /config and compare model inventories.
    ///
    /// Off by default because `status` is a node-local tool; on, it is the
    /// check that makes "every node carries the same enabled set" more than
    /// an operator convention.
    #[arg(long, action = ArgAction::SetTrue)]
    pub fleet: bool,
}

#[derive(Debug, Args, Default)]
pub struct ModelArgs {
    /// Model names.
    #[arg(value_name = "MODEL", required = true, num_args = 1..)]
    pub models: Vec<String>,

    #[command(flatten)]
    pub local: LocalArgs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serve_is_the_default_command() {
        let cli = Cli::try_parse_from(["postvec-server", "--peers", "10.0.0.1,10.0.0.2"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(
            cli.serve.peers.as_deref(),
            Some(["10.0.0.1".to_string(), "10.0.0.2".to_string()].as_slice())
        );
    }

    /// `--cluster` is the spelling in the original sketch and in the upstream engine's
    /// configuration. It stays typeable; `--peers` is the documented name
    /// because "cluster" already means "PostgreSQL cluster" in postvec-cli.
    #[test]
    fn cluster_is_an_accepted_alias_for_peers() {
        let cli = Cli::try_parse_from(["postvec-server", "--cluster", "a,b"]).unwrap();
        assert_eq!(
            cli.serve.peers.as_deref(),
            Some(["a".to_string(), "b".to_string()].as_slice())
        );
    }

    #[test]
    fn remote_is_a_hidden_alias_for_gossip() {
        let cli = Cli::try_parse_from(["postvec-server", "--remote", "11111"]).unwrap();
        assert_eq!(cli.serve.remote, Some(11111));
        assert_eq!(cli.serve.gossip, None);
    }

    #[test]
    fn ssl_key_is_an_accepted_alias() {
        let cli = Cli::try_parse_from(["postvec-server", "--ssl-key", "/k.pem"]).unwrap();
        assert_eq!(cli.serve.ssl_cert_key.unwrap().to_str(), Some("/k.pem"));
    }

    #[test]
    fn subcommands_parse() {
        let cli = Cli::try_parse_from(["postvec-server", "load", "a", "b"]).unwrap();
        match cli.command {
            Some(Command::Load(args)) => assert_eq!(args.models, ["a", "b"]),
            other => panic!("expected load, got {other:?}"),
        }
        assert!(Cli::try_parse_from(["postvec-server", "load"]).is_err());
        assert!(Cli::try_parse_from(["postvec-server", "status"]).is_ok());
        let cli = Cli::try_parse_from(["postvec-server", "status", "--fleet"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Status(a)) if a.fleet));
    }

    /// Absent flags must stay absent, so the merge in `config` can tell
    /// "unset" from "set to the default".
    #[test]
    fn absent_flags_are_none() {
        let cli = Cli::try_parse_from(["postvec-server"]).unwrap();
        assert!(cli.serve.http.is_none());
        assert!(cli.serve.grpc.is_none());
        assert!(cli.serve.peers.is_none());
        assert!(!cli.serve.insecure);
    }
}
