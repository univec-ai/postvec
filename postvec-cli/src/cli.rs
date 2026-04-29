//! The public command-line contract.
//!
//! Everything here is declarative: parsing, value validation, and the flag
//! relationships Clap can express. Semantic validation that needs the host
//! (cluster discovery, filesystem, endpoints) lives in `commands/`.

use crate::error::{CliError, Result};
use crate::validate;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Install, configure and diagnose postvec against an existing PostgreSQL
/// cluster.
///
/// The package manager owns the extension files (`postvec.so`, the control
/// file, the SQL scripts) and this binary; the CLI never copies or deletes
/// package-owned files.
#[derive(Debug, Parser)]
#[command(
    name = "postvec",
    version,
    about = "Install and diagnose postvec",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Cluster identity as reported by pg_lsclusters, e.g. 18/main.
    #[arg(long, global = true, value_name = "MAJOR/NAME")]
    pub cluster: Option<String>,

    /// Explicit pg_config for a non-postgresql-common installation.
    #[arg(long, global = true, value_name = "PATH")]
    pub pg_config: Option<PathBuf>,

    /// Configuration directory already included by postgresql.conf.
    /// Only meaningful together with --pg-config.
    #[arg(long, global = true, requires = "pg_config", value_name = "DIR")]
    pub config_dir: Option<PathBuf>,

    /// Connection URI. Prefer POSTVEC_DATABASE_URL: an URI on the command
    /// line is visible in process listings.
    #[arg(
        long,
        global = true,
        env = "POSTVEC_DATABASE_URL",
        hide_env_values = true,
        value_name = "URI"
    )]
    pub database_url: Option<String>,

    /// Output format. JSON is a versioned object on stdout; human progress
    /// then goes to stderr.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Disable ANSI styling (also honoured via NO_COLOR).
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Overall deadline for the command's network and subprocess work.
    #[arg(long, global = true, value_parser = parse_duration, default_value = "30s")]
    pub timeout: Duration,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Configure a cluster and install the extension into one or more
    /// databases.
    Setup(SetupArgs),
    /// Remove postvec's database objects and extension, and stop serving the
    /// database. Retains user data by default.
    Uninstall(UninstallArgs),
    /// Read-only diagnosis of an installation. Changes nothing.
    Doctor(DoctorArgs),
    /// Manage the models in an engine root: pull from the UniVec registry,
    /// list, inspect, remove, activate, deactivate.
    #[command(subcommand)]
    Model(ModelCommand),
    /// Manage external embedding providers (OpenAI, Gemini, Cohere, AWS
    /// Bedrock, Mistral, OpenRouter): the providers.d connector files the
    /// inference host reads. Credentials live only in those (0600) files —
    /// never in a GUC, catalog table or SQL argument.
    #[command(subcommand)]
    Provider(ProviderCommand),
    /// Store a UniVec API key for the authenticated model catalogue.
    Login(LoginArgs),
    /// Remove the stored credential; model commands become anonymous.
    Logout,
    /// Report which credential (if any) model commands would use.
    Whoami(WhoamiArgs),
    /// Internal: execute database work with dropped privileges. Speaks a
    /// line-delimited JSON protocol on stdin/stdout and is not a stable
    /// interface.
    #[command(name = "__db-agent", hide = true)]
    DbAgent,
    /// Internal: merge postvec into a shared_preload_libraries value and print
    /// the result. Used by the container entrypoint, which must apply exactly
    /// the same list grammar the server does; not a stable interface.
    #[command(name = "__preload-merge", hide = true)]
    PreloadMerge(PreloadMergeArgs),
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Download models (and everything they need) from the registry into the
    /// engine root and verify them. Installing is not activating: the models
    /// land deactivated — `postvec model activate` turns them on.
    Pull(ModelPullArgs),
    /// Replace installed models with newer registry revisions of the same
    /// name, in place. Never implicit: `pull` only ever installs.
    Upgrade(ModelUpgradeArgs),
    /// List installed models — or, with --available, the registry catalogue.
    /// Neither form needs a database login when the engine root is readable.
    Ls(ModelLsArgs),
    /// Show one model: registry identity, licence, dependencies, disk and
    /// load state.
    Show(ModelShowArgs),
    /// Remove a CLI-installed model from the engine root.
    Rm(ModelRmArgs),
    /// Turn models on: mark them enabled on disk (surviving restarts), load
    /// them into a running embedded engine, and refresh the SQL model cache.
    Activate(ModelActivateArgs),
    /// Turn models off: unload them from a running embedded engine and mark
    /// them disabled on disk, so a restart does not bring them back.
    Deactivate(ModelDeactivateArgs),
}

impl ModelCommand {
    /// The command name as it appears in output envelopes and error reports.
    pub fn name(&self) -> &'static str {
        match self {
            ModelCommand::Pull(_) => "model pull",
            ModelCommand::Upgrade(_) => "model upgrade",
            ModelCommand::Ls(_) => "model ls",
            ModelCommand::Show(_) => "model show",
            ModelCommand::Rm(_) => "model rm",
            ModelCommand::Activate(_) => "model activate",
            ModelCommand::Deactivate(_) => "model deactivate",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Configure a provider: write its providers.d file (0600), verify the
    /// key with one live embed per model, and reload the running host.
    /// Boxed: the add flags dwarf the other subcommands' and clap clones
    /// the whole enum per dispatch.
    Add(Box<ProviderAddArgs>),
    /// List configured providers: key source (never the key), models, dims,
    /// and whether the running host currently serves them.
    Ls(ProviderLsArgs),
    /// Remove a provider file, or one model entry from it.
    Rm(ProviderRmArgs),
    /// Run the verification probe against a configured provider.
    Test(ProviderTestArgs),
}

impl ProviderCommand {
    /// The command name as it appears in output envelopes and error reports.
    pub fn name(&self) -> &'static str {
        match self {
            ProviderCommand::Add(_) => "provider add",
            ProviderCommand::Ls(_) => "provider ls",
            ProviderCommand::Rm(_) => "provider rm",
            ProviderCommand::Test(_) => "provider test",
        }
    }
}

#[derive(Debug, Args, Clone)]
pub struct ProviderAddArgs {
    /// Connector type: openai | openrouter | mistral | google | cohere |
    /// aws | univec (gemini is accepted as an alias for google).
    #[arg(value_name = "TYPE")]
    pub provider_type: String,

    /// File stem for the providers.d file. Default: the canonical type.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Provider model id (e.g. text-embedding-3-small). Repeatable.
    #[arg(
        long = "model",
        value_name = "ID",
        required_unless_present = "convert_source",
        conflicts_with = "convert_source"
    )]
    pub models: Vec<String>,

    /// univec only: add a hosted CONVERTER entry instead of embed models —
    /// this is the provider-side id of the SOURCE vector space. Requires
    /// --convert-target, --source-model, --target-model and --source-dim.
    /// postvec.migrate(strategy => 'convert') and postvec.convert() route
    /// through the entry; it serves no embedding.
    #[arg(long, value_name = "ID")]
    pub convert_source: Option<String>,

    /// Converter only: the provider-side id of the TARGET vector space.
    #[arg(long, value_name = "ID")]
    pub convert_target: Option<String>,

    /// Converter only: the postvec-side public name of the SOURCE space —
    /// what a bound column's `model` says (the resolver's vocabulary, not
    /// the provider's).
    #[arg(long, value_name = "NAME")]
    pub source_model: Option<String>,

    /// Converter only: the postvec-side public name of the TARGET space —
    /// what postvec.migrate() is called with.
    #[arg(long, value_name = "NAME")]
    pub target_model: Option<String>,

    /// Converter only: the SOURCE space's dimension (--dim is the target's;
    /// the verification probe measures it when omitted).
    #[arg(long, value_name = "N")]
    pub source_dim: Option<u32>,

    /// Converter only: the entry's public name. Default:
    /// univec-convert-<source-model>-to-<target-model>.
    #[arg(long, value_name = "NAME")]
    pub converter_name: Option<String>,

    /// Reference this key file from the provider file (the file itself must
    /// be 0600; it is referenced, never copied).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["api_key_env", "key_stdin"])]
    pub api_key_file: Option<PathBuf>,

    /// Record this environment variable as the key source. It must be
    /// present in the POSTMASTER's (or the server unit's) environment — not
    /// this shell's.
    #[arg(long, value_name = "VAR", conflicts_with = "key_stdin")]
    pub api_key_env: Option<String>,

    /// Read the key from stdin (for pipes). Without any key source and a
    /// TTY, a hidden prompt asks for it. A key is never accepted as a
    /// command-line value: argv is world-observable.
    #[arg(long)]
    pub key_stdin: bool,

    /// AWS only: the Bedrock region. The key source supplies the Bedrock
    /// bearer token; for static SigV4 credentials, edit the provider file's
    /// access_key_id/secret_access_key fields (file/env/inline triads).
    #[arg(long, value_name = "REGION")]
    pub region: Option<String>,

    /// Optional base-URL override (Azure-style fronts, mock servers).
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// Vector dimension, when a single unknown model is added with
    /// --no-verify (otherwise the catalog or the verification probe fills
    /// it in).
    #[arg(long, value_name = "N")]
    pub dim: Option<u32>,

    /// Write into <DIR>/providers.d (or <DIR> itself when it already is
    /// one) instead of the selected cluster's providers path. This is how
    /// remote postvec-server roots are administered. Default:
    /// POSTVEC_PROVIDERS_PATH, then the selected cluster's providers path.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Skip the live verification embed (it costs one paid API call per
    /// model).
    #[arg(long)]
    pub no_verify: bool,

    /// Acknowledge that existing columns bound to these model names start
    /// sending their source text to the provider on the next worker cycle.
    /// Required with --yes when any column is affected; never implied by
    /// --yes.
    #[arg(long)]
    pub acknowledge_in_use: bool,

    /// Skip the confirmation prompt. Required for mutation without a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ProviderLsArgs {
    /// Inspect <DIR>/providers.d (or <DIR> itself) instead of the selected
    /// cluster's providers path. Default: POSTVEC_PROVIDERS_PATH, then the
    /// selected cluster's providers path.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct ProviderRmArgs {
    /// Provider name (the providers.d file stem).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Remove only this model entry (public name or provider id) instead of
    /// the whole file.
    #[arg(long = "model", value_name = "ID")]
    pub model: Option<String>,

    /// Operate on <DIR>/providers.d (or <DIR> itself) instead of the
    /// selected cluster's providers path. Default: POSTVEC_PROVIDERS_PATH,
    /// then the selected cluster's providers path.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Acknowledge that managed columns lose their embedding route to the
    /// removed model(s). Required with --yes when any column is affected;
    /// never implied by --yes.
    #[arg(long)]
    pub acknowledge_in_use: bool,

    /// Skip the confirmation prompt. Required for mutation without a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ProviderTestArgs {
    /// Provider name (the providers.d file stem).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Probe only this model (public name or provider id).
    #[arg(long = "model", value_name = "ID")]
    pub model: Option<String>,

    /// Inspect <DIR>/providers.d (or <DIR> itself) instead of the selected
    /// cluster's providers path. Default: POSTVEC_PROVIDERS_PATH, then the
    /// selected cluster's providers path.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct ModelPullArgs {
    /// Registry model name. Repeatable; dependencies and postvec companions
    /// are added automatically.
    #[arg(value_name = "NAME", required = true)]
    pub names: Vec<String>,

    /// Manage this engine root directly instead of the selected cluster's.
    /// Filesystem management only: no activation, no database refresh.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Read the API key from this file instead of the credential store.
    #[arg(long, value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,

    /// Acknowledge a notice-policy terms document non-interactively, as the
    /// exact <ID>@<VERSION> the plan names (repeatable). Version-specific:
    /// a token for a document this run does not need is refused as stale.
    /// `--yes` never stands in for it.
    #[arg(long = "accept-license", value_name = "ID@VERSION")]
    pub accept_license: Vec<String>,

    /// Skip the confirmation prompt. Required for mutation without a TTY.
    /// Answers only the ordinary mutation confirmation — never a terms
    /// acknowledgement.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl ModelPullArgs {
    pub fn validated(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for raw in &self.names {
            crate::registry::index::valid_model_name(raw).map_err(CliError::usage)?;
            if !names.contains(raw) {
                names.push(raw.clone());
            }
        }
        if let Some(path) = &self.path {
            validate::absolute_path(path, "--path")?;
        }
        Ok(names)
    }
}

#[derive(Debug, Args, Clone)]
pub struct ModelUpgradeArgs {
    /// Installed model name. Repeatable; new dependencies the replacement
    /// needs are installed alongside it.
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,

    /// Upgrade every CLI-installed model the registry offers a newer
    /// revision of.
    #[arg(long, conflicts_with = "names")]
    pub all: bool,

    /// Manage this engine root directly instead of the selected cluster's.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Read the API key from this file instead of the credential store.
    #[arg(long, value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,

    /// Acknowledge a notice-policy terms document non-interactively, as the
    /// exact <ID>@<VERSION> the plan names (repeatable). Version-specific:
    /// a token for a document this run does not need is refused as stale.
    /// `--yes` never stands in for it.
    #[arg(long = "accept-license", value_name = "ID@VERSION")]
    pub accept_license: Vec<String>,

    /// Skip the confirmation prompt. Required for mutation without a TTY.
    /// Answers only the ordinary mutation confirmation — never a terms
    /// acknowledgement.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl ModelUpgradeArgs {
    pub fn validated(&self) -> Result<Vec<String>> {
        if self.names.is_empty() && !self.all {
            return Err(CliError::usage(
                "name at least one installed model, or pass --all to upgrade every CLI-installed \
                 model with a newer revision",
            ));
        }
        let mut names = Vec::new();
        for raw in &self.names {
            crate::registry::index::valid_model_name(raw).map_err(CliError::usage)?;
            if !names.contains(raw) {
                names.push(raw.clone());
            }
        }
        if let Some(path) = &self.path {
            validate::absolute_path(path, "--path")?;
        }
        Ok(names)
    }
}

#[derive(Debug, Args, Clone)]
pub struct ModelLsArgs {
    /// List the registry catalogue instead of installed models.
    #[arg(long)]
    pub available: bool,

    /// Inspect this engine root instead of the selected cluster's.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR", conflicts_with = "available")]
    pub path: Option<PathBuf>,

    /// Read the API key from this file instead of the credential store.
    #[arg(long, requires = "available", value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct ModelShowArgs {
    /// Model name (registry name / directory name).
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Inspect this engine root instead of the selected cluster's.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Hash every installed file against the receipt. Reads the whole model.
    #[arg(long)]
    pub verify: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ModelRmArgs {
    /// Model to remove. Repeatable.
    #[arg(value_name = "NAME", required = true)]
    pub names: Vec<String>,

    /// Manage this engine root directly instead of the selected cluster's.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Remove even when another installed model depends on this one. Does not
    /// stand in for --acknowledge-in-use.
    #[arg(long)]
    pub force: bool,

    /// Acknowledge that managed columns lose their embedding route to this
    /// model and will stop working — directly, or through a converter/bridge
    /// chain it is part of. Required with --yes when any column is affected;
    /// never implied by --yes or --force.
    #[arg(long)]
    pub acknowledge_in_use: bool,
}

#[derive(Debug, Args, Clone)]
pub struct ModelActivateArgs {
    /// Model to activate. Repeatable; a model's deactivated dependencies are
    /// enabled with it, because the engine will not load one without them.
    #[arg(value_name = "NAME")]
    pub names: Vec<String>,

    /// Activate every eligible CLI-installed model in the engine root. This
    /// is also what a bare `postvec model activate` does.
    #[arg(long, conflicts_with = "names")]
    pub all: bool,

    /// Manage this engine root directly instead of the selected cluster's.
    /// Marks the models enabled on disk only: no engine load, no SQL refresh.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Show what would change and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl ModelActivateArgs {
    /// The requested names. Empty means every eligible model — a bare
    /// `activate` kept its original catch-up meaning.
    pub fn validated(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for raw in &self.names {
            crate::registry::index::valid_model_name(raw).map_err(CliError::usage)?;
            if !names.contains(raw) {
                names.push(raw.clone());
            }
        }
        if let Some(path) = &self.path {
            validate::absolute_path(path, "--path")?;
        }
        Ok(names)
    }
}

#[derive(Debug, Args, Clone)]
pub struct ModelDeactivateArgs {
    /// Model to deactivate. Repeatable. There is deliberately no `--all`:
    /// turning every model off in one flag is how search goes down by
    /// accident.
    #[arg(value_name = "NAME", required = true)]
    pub names: Vec<String>,

    /// Manage this engine root directly instead of the selected cluster's.
    /// Marks the model disabled on disk only: no engine unload, no SQL
    /// refresh, and no database to check for columns still using it.
    /// Default: POSTVEC_PATH, then the selected cluster's root,
    /// then /opt/postvec when no cluster exists.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Deactivate even when another enabled installed model depends on this
    /// one. Does not stand in for --acknowledge-in-use.
    #[arg(long)]
    pub force: bool,

    /// Acknowledge that managed columns lose their embedding route to this
    /// model and will stop working — directly, or through a converter/bridge
    /// chain it is part of. Required with --yes when any column is affected;
    /// never implied by --yes or --force.
    #[arg(long)]
    pub acknowledge_in_use: bool,

    /// Skip the ordinary confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Show what would change and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

impl ModelDeactivateArgs {
    pub fn validated(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for raw in &self.names {
            crate::registry::index::valid_model_name(raw).map_err(CliError::usage)?;
            if !names.contains(raw) {
                names.push(raw.clone());
            }
        }
        if let Some(path) = &self.path {
            validate::absolute_path(path, "--path")?;
        }
        Ok(names)
    }
}

#[derive(Debug, Args, Clone)]
pub struct LoginArgs {
    /// Read the API key from this file instead of prompting. The key is
    /// never accepted as a command-line value: argv is world-observable.
    #[arg(long, value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct WhoamiArgs {
    /// Also consider this key file, the way model commands would.
    #[arg(long, value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,
}

#[derive(Debug, Args, Clone)]
pub struct PreloadMergeArgs {
    /// The operator-supplied list. Empty or absent yields just `postvec`.
    #[arg(value_name = "LIST", default_value = "")]
    pub value: String,
}

#[derive(Debug, Args, Clone)]
pub struct SetupArgs {
    /// Database to serve. Repeatable; a comma-separated list is also accepted.
    #[arg(long, required = true, value_name = "NAME", value_delimiter = ',')]
    pub database: Vec<String>,

    /// Host the inference engine inside the launcher process instead of
    /// calling remote inference nodes (postvec-server).
    #[arg(long, conflicts_with_all = ["grpc", "http"])]
    pub embedded: bool,

    /// Absolute engine root (contains libs/ and models/). Defaults to
    /// /opt/postvec, where the packages install theirs.
    #[arg(long, requires = "embedded", value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// Model to preload. Repeatable. Omit to load every enabled model found
    /// under <root>/models.
    #[arg(
        long,
        requires = "embedded",
        value_name = "NAME",
        value_delimiter = ','
    )]
    pub model: Vec<String>,

    /// Directory of external-provider connector files (providers.d) the
    /// embedded host reads. Omit to keep the extension's default
    /// (/etc/postvec/providers.d). A path, never a credential.
    #[arg(long, requires = "embedded", value_name = "DIR")]
    pub providers_path: Option<PathBuf>,

    /// Loopback address for the embedded gRPC listener (default
    /// 127.0.0.1:33433).
    #[arg(long, requires = "embedded", value_name = "ADDR")]
    pub embedded_grpc_listen: Option<SocketAddr>,

    /// Loopback address for the embedded GET /config listener (default
    /// 127.0.0.1:33434).
    #[arg(long, requires = "embedded", value_name = "ADDR")]
    pub embedded_http_listen: Option<SocketAddr>,

    /// Inference gRPC endpoint (postvec-server) as host:port. Repeatable;
    /// order is preserved
    /// (it drives round-robin).
    #[arg(
        long,
        required_unless_present = "embedded",
        value_name = "HOST:PORT",
        value_delimiter = ','
    )]
    pub grpc: Vec<String>,

    /// Inference HTTP base URL used for GET /config discovery. Repeatable.
    #[arg(
        long,
        required_unless_present = "embedded",
        value_name = "URL",
        value_delimiter = ','
    )]
    pub http: Vec<String>,

    /// Change the cluster-wide inference mode. Required to switch between
    /// remote and embedded once databases are configured.
    #[arg(long)]
    pub switch_mode: bool,

    /// Skip the confirmation prompt. Required for mutation without a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Apply everything but leave the restart (and therefore the POSTMASTER
    /// settings) pending. Exits 4.
    #[arg(long)]
    pub no_restart: bool,

    /// Finish with a warning instead of failing when the configured engine is
    /// not reachable yet.
    #[arg(long)]
    pub allow_unreachable: bool,
}

#[derive(Debug, Args, Clone)]
pub struct UninstallArgs {
    /// Database to stop serving. Repeatable. Either this or --all.
    #[arg(
        long,
        value_name = "NAME",
        value_delimiter = ',',
        conflicts_with = "all"
    )]
    pub database: Vec<String>,

    /// Every database: those the launcher is configured to serve plus any
    /// other database in the cluster where the extension is installed.
    #[arg(long)]
    pub all: bool,

    /// Also drop what postvec created to hold vectors: the shadow vector
    /// columns and, for chunked entries, the managed chunk destination
    /// tables and views. Destroys stored embeddings. Adopted (user-owned)
    /// columns are never dropped.
    #[arg(long)]
    pub drop_columns: bool,

    /// With --drop-columns: keep the chunk destination tables and views as
    /// ordinary frozen tables, dropping only the shadow columns.
    #[arg(long, requires = "drop_columns")]
    pub keep_destinations: bool,

    /// Required acknowledgement for --drop-columns.
    #[arg(long, requires = "drop_columns")]
    pub acknowledge_data_loss: bool,

    /// With --all: after the SQL and configuration removal, also delete
    /// postvec's files on this host — pulled/manual models and CLI state under
    /// the engine root, provider connector files, the CLI's state and lock
    /// directories, and an unpackaged extension library. Package-owned files
    /// are never deleted; the packages to purge are printed instead.
    #[arg(
        long,
        requires = "all",
        conflicts_with_all = ["keep_config", "no_restart"]
    )]
    pub purge: bool,

    /// Remove SQL state only; leave cluster configuration untouched.
    #[arg(long)]
    pub keep_config: bool,

    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,

    /// Show the plan and exit without changing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Apply SQL and configuration changes but leave the restart pending.
    #[arg(long)]
    pub no_restart: bool,
}

#[derive(Debug, Args, Clone)]
pub struct DoctorArgs {
    /// Database to inspect. Repeatable. Default: every configured database.
    #[arg(long, value_name = "NAME", value_delimiter = ',')]
    pub database: Vec<String>,

    /// Run bounded active checks, notably heartbeat advancement. Still
    /// read-only.
    #[arg(long)]
    pub deep: bool,

    /// Exit non-zero for WARN as well as FAIL.
    #[arg(long)]
    pub strict: bool,

    /// Engine root to diagnose when the cluster's postvec.path cannot be
    /// read (a snippet-only view, say). Never written to any configuration
    /// file.
    #[arg(long, value_name = "DIR")]
    pub path: Option<PathBuf>,

    /// TLS policy for the HTTP discovery probes.
    #[arg(long, value_enum, default_value_t = TlsPolicy::ExtensionCompatible)]
    pub tls: TlsPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TlsPolicy {
    /// Match the extension: accept invalid/self-signed certificates, but
    /// report when verification would have failed.
    ExtensionCompatible,
    /// Require a verifiable certificate chain.
    Strict,
}

fn parse_duration(raw: &str) -> std::result::Result<Duration, String> {
    humantime::parse_duration(raw).map_err(|e| format!("{e}"))
}

/// Inference deployment mode, as selected on the command line. Mirrors
/// `postvec.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Grpc,
    Embedded,
}

impl Mode {
    pub fn as_guc(self) -> &'static str {
        match self {
            Mode::Grpc => "grpc",
            Mode::Embedded => "embedded",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_guc())
    }
}

impl SetupArgs {
    pub fn mode(&self) -> Mode {
        if self.embedded {
            Mode::Embedded
        } else {
            Mode::Grpc
        }
    }

    /// Validate and normalize every user-supplied value. Runs before any
    /// host access so a typo costs nothing.
    pub fn validated(&self) -> Result<ValidatedSetup> {
        let databases = validate::database_list(&self.database)?;
        match self.mode() {
            Mode::Grpc => {
                let grpc = validate::grpc_endpoints(&self.grpc)?;
                let http = validate::http_endpoints(&self.http)?;
                Ok(ValidatedSetup {
                    databases,
                    target: ModeTarget::Grpc { grpc, http },
                })
            }
            Mode::Embedded => {
                // The packaged engine root, which is also what
                // `postvec.path` defaults to — so on a package
                // install `--embedded` needs no path at all. A root that is
                // not there is caught by the engine preflight below, from the
                // PostgreSQL account's perspective, which is the only vantage
                // point that can answer the question.
                let path = self
                    .path
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_ENGINE_ROOT));
                // Syntax only: existence and readability are checked from the
                // PostgreSQL account's perspective during engine preflight.
                let path = validate::absolute_path(&path, "--path")?;
                let providers_path = self
                    .providers_path
                    .as_deref()
                    .map(|p| validate::absolute_path(p, "--providers-path"))
                    .transpose()?;
                let models = validate::model_list(&self.model)?;
                let grpc_listen = self.embedded_grpc_listen;
                let http_listen = self.embedded_http_listen;
                if let (Some(a), Some(b)) = (grpc_listen, http_listen) {
                    if a == b {
                        return Err(CliError::usage(
                            "--embedded-grpc-listen and --embedded-http-listen must differ",
                        ));
                    }
                }
                for (addr, flag) in [
                    (grpc_listen, "--embedded-grpc-listen"),
                    (http_listen, "--embedded-http-listen"),
                ] {
                    if let Some(addr) = addr {
                        validate::loopback_listener(addr, flag)?;
                    }
                }
                Ok(ValidatedSetup {
                    databases,
                    target: ModeTarget::Embedded {
                        path,
                        providers_path,
                        models,
                        grpc_listen,
                        http_listen,
                    },
                })
            }
        }
    }
}

impl UninstallArgs {
    /// The explicitly named databases. Empty with `--all`, whose targets are
    /// discovered from the cluster.
    pub fn validated(&self) -> Result<Vec<String>> {
        // Repeated here: clap treats a `requires` as satisfied when the named
        // argument's conflict partner is present, so `--database d --purge`
        // parses.
        if self.purge && !self.all {
            return Err(CliError::usage(
                "--purge deletes postvec's files on this host and only makes sense with --all",
            ));
        }
        if self.all {
            return Ok(Vec::new());
        }
        if self.database.is_empty() {
            return Err(CliError::usage(
                "name the database(s) to remove postvec from with --database, or pass --all",
            ));
        }
        validate::database_list(&self.database)
    }

    /// Whether `postvec.uninstall()` should drop the managed chunk
    /// destinations as well as the shadow columns.
    pub fn drop_destinations(&self) -> bool {
        self.drop_columns && !self.keep_destinations
    }
}

impl DoctorArgs {
    pub fn validated(&self) -> Result<Vec<String>> {
        validate::database_list_allow_empty(&self.database)
    }
}

/// Normalized `setup` input.
#[derive(Debug, Clone)]
pub struct ValidatedSetup {
    pub databases: Vec<String>,
    pub target: ModeTarget,
}

#[derive(Debug, Clone)]
pub enum ModeTarget {
    Grpc {
        grpc: Vec<crate::validate::GrpcEndpoint>,
        http: Vec<crate::validate::HttpEndpoint>,
    },
    Embedded {
        path: PathBuf,
        /// providers.d override; `None` keeps the extension's default path.
        providers_path: Option<PathBuf>,
        models: Vec<String>,
        grpc_listen: Option<SocketAddr>,
        http_listen: Option<SocketAddr>,
    },
}

impl ModeTarget {
    pub fn mode(&self) -> Mode {
        match self {
            ModeTarget::Grpc { .. } => Mode::Grpc,
            ModeTarget::Embedded { .. } => Mode::Embedded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("postvec").chain(args.iter().copied()))
    }

    #[test]
    fn setup_requires_endpoints_in_remote_mode() {
        assert!(parse(&["setup", "--database", "d"]).is_err());
        assert!(parse(&["setup", "--database", "d", "--grpc", "h:1"]).is_err());
        assert!(parse(&[
            "setup",
            "--database",
            "d",
            "--grpc",
            "h:1",
            "--http",
            "http://h"
        ])
        .is_ok());
    }

    #[test]
    fn embedded_conflicts_with_endpoints() {
        assert!(parse(&["setup", "--database", "d", "--embedded", "--grpc", "h:1"]).is_err());
        assert!(parse(&["setup", "--database", "d", "--embedded", "--path", "/x"]).is_ok());
    }

    #[test]
    fn embedded_only_flags_require_embedded() {
        assert!(parse(&["setup", "--database", "d", "--path", "/x"]).is_err());
        assert!(parse(&["setup", "--database", "d", "--model", "m"]).is_err());
    }

    /// `--embedded` alone is a complete instruction on a package install:
    /// the path defaults to where the packages put the engine root, which is
    /// also what the extension's `postvec.path` defaults to.
    #[test]
    fn embedded_defaults_the_path_to_the_packaged_engine_root() {
        let cli = parse(&["setup", "--database", "d", "--embedded"]).unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        let validated = args.validated().expect("--embedded needs no --path");
        match validated.target {
            ModeTarget::Embedded { path, .. } => assert_eq!(
                path,
                PathBuf::from("/opt/postvec"),
                "must match postvec/src/gucs.rs::DEFAULT_ENGINE_PATH"
            ),
            other => panic!("expected embedded, got {other:?}"),
        }
    }

    /// An explicit path still wins, and still has to be absolute.
    #[test]
    fn an_explicit_embedded_path_overrides_the_default() {
        let cli = parse(&["setup", "--database", "d", "--embedded", "--path", "/srv/e"]).unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        match args.validated().unwrap().target {
            ModeTarget::Embedded { path, .. } => assert_eq!(path, PathBuf::from("/srv/e")),
            other => panic!("expected embedded, got {other:?}"),
        }
        let cli = parse(&["setup", "--database", "d", "--embedded", "--path", "rel"]).unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        assert!(
            args.validated().is_err(),
            "a relative --path is still refused"
        );
    }

    #[test]
    fn drop_columns_gates_acknowledgement() {
        assert!(parse(&["uninstall", "--database", "d", "--acknowledge-data-loss"]).is_err());
        assert!(parse(&[
            "uninstall",
            "--database",
            "d",
            "--drop-columns",
            "--acknowledge-data-loss"
        ])
        .is_ok());
    }

    #[test]
    fn uninstall_targets_are_database_or_all() {
        // Neither: a usage error at validation time (clap cannot express
        // "one of a repeatable and a flag" without a group).
        let cli = parse(&["uninstall"]).unwrap();
        let Command::Uninstall(args) = cli.command else {
            unreachable!()
        };
        assert!(args.validated().is_err());
        // Both: refused by clap.
        assert!(parse(&["uninstall", "--all", "--database", "d"]).is_err());
        let cli = parse(&["uninstall", "--all"]).unwrap();
        let Command::Uninstall(args) = cli.command else {
            unreachable!()
        };
        assert!(args.validated().unwrap().is_empty());
    }

    #[test]
    fn purge_needs_all_and_a_real_teardown() {
        assert!(parse(&["uninstall", "--purge"]).is_err());
        // clap lets this one through (the conflict partner of `all` counts
        // as satisfying `requires`); validation catches it.
        let cli = parse(&["uninstall", "--database", "d", "--purge"]).unwrap();
        let Command::Uninstall(args) = cli.command else {
            unreachable!()
        };
        assert!(args.validated().is_err());
        assert!(parse(&["uninstall", "--all", "--purge", "--keep-config"]).is_err());
        assert!(parse(&["uninstall", "--all", "--purge", "--no-restart"]).is_err());
        assert!(parse(&["uninstall", "--all", "--purge", "--yes"]).is_ok());
    }

    #[test]
    fn drop_columns_covers_destinations_unless_kept() {
        assert!(parse(&["uninstall", "--all", "--keep-destinations"]).is_err());
        let cli = parse(&[
            "uninstall",
            "--all",
            "--drop-columns",
            "--acknowledge-data-loss",
        ])
        .unwrap();
        let Command::Uninstall(args) = cli.command else {
            unreachable!()
        };
        assert!(args.drop_destinations());
        let cli = parse(&[
            "uninstall",
            "--all",
            "--drop-columns",
            "--keep-destinations",
            "--acknowledge-data-loss",
        ])
        .unwrap();
        let Command::Uninstall(args) = cli.command else {
            unreachable!()
        };
        assert!(!args.drop_destinations());
    }

    #[test]
    fn comma_separated_lists_normalize() {
        let cli = parse(&[
            "setup",
            "--database",
            "a,b",
            "--grpc",
            "h:1,h:2",
            "--http",
            "http://h",
        ])
        .unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        assert_eq!(args.database, ["a", "b"]);
        assert_eq!(args.grpc, ["h:1", "h:2"]);
    }

    #[test]
    fn config_dir_requires_pg_config() {
        assert!(parse(&["doctor", "--config-dir", "/etc/x"]).is_err());
        assert!(parse(&["doctor", "--config-dir", "/etc/x", "--pg-config", "/p"]).is_ok());
    }

    #[test]
    fn equal_embedded_listeners_are_rejected() {
        let cli = parse(&[
            "setup",
            "--database",
            "d",
            "--embedded",
            "--path",
            "/x",
            "--embedded-grpc-listen",
            "127.0.0.1:1",
            "--embedded-http-listen",
            "127.0.0.1:1",
        ])
        .unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        assert!(args.validated().unwrap_err().to_string().contains("differ"));
    }

    #[test]
    fn non_loopback_embedded_listener_is_rejected() {
        let cli = parse(&[
            "setup",
            "--database",
            "d",
            "--embedded",
            "--path",
            "/x",
            "--embedded-grpc-listen",
            "192.0.2.2:33433",
        ])
        .unwrap();
        let Command::Setup(args) = cli.command else {
            unreachable!()
        };
        assert!(args
            .validated()
            .unwrap_err()
            .to_string()
            .contains("loopback"));
    }
}
