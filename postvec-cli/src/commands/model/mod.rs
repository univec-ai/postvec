//! `postvec model ...`: model management for an engine root.
//!
//! Target resolution, in order: an explicit `--path DIR` wins and is
//! pure filesystem management (no cluster, no activation, no database
//! refresh). Otherwise the selected cluster's effective settings decide.
//! Embedded mode manages (and can activate against)
//! `postvec.ninference_path`. Remote mode can only list what the
//! configured nodes advertise and refuses local mutation: a remote fleet
//! is administered through `nin`, not through this CLI.

pub mod activate;
pub mod admin;
pub mod deactivate;
pub mod ls;
pub mod pull;
pub mod rm;
pub mod show;
pub mod terms;

use crate::cli::{Cli, Mode};
use crate::cluster::Cluster;
use crate::commands::{collect, Context};
use crate::config::owned;
use crate::error::{CliError, Result};
use crate::facts::SettingsSnapshot;
use crate::output::Output;
use crate::registry::auth::{self, Credential};
use crate::registry::client::RegistryClient;
use crate::registry::index::Index;
use crate::registry::root::{InstalledModel, ModelRoot, Ownership};
use crate::registry::urls;
use std::collections::BTreeSet;
use std::path::Path;

/// What a model command works against.
pub enum ModelTarget {
    /// `--path DIR`: a local engine root, no cluster involved.
    Path(ModelRoot),
    /// The selected cluster, embedded mode: root management plus activation.
    ///
    /// `context` is `None` when the command could not open a peer-authenticated
    /// session and is working from the owned configuration snippet instead.
    /// Listing and filesystem work still run; SQL-cache refresh does not.
    Embedded {
        root: ModelRoot,
        settings: SettingsSnapshot,
        context: Option<Box<Context>>,
        cluster_id: String,
    },
    /// The selected cluster, remote (gRPC) mode: listing only.
    Remote {
        settings: SettingsSnapshot,
        context: Option<Box<Context>>,
        cluster_id: String,
    },
}

impl ModelTarget {
    /// The engine root, where one exists.
    pub fn root(&self) -> Option<&ModelRoot> {
        match self {
            ModelTarget::Path(root) => Some(root),
            ModelTarget::Embedded { root, .. } => Some(root),
            ModelTarget::Remote { .. } => None,
        }
    }

    /// The label that stands in for `cluster` in result envelopes.
    pub fn label(&self) -> String {
        match self {
            ModelTarget::Path(root) => format!("path:{}", root.root.display()),
            ModelTarget::Embedded { cluster_id, .. } | ModelTarget::Remote { cluster_id, .. } => {
                cluster_id.clone()
            }
        }
    }
}

/// Resolve the model-command target for **read-only** commands (`ls`, `show`).
///
/// A live database session is preferred (effective GUCs, loaded state).
/// It is not required: if peer authentication fails, the owned
/// `99-postvec.conf` is enough to find the engine root or the remote
/// endpoints. That is why `postvec model ls` does not need `sudo`.
///
/// Mutating commands must use [`resolve_target_for_mutation`]: a cluster
/// pull/rm that proceeds without a login writes files as the invoking
/// user and then cannot refresh `postvec.models`, after which `sudo`
/// refuses the now-foreign-owned tree.
pub async fn resolve_target(
    cli: &Cli,
    path: Option<&Path>,
    output: &Output,
) -> Result<ModelTarget> {
    resolve_target_inner(cli, path, output, /*allow_snippet=*/ true).await
}

/// Resolve a target that will mutate the engine root or refresh SQL
/// caches (`pull`, `upgrade`, `rm`, `activate`).
///
/// `--path` stays filesystem-only. A cluster target needs a live
/// database session — the snippet fallback is for listing only.
pub async fn resolve_target_for_mutation(
    cli: &Cli,
    path: Option<&Path>,
    output: &Output,
) -> Result<ModelTarget> {
    resolve_target_inner(cli, path, output, /*allow_snippet=*/ false).await
}

async fn resolve_target_inner(
    cli: &Cli,
    path: Option<&Path>,
    output: &Output,
    allow_snippet: bool,
) -> Result<ModelTarget> {
    if let Some(path) = path {
        let path = crate::validate::absolute_path(path, "--path")?;
        return Ok(ModelTarget::Path(ModelRoot::new(path)));
    }
    // An operator-supplied URI is an explicit authentication choice. Do not
    // fall back to a local snippet that may belong to a different server.
    if cli.database_url.is_some() {
        let context = Context::open(cli, output).await?;
        return target_from_live(context).await;
    }

    let cluster = Context::discover_local(cli, output).await?;
    let cluster_id = cluster.identity.id.clone();
    match Context::connect_to(cli, cluster.clone(), output).await {
        Ok(context) => target_from_live(context).await,
        Err(error) if allow_snippet => match target_from_snippet(&cluster, cluster_id, output) {
            Some(target) => Ok(target),
            None => Err(error),
        },
        Err(error) => Err(CliError::precondition(format!(
            "{error}; installing or removing models against this cluster also needs a \
             database login so postvec.models can be refreshed"
        ))
        .with_fix(
            "rerun with sudo (it drops to the cluster owner), as the cluster owner, or \
             pass --database-url. `model ls` and `model show` do not need that. For \
             files only, pass --path DIR",
        )),
    }
}

async fn target_from_live(mut context: Context) -> Result<ModelTarget> {
    let snapshot = collect::cluster_snapshot(&mut context).await?;
    target_from_settings(snapshot.settings, Some(context), None)
}

/// Read mode / path / endpoints from the owned snippet. The state file is
/// 0600 and unreadable to a regular user; the snippet is 0644.
fn target_from_snippet(
    cluster: &Cluster,
    cluster_id: String,
    output: &Output,
) -> Option<ModelTarget> {
    let paths = cluster.owned_paths().ok()?;
    let content = match paths.read_config() {
        Ok(Some(content)) => content,
        _ => return None,
    };
    let settings = owned::settings_from_snippet(&content);
    let config_path = paths.config.clone();
    match target_from_settings(settings, None, Some(cluster_id)) {
        Ok(target) => {
            output.note(&format!(
                "could not log in as the cluster owner; using {}",
                config_path.display()
            ));
            Some(target)
        }
        Err(_) => None,
    }
}

fn target_from_settings(
    settings: SettingsSnapshot,
    context: Option<Context>,
    cluster_id: Option<String>,
) -> Result<ModelTarget> {
    let cluster_id = cluster_id
        .or_else(|| context.as_ref().map(|ctx| ctx.cluster.identity.id.clone()))
        .unwrap_or_else(|| "cluster".to_string());
    let boxed = context.map(Box::new);
    match settings.mode() {
        Some(Mode::Embedded) => {
            let Some(root) = settings.ninference_path() else {
                return Err(CliError::precondition(
                    "the cluster is in embedded mode but postvec.ninference_path is unset \
                     (the server may inherit NINFERENCE_PATH from its environment, which \
                     this CLI cannot observe)",
                )
                .with_fix("pass --path <DIR> to manage the root directly"));
            };
            Ok(ModelTarget::Embedded {
                root: ModelRoot::new(root),
                settings,
                context: boxed,
                cluster_id,
            })
        }
        Some(Mode::Grpc) => Ok(ModelTarget::Remote {
            settings,
            context: boxed,
            cluster_id,
        }),
        None => Err(CliError::precondition(format!(
            "the cluster's postvec.mode is unparseable ({:?})",
            settings.raw_mode().unwrap_or("unset")
        ))
        .with_fix("fix postvec.mode, or pass --path <DIR> to manage a root directly")),
    }
}

/// A target that may be mutated: an engine root. Remote mode refuses here.
pub fn require_root<'a>(target: &'a ModelTarget, action: &str) -> Result<&'a ModelRoot> {
    target.root().ok_or_else(|| {
        CliError::precondition(format!(
            "the selected cluster uses remote inference; {action} would change nothing it uses"
        ))
        .with_fix(
            "remote nodes are administered with `nin`; pass --path <DIR> to manage a local \
             standalone engine root",
        )
    })
}

/// Resolve the channel. A selected credential must authenticate or the
/// command stops (no silent downgrade). No credential means the public
/// channel. Returns the validated index and the credential used, if any.
pub async fn fetch_channel_index(
    timeout: std::time::Duration,
    api_key_file: Option<&Path>,
    output: &Output,
) -> Result<(Index, Option<Credential>)> {
    let credential = auth::resolve(api_key_file)?;
    match credential {
        Some(credential) => {
            let target = urls::authenticated_index_url();
            if target.overridden {
                output.note("registry index override is active (testing only)");
            }
            let client = RegistryClient::new(timeout, target.overridden)?;
            let index = client
                .fetch_index(&target.url, Some(&credential.key))
                .await
                .map_err(|e| annotate_credential_failure(e, &credential))?;
            Ok((index, Some(credential)))
        }
        None => {
            let target = urls::public_index_url();
            if target.overridden {
                output.note("registry index override is active (testing only)");
            }
            let client = RegistryClient::new(timeout, target.overridden)?;
            let index = client.fetch_index(&target.url, None).await?;
            Ok((index, None))
        }
    }
}

fn annotate_credential_failure(error: CliError, credential: &Credential) -> CliError {
    // The error text already says what failed; add which credential was
    // selected so a stale store is findable. Never the key itself.
    CliError::precondition(format!(
        "{error} (credential {} from {})",
        credential.masked(),
        credential.source
    ))
    .with_fix(match error.remediation() {
        Some(fix) => fix.to_string(),
        None => "run `postvec login` with a current key, or `postvec logout`".to_string(),
    })
}

/// The engine's loopback admin address for a target, when there is one. A
/// `--path` target has no live engine; a remote target never reaches here.
pub fn engine_listen(target: &ModelTarget) -> Option<String> {
    match target {
        ModelTarget::Embedded { settings, .. } => Some(settings.embedded_http_listen()),
        _ => None,
    }
}

/// **Quiesce**: take every name that might hold new bytes out of the engine,
/// with the exact result set verified, before any of them is moved on disk.
///
/// A transport error is an *unknown partial application*, never "nothing
/// happened": the request may have been applied in
/// whole or in part with only the answer lost. So this refuses, and the
/// caller leaves the transaction recorded for the next command to settle.
pub async fn quiesce(
    engine: Option<&str>,
    names: &[String],
    timeout: std::time::Duration,
) -> Result<()> {
    let (Some(listen), false) = (engine, names.is_empty()) else {
        return Ok(());
    };
    let outcomes = admin::unload(listen, names, timeout).await.map_err(|e| {
        CliError::precondition(format!(
            "the embedded engine at {listen} could not be asked to unload {} ({e}); it may hold \
             any of them, so nothing was moved on disk",
            names.join(", ")
        ))
        .with_fix(
            "wait for the engine to finish starting and rerun; or stop the cluster and rerun \
             with --path <engine root> to work offline",
        )
    })?;
    let expected: Vec<(String, &'static [&'static str])> = names
        .iter()
        .map(|name| (name.clone(), admin::UNLOADED))
        .collect();
    admin::verify_outcomes(&expected, &outcomes).map_err(|problem| {
        CliError::precondition(format!(
            "unload did not complete cleanly: {problem}; nothing was moved on disk"
        ))
        .with_fix("resolve the engine-side error (server log) and rerun")
    })
}

/// **Restore and prove**: undo the whole batch — put every parked predecessor
/// back and remove every fresh install — then load the *complete* intended
/// predecessor set with the exact result set verified, and only then clear the
/// record.
///
/// The record is the last thing to go. If the reload cannot be proven the
/// transaction stays on disk, so the next command still sees unsettled state
/// instead of being allowed to proceed over an engine that is not where it
/// should be.
///
/// Note the two different name sets. Quiesce covers **every** batch member,
/// because the engine may have been made to hold any of them. The reload
/// covers only the **replacements**: a fresh install has no predecessor, and
/// asking the engine to load a name whose directory was just removed would
/// fail the very proof this function exists to establish.
///
/// The caller must have run [`quiesce`] over at least this transaction's
/// names first.
pub async fn restore_and_prove(
    root: &ModelRoot,
    engine: Option<&str>,
    transaction: &crate::registry::root::SwapTransaction,
    timeout: std::time::Duration,
) -> Result<()> {
    root.restore_swapped(transaction)?;
    root.remove_recorded_installs(transaction)?;
    let names = transaction.replaced_names();
    if let (Some(listen), false) = (engine, names.is_empty()) {
        let outcomes = admin::load(listen, &names, timeout).await.map_err(|e| {
            CliError::apply(format!(
                "the previous revision of {} was restored on disk but the engine could not be \
                 asked to load it: {e}",
                names.join(", ")
            ))
            .with_fix(
                "the replacement is still recorded; rerun any `postvec model` command once the \
                 engine is reachable to finish recovery",
            )
        })?;
        // After a verified unload the predecessor is not resident, so only a
        // genuine `loaded` proves it came back.
        let expected: Vec<(String, &'static [&'static str])> = names
            .iter()
            .map(|name| (name.clone(), admin::LOADED_FRESH))
            .collect();
        admin::verify_outcomes(&expected, &outcomes).map_err(|problem| {
            CliError::apply(format!(
                "the previous revision of {} was restored on disk but did not load: {problem}",
                names.join(", ")
            ))
            .with_fix(
                "check the engine log; the restored files are the previously working ones, and \
                 the replacement stays recorded until they load",
            )
        })?;
    }
    root.clear_swap(transaction)
}

/// Settle the replacement transaction an interrupted `model upgrade` may
/// have left behind, before this command changes anything else
/// Called by every command that takes the root's
/// exclusive lock.
///
/// A **confirmed** transaction only has superseded copies to drop. An
/// **unconfirmed** one means the replacement was never proven loadable, so
/// the protocol runs in full: quiesce every recorded name (the engine may
/// hold new bytes, or may hold nothing because the crash happened between the
/// unload and the rename), restore the filesystem, reload the complete
/// predecessor set, and clear the record last. If any of that cannot be
/// proven, the record and every recoverable copy are retained and the command
/// refuses — guessing here is what would lose the last good revision.
///
/// A `--path` target has no live engine to consult; the replacement's local
/// extraction and descriptor checks already passed, and the publisher's load
/// check is the upstream loadability gate, so recovery is the filesystem
/// rollback alone and runtime compatibility stays unknown until a serving
/// engine loads it.
pub async fn recover_pending_swap(
    root: &ModelRoot,
    target: &ModelTarget,
    timeout: std::time::Duration,
    output: &Output,
) -> Result<()> {
    let Some((transaction, phase)) = root.pending_swap()? else {
        return Ok(());
    };
    if phase == crate::registry::root::SwapPhase::Confirmed {
        root.discard_swap(&transaction);
        return Ok(());
    }
    let names = transaction.names();
    output.note(&format!(
        "recovering an interrupted model batch ({}): replacements are being rolled back to the \
         previous revision and fresh installs removed, because nothing proved the batch loadable",
        names.join(", ")
    ));
    let engine = engine_listen(target);
    quiesce(engine.as_deref(), &names, timeout).await?;
    restore_and_prove(root, engine.as_deref(), &transaction, timeout).await?;
    output.note(if engine.is_some() {
        "the previous revision was restored and reloaded"
    } else {
        "the previous revision was restored on disk"
    });
    Ok(())
}

/// A pull/rm on a cluster target also refreshes every configured database's
/// `postvec.models` cache so new models resolve without waiting out the
/// refresh interval. Failures are reported, not fatal — the worker refreshes
/// on its own cadence anyway.
/// Refresh `postvec.models` in every configured database. A missing
/// connection (offline listing fallback) is recorded as incomplete rather
/// than invented as success.
pub async fn refresh_databases(target: &mut ModelTarget, journal: &mut crate::plan::ApplyJournal) {
    let ModelTarget::Embedded {
        context, settings, ..
    } = target
    else {
        return;
    };
    let databases = settings.configured_databases();
    if databases.is_empty() {
        return;
    }
    let Some(context) = context.as_mut() else {
        journal.incomplete(
            "could not refresh postvec.models: no database connection \
             (rerun as root or as the cluster owner, or pass --database-url)"
                .to_string(),
        );
        return;
    };
    for database in databases {
        match context.db.refresh_models(&database).await {
            Ok(count) => {
                journal.record(format!("refreshed {database}: {count} models in cache"));
            }
            Err(e) => {
                journal.incomplete(format!(
                    "could not refresh postvec.models in {database}: {e} \
                     (the worker refreshes automatically on its next cycle)"
                ));
            }
        }
    }
}

/// Resolve a requested name against the inventory.
///
/// A name present under more than one backend is refused outright. The engine
/// resolves by name, so acting on "the name" while whichever directory an
/// iteration order found first is the one that moves would leave the other
/// copy to load at the next restart: a guess dressed up as success.
pub fn resolve_unambiguous<'a>(
    inventory: &'a [InstalledModel],
    name: &str,
    models_dir: &Path,
) -> Result<&'a InstalledModel> {
    let matches: Vec<&InstalledModel> = inventory.iter().filter(|m| m.dir_name == name).collect();
    match matches.len() {
        0 => Err(CliError::precondition(format!(
            "{name} is not installed under {}",
            models_dir.display()
        ))),
        1 => Ok(matches[0]),
        _ => Err(CliError::precondition(format!(
            "{name} exists under multiple backends: {}; the operation would be ambiguous",
            matches
                .iter()
                .map(|m| m.path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .with_fix(
            "resolve the duplicate first: remove the stray directory manually (it was \
             not created by this CLI — pulls refuse cross-backend collisions), then rerun",
        )),
    }
}

/// Ownership gate for every command that changes a model's state.
///
/// Package-owned content is refused unconditionally — a package upgrade
/// rewrites the descriptor to whatever it ships, so a persisted flip would be
/// a promise the CLI cannot keep. A directory with no receipt is refused
/// because there is nothing to keep `--verify` truthful against.
pub async fn require_cli_owned(
    model: &InstalledModel,
    verb: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    match model.ownership(timeout).await {
        Ownership::Cli => Ok(()),
        Ownership::Package => Err(CliError::precondition(format!(
            "{} is owned by a package; postvec model {verb} never changes package content",
            model.dir_name
        ))
        .with_fix(
            "a package upgrade would rewrite the descriptor, so the change could not be kept; \
             manage it with apt/dnf instead",
        )),
        Ownership::Manual => {
            let detail = model
                .receipt_error
                .clone()
                .unwrap_or_else(|| "no .postvec-install.json receipt".to_string());
            Err(CliError::precondition(format!(
                "{} was not installed by this CLI ({detail})",
                model.dir_name
            ))
            .with_fix(format!(
                "edit {}/ninference.hub.json by hand if that is really wanted",
                model.path.display()
            )))
        }
    }
}

/// Other **enabled** installed models that list `name` as a dependency.
///
/// A disabled dependant is not counted: it is not going to be loaded, so
/// nothing breaks. `exempt` holds the names this same command is already
/// acting on, so a batch does not report itself.
pub fn enabled_dependants(
    inventory: &[InstalledModel],
    name: &str,
    exempt: &[String],
) -> Vec<String> {
    inventory
        .iter()
        .filter(|other| other.dir_name != name && !exempt.contains(&other.dir_name))
        .filter(|other| other.enabled)
        .filter(|other| {
            other.dependencies.iter().any(|d| d == name)
                || other.receipt.as_ref().is_some_and(|r| {
                    r.dependencies.iter().any(|d| d == name)
                        || r.postvec_requires.iter().any(|d| d == name)
                })
        })
        .map(|other| other.dir_name.clone())
        .collect()
}

/// An explicit `postvec.embedded_models` list naming this model must change
/// before the model may be taken away.
///
/// The engine's startup preflight refuses a listed root whose descriptor is
/// disabled, and it refuses the whole initialization — so deactivating (or
/// removing) a listed model without editing the list first is a cluster that
/// will not bring its engine back on the next restart.
pub fn refuse_if_on_allow_list(allow_list: &[String], name: &str, verb: &str) -> Result<()> {
    if !allow_list.iter().any(|listed| listed == name) {
        return Ok(());
    }
    Err(CliError::precondition(format!(
        "{name} is named in postvec.embedded_models; {verb} it would make the next engine \
         start fail (an explicitly preloaded model that is disabled is a startup error, not a \
         skip)"
    ))
    .with_fix(format!(
        "first run: postvec setup --embedded --model {} …",
        allow_list
            .iter()
            .filter(|m| *m != name)
            .cloned()
            .collect::<Vec<_>>()
            .join(",")
    )))
}

/// The deactivated models `names` needs enabled before the engine will load
/// them, depth-first so a dependency is enabled before its dependant.
///
/// The engine refuses a load whose closure contains a deactivated model, and a
/// restart would not make one resident either — so activating a converter has
/// to bring its embed model back with it, or it would activate nothing usable.
pub fn disabled_closure_to_enable(inventory: &[InstalledModel], names: &[String]) -> Vec<String> {
    let by_name: std::collections::BTreeMap<&str, &InstalledModel> = inventory
        .iter()
        .map(|model| (model.dir_name.as_str(), model))
        .collect();
    let mut ordered: Vec<String> = Vec::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();

    fn visit(
        by_name: &std::collections::BTreeMap<&str, &InstalledModel>,
        name: &str,
        depth: usize,
        visited: &mut BTreeSet<String>,
        ordered: &mut Vec<String>,
    ) {
        // The engine bounds its own traversal at depth 32 and refuses cycles;
        // this only has to terminate.
        if depth > 32 || !visited.insert(name.to_string()) {
            return;
        }
        let Some(model) = by_name.get(name) else {
            return;
        };
        for dependency in &model.dependencies {
            visit(by_name, dependency, depth + 1, visited, ordered);
        }
        if !model.enabled {
            ordered.push(name.to_string());
        }
    }

    for name in names {
        visit(&by_name, name, 0, &mut visited, &mut ordered);
    }
    ordered
}

/// The name of the executor that lets a column on a convert-only target stay
/// writable and searchable. A product constant, not a configuration value:
/// `Descriptor::derived_postvec_requires` emits exactly this string.
const EMBED_BRIDGE: &str = "embed-bridge";

/// The vector space an embed model produces, as `postvec.models` records it
/// and `api/embed.rs` matches it: `COALESCE(target_model, name)`.
///
/// The two keys are genuinely different. `postvec.embedded_models` and an
/// engine load request name the **directory**; the resolver matches the
/// **declared** name, or the `target_model` alias when the descriptor sets
/// one. Published registry models make all three agree, so this only shows up
/// on an aliased or hand-built model — but reading it wrong there would let an
/// aliased embed silently look like no route at all.
fn embed_space_of(model: &InstalledModel) -> &str {
    model
        .target_model
        .as_deref()
        .or(model.descriptor_name.as_deref())
        .unwrap_or(&model.dir_name)
}

/// The models the engine would actually hold, given an explicit
/// `postvec.embedded_models` allow-list (empty means scan-load) and a set of
/// names treated as **already gone**.
///
/// Enabled-on-disk is necessary but not sufficient. With an explicit list the
/// engine loads only listed **roots** plus their dependency closure, so an
/// enabled-but-unlisted converter serves nothing — and counting it as a route
/// would silence the warning for the model that really is serving the column.
///
/// A root whose closure contains a deactivated or absent model is dropped
/// whole: `InferenceEngine::load_model` refuses a disabled config, so the load
/// fails and the root is not resident either.
///
/// `going_away` is why this takes a parameter instead of the caller filtering
/// the result. Subtracting names from an already-computed resident set answers
/// a different, weaker question: it drops the named models but keeps every
/// parent that can no longer load without them. Deactivating an engine
/// `dependencies` member that is *not* the converter's source and not
/// `embed-bridge` would then leave the parent looking resident, and the column
/// it serves would lose its route with no warning. Walking twice makes the two
/// sides symmetric and the answer exact.
fn resident_models<'a>(
    inventory: &'a [InstalledModel],
    allow_list: &[String],
    going_away: &[String],
) -> Vec<&'a InstalledModel> {
    let by_dir: std::collections::BTreeMap<&str, &InstalledModel> = inventory
        .iter()
        .map(|model| (model.dir_name.as_str(), model))
        .collect();

    /// Add `name` and its dependency closure to `closure`; `false` if any
    /// member is missing, deactivated or going away, in which case nothing is
    /// added — the engine's load of the root would fail on that member.
    fn visit(
        by_dir: &std::collections::BTreeMap<&str, &InstalledModel>,
        going_away: &[String],
        name: &str,
        depth: usize,
        seen: &mut BTreeSet<String>,
        closure: &mut Vec<String>,
    ) -> bool {
        if depth > 32 {
            return false;
        }
        if !seen.insert(name.to_string()) {
            return true; // already accounted for, or a cycle the engine refuses
        }
        let Some(model) = by_dir.get(name) else {
            return false;
        };
        if !model.enabled || going_away.iter().any(|gone| gone == name) {
            return false;
        }
        for dependency in &model.dependencies {
            if !visit(by_dir, going_away, dependency, depth + 1, seen, closure) {
                return false;
            }
        }
        closure.push(name.to_string());
        true
    }

    let roots: Vec<&InstalledModel> = if allow_list.is_empty() {
        inventory.iter().filter(|model| model.enabled).collect()
    } else {
        allow_list
            .iter()
            .filter_map(|name| by_dir.get(name.as_str()).copied())
            .collect()
    };

    let mut resident: BTreeSet<String> = BTreeSet::new();
    for root in roots {
        let mut closure = Vec::new();
        let mut seen = BTreeSet::new();
        if visit(
            &by_dir,
            going_away,
            &root.dir_name,
            0,
            &mut seen,
            &mut closure,
        ) {
            resident.extend(closure);
        }
    }
    inventory
        .iter()
        .filter(|model| resident.contains(&model.dir_name))
        .collect()
}

/// Can the resident inventory still serve a column declared on `space`? This
/// mirrors the extension's two-tier resolver (`api/embed.rs`,
/// postvec-description §3.5):
///
/// 1. a direct embed model whose space **is** `space` — `COALESCE(target_model,
///    name)`, not the directory name; or
/// 2. a converter **into** `space` whose own source space is itself
///    embeddable, routed through the `embed-bridge` executor.
///
/// Matching a column only by `registry.model == NAME` — the obvious reading —
/// is wrong in both directions. It misses the case this product exists for: a
/// column on `openai-text-embedding-ada-002` is served by a *converter* and
/// the bridge, neither of which is named `openai-text-embedding-ada-002`, so
/// deactivating the converter would have broken search with no warning at all.
/// And it over-warns when a second route into the same space survives.
fn space_is_servable(resident: &[&InstalledModel], space: &str) -> bool {
    let is = |model: &InstalledModel, kind: &str| model.model_type.as_deref() == Some(kind);
    let embeddable = |target: &str| {
        resident
            .iter()
            .any(|model| is(model, "embed") && embed_space_of(model) == target)
    };
    if embeddable(space) {
        return true;
    }
    let bridge_present = resident.iter().any(|model| model.dir_name == EMBED_BRIDGE);
    bridge_present
        && resident.iter().any(|model| {
            is(model, "convert")
                && model.target_model.as_deref() == Some(space)
                && model.source_model.as_deref().is_some_and(embeddable)
        })
}

/// Every managed column that would **lose its embedding route** if `names`
/// stopped serving, across every configured database — plus the databases
/// whose registry could not be read.
///
/// The question asked is not "does a column name this model" but "can this
/// column still be embedded afterwards", evaluated against what the engine
/// would actually hold, with `names` removed. A column whose route was already
/// missing before this command is not reported: this command is not what broke
/// it.
///
/// Reuses the ordinary database inspection rather than adding a second query:
/// it already tolerates an absent database, an unreachable one and a database
/// without the extension, and there is then one implementation of "what does
/// `postvec.registry` say".
///
/// A database that cannot be inspected is reported as **unknown**, never as
/// "not in use" — a silent "looked clean" is the exact failure this gate
/// exists to prevent.
///
/// Known limit: `registry.model` is the entry's *declared* model, so a column
/// part-way through a `migrate()` still reads as its pre-migration space until
/// finalization. That is the conservative direction — it warns about the model
/// the column is still being served by.
pub async fn in_use_columns(
    target: &mut ModelTarget,
    names: &[String],
    inventory: &[InstalledModel],
) -> (Vec<crate::plan::InUseColumn>, Vec<String>) {
    let ModelTarget::Embedded {
        context, settings, ..
    } = target
    else {
        return (Vec::new(), Vec::new());
    };
    let databases = settings.configured_databases();
    let allow_list = settings.embedded_models();
    let Some(context) = context.as_mut() else {
        return (Vec::new(), databases);
    };

    // What the engine holds now, and what it would hold with `names` gone —
    // two independent walks, not a subtraction. Both are computed the way the
    // engine decides residency, not merely from the `enabled` bit: an unlisted
    // model is not a route, and a parent that can no longer load its whole
    // closure is not one either.
    let before = resident_models(inventory, &allow_list, &[]);
    let after = resident_models(inventory, &allow_list, names);
    // Attribution needs "what if only this one went away", per name rather
    // than per column, so the walk runs once each instead of once per entry.
    let without_each: Vec<(&String, Vec<&InstalledModel>)> = names
        .iter()
        .map(|name| {
            (
                name,
                resident_models(inventory, &allow_list, std::slice::from_ref(name)),
            )
        })
        .collect();

    let mut columns = Vec::new();
    let mut unknown = Vec::new();
    for database in databases {
        let facts = match context.db.inspect_database(&database).await {
            Ok(facts) => facts,
            Err(_) => {
                unknown.push(database);
                continue;
            }
        };
        if facts.unreachable.is_some() {
            unknown.push(database);
            continue;
        }
        if !facts.exists || facts.postvec.is_none() {
            // A database that is configured but has no extension holds no
            // registry entries. That is knowledge, not a gap.
            continue;
        }
        for entry in &facts.registry {
            if space_is_servable(&after, &entry.model) || !space_is_servable(&before, &entry.model)
            {
                continue;
            }
            // Every name going away that this column's route depended on gets
            // the row, not just the first: when two equivalent routes are
            // removed together neither breaks the space *alone*, and blaming
            // one of them would leave the other's warning missing the column
            // it is actually taking down.
            let mut blamed: Vec<String> = without_each
                .iter()
                .filter(|(name, without)| {
                    // Either this name alone breaks the space, or it takes part
                    // in the route at all — the second test is what attributes
                    // the column to *every* removed leg when no single one is
                    // solely responsible.
                    !space_is_servable(without, &entry.model)
                        || provides_route(&before, name, &entry.model)
                })
                .map(|(name, _)| (*name).clone())
                .collect();
            if blamed.is_empty() {
                blamed = names.to_vec();
            }
            for name in blamed {
                columns.push(crate::plan::InUseColumn {
                    model: name,
                    declared_model: entry.model.clone(),
                    database: database.clone(),
                    relation: entry.relation.clone(),
                    column: entry.source_column.clone(),
                    state: entry.state.clone(),
                });
            }
        }
    }
    (columns, unknown)
}

/// Does `name` take part in any route into `space`? Used for attribution, not
/// for the decision: the decision is "is the space still servable", which
/// `space_is_servable` answers over the whole set.
fn provides_route(resident: &[&InstalledModel], name: &str, space: &str) -> bool {
    let Some(model) = resident.iter().find(|model| model.dir_name == name) else {
        return false;
    };
    let is = |model: &InstalledModel, kind: &str| model.model_type.as_deref() == Some(kind);
    if is(model, "embed") && embed_space_of(model) == space {
        return true;
    }
    if is(model, "convert") && model.target_model.as_deref() == Some(space) {
        return true;
    }
    // The bridge, and any embed model feeding a converter into this space, are
    // load-bearing for every bridged route.
    let feeds_a_converter = || {
        resident.iter().any(|converter| {
            is(converter, "convert")
                && converter.target_model.as_deref() == Some(space)
                && converter.source_model.as_deref() == Some(embed_space_of(model))
        })
    };
    let bridged_route_exists = || {
        resident.iter().any(|converter| {
            is(converter, "convert") && converter.target_model.as_deref() == Some(space)
        })
    };
    (model.dir_name == EMBED_BRIDGE && bridged_route_exists())
        || (is(model, "embed") && feeds_a_converter())
}

/// Attach one `AcknowledgeInUse` step per affected model, in `names` order.
///
/// A model with no columns *and* no unreadable database is simply absent from
/// the plan: nothing declares it, and there is nothing to acknowledge.
pub fn push_in_use_steps(
    plan: &mut crate::plan::Plan,
    names: &[String],
    columns: &[crate::plan::InUseColumn],
    unknown_databases: &[String],
) {
    for name in names {
        let mine: Vec<crate::plan::InUseColumn> = columns
            .iter()
            .filter(|column| &column.model == name)
            .cloned()
            .collect();
        if mine.is_empty() && unknown_databases.is_empty() {
            continue;
        }
        plan.push(crate::plan::PlanStep::AcknowledgeInUse {
            model: name.clone(),
            columns: mine,
            unknown_databases: unknown_databases.to_vec(),
        });
    }
}

/// Human-readable byte count, base-10 (kB/MB/GB).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Directory names an explicit `postvec.embedded_models` list would need.
/// Used by pull/activate to print the exact setup change.
pub fn setup_change_hint(settings: &SettingsSnapshot, missing: &[String]) -> String {
    let mut models: Vec<String> = settings.embedded_models();
    for name in missing {
        if !models.contains(name) {
            models.push(name.clone());
        }
    }
    format!(
        "postvec setup --embedded --model {} … (postvec.embedded_models is an explicit \
         allow-list; unlisted models are not loaded at startup)",
        models.join(",")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(
        name: &str,
        model_type: &str,
        source: Option<&str>,
        target: Option<&str>,
    ) -> InstalledModel {
        InstalledModel {
            dir_name: name.to_string(),
            descriptor_name: Some(name.to_string()),
            backend: "onnx-runtime".to_string(),
            enabled: true,
            dependencies: vec![],
            model_type: Some(model_type.to_string()),
            source_model: source.map(str::to_string),
            target_model: target.map(str::to_string),
            target_dim: Some(384),
            path: std::path::PathBuf::from(format!("/root/models/onnx-runtime/{name}")),
            receipt: None,
            receipt_error: None,
            disk_bytes: 1,
        }
    }

    /// The conversion story, which is the whole reason this product exists:
    /// a column declared on a legacy space is served by a **converter** plus
    /// the `embed-bridge` executor, and by nothing carrying that space's name.
    /// Matching a column by `registry.model == NAME` would see none of this.
    #[test]
    fn a_convert_only_space_is_servable_through_the_bridge_and_nothing_else() {
        let inventory = [
            model("baai-bge-m3", "embed", None, None),
            model(
                "convert-bge-to-ada",
                "convert",
                Some("baai-bge-m3"),
                Some("openai-text-embedding-ada-002"),
            ),
            model("embed-bridge", "embed-bridge", None, None),
        ];
        let all: Vec<&InstalledModel> = inventory.iter().collect();
        assert!(space_is_servable(&all, "openai-text-embedding-ada-002"));
        assert!(space_is_servable(&all, "baai-bge-m3"));

        // Each leg of the route is load-bearing, and removing any one of them
        // takes the legacy space with it.
        for gone in ["convert-bge-to-ada", "embed-bridge", "baai-bge-m3"] {
            let survivors: Vec<&InstalledModel> =
                inventory.iter().filter(|m| m.dir_name != gone).collect();
            assert!(
                !space_is_servable(&survivors, "openai-text-embedding-ada-002"),
                "removing {gone} must break the bridged space"
            );
        }

        // A converter whose own source space is not embeddable is not a route.
        let orphaned = [
            model(
                "convert-x-to-ada",
                "convert",
                Some("not-installed"),
                Some("openai-text-embedding-ada-002"),
            ),
            model("embed-bridge", "embed-bridge", None, None),
        ];
        assert!(!space_is_servable(
            &orphaned.iter().collect::<Vec<_>>(),
            "openai-text-embedding-ada-002"
        ));
    }

    /// The other direction the naive predicate gets wrong: a second route into
    /// the same space means nothing breaks, so warning would be a false alarm.
    #[test]
    fn a_surviving_second_route_means_the_space_is_still_servable() {
        let inventory = [
            model("baai-bge-m3", "embed", None, None),
            model("snowflake-arctic", "embed", None, None),
            model(
                "convert-bge-to-ada",
                "convert",
                Some("baai-bge-m3"),
                Some("openai-text-embedding-ada-002"),
            ),
            model(
                "convert-arctic-to-ada",
                "convert",
                Some("snowflake-arctic"),
                Some("openai-text-embedding-ada-002"),
            ),
            model("embed-bridge", "embed-bridge", None, None),
        ];
        let survivors: Vec<&InstalledModel> = inventory
            .iter()
            .filter(|m| m.dir_name != "convert-bge-to-ada")
            .collect();
        assert!(
            space_is_servable(&survivors, "openai-text-embedding-ada-002"),
            "the arctic converter still serves the space"
        );
    }

    /// Residency is not the `enabled` bit. With an explicit
    /// `postvec.embedded_models`, the engine loads listed roots plus their
    /// dependency closure — so an enabled-but-unlisted converter serves
    /// nothing, and counting it as a route would silence the warning for the
    /// converter that really is serving the column.
    #[test]
    fn an_unlisted_model_is_not_a_route_even_when_enabled_on_disk() {
        let mut serving = model(
            "convert-listed",
            "convert",
            Some("baai-bge-m3"),
            Some("openai-text-embedding-ada-002"),
        );
        serving.dependencies = vec!["baai-bge-m3".into(), EMBED_BRIDGE.into()];
        let inventory = [
            model("baai-bge-m3", "embed", None, None),
            serving,
            // Enabled on disk, but the operator never listed it.
            model(
                "convert-unlisted",
                "convert",
                Some("baai-bge-m3"),
                Some("openai-text-embedding-ada-002"),
            ),
            model(EMBED_BRIDGE, "embed-bridge", None, None),
        ];
        let space = "openai-text-embedding-ada-002";

        // Scan mode: both converters are routes, so removing one is harmless.
        let scan = resident_models(&inventory, &[], &[]);
        assert!(space_is_servable(&scan, space));
        let without_listed: Vec<&InstalledModel> = scan
            .iter()
            .copied()
            .filter(|m| m.dir_name != "convert-listed")
            .collect();
        assert!(
            space_is_servable(&without_listed, space),
            "scan mode really does have a second route"
        );

        // Explicit list: only the listed converter (and its closure) is
        // resident, so removing it takes the space down — and the unlisted
        // twin must not silence that.
        let listed = resident_models(&inventory, &["convert-listed".to_string()], &[]);
        assert!(space_is_servable(&listed, space));
        assert!(
            !listed.iter().any(|m| m.dir_name == "convert-unlisted"),
            "an unlisted model is not resident"
        );
        assert!(
            listed.iter().any(|m| m.dir_name == "baai-bge-m3"),
            "a listed root brings its dependency closure"
        );
        let after: Vec<&InstalledModel> = listed
            .iter()
            .copied()
            .filter(|m| m.dir_name != "convert-listed")
            .collect();
        assert!(
            !space_is_servable(&after, space),
            "the unlisted twin must not silence the warning"
        );
    }

    /// A listed root whose closure holds a deactivated model is not resident
    /// either: `InferenceEngine::load_model` refuses a disabled config, so the
    /// whole load fails.
    #[test]
    fn a_root_with_a_deactivated_dependency_is_not_resident() {
        let mut converter = model("converter", "convert", Some("dep"), Some("space-b"));
        converter.dependencies = vec!["dep".into()];
        let mut dep = model("dep", "embed", None, None);
        dep.enabled = false;
        let inventory = [converter, dep];

        for allow_list in [Vec::new(), vec!["converter".to_string()]] {
            let resident = resident_models(&inventory, &allow_list, &[]);
            assert!(
                resident.is_empty(),
                "an unloadable root must not count as resident: {:?}",
                resident.iter().map(|m| &m.dir_name).collect::<Vec<_>>()
            );
        }
    }

    /// The case a subtraction misses: deactivating an engine `dependencies`
    /// member that is neither the converter's source space nor `embed-bridge`.
    ///
    /// Dropping only the named model leaves the parent looking resident, so the
    /// column it serves keeps a route on paper — but `load_model` refuses the
    /// disabled dependency, the parent's executor build fails, and after a
    /// restart nothing serves that space. Walking residency a second time with
    /// the name treated as gone is what makes the warning fire.
    #[test]
    fn deactivating_a_plain_dependency_takes_its_parent_down_too() {
        let mut converter = model("converter", "convert", Some("embed-a"), Some("space"));
        // A tokenizer/companion the converter loads but that is not its source
        // space and not the bridge — the shape `postvec_requires` does not
        // derive, so only the engine's `dependencies` list knows about it.
        converter.dependencies = vec!["companion".into(), "embed-a".into(), EMBED_BRIDGE.into()];
        let inventory = [
            model("embed-a", "embed", None, None),
            converter,
            model("companion", "embed", None, Some("private-companion-space")),
            model(EMBED_BRIDGE, "embed-bridge", None, None),
        ];

        let before = resident_models(&inventory, &[], &[]);
        assert!(space_is_servable(&before, "space"));

        // The old shape: drop only the named model from the resident set.
        let subtracted: Vec<&InstalledModel> = before
            .iter()
            .copied()
            .filter(|m| m.dir_name != "companion")
            .collect();
        assert!(
            space_is_servable(&subtracted, "space"),
            "the subtraction really does miss it — this is the bug being pinned"
        );

        // A second walk drops the parent that can no longer load.
        let after = resident_models(&inventory, &[], &["companion".to_string()]);
        assert!(
            !after.iter().any(|m| m.dir_name == "converter"),
            "a parent whose dependency went away is not resident"
        );
        assert!(
            !space_is_servable(&after, "space"),
            "the column's route is gone, and the warning must fire"
        );
    }

    /// The resolver keys an embed model by `COALESCE(target_model, name)`, not
    /// by its directory. An aliased embed would otherwise read as no route at
    /// all — and the directory is what a load request and the allow-list use,
    /// so the two keys have to stay distinct.
    #[test]
    fn an_aliased_embed_serves_the_space_it_declares() {
        let mut aliased = model("vendor-dir-name", "embed", None, Some("public-space"));
        aliased.descriptor_name = Some("vendor-declared-name".into());
        let inventory = [aliased];
        let resident = resident_models(&inventory, &[], &[]);
        assert!(space_is_servable(&resident, "public-space"));
        assert!(!space_is_servable(&resident, "vendor-dir-name"));

        // With no alias the declared name is the space.
        let mut plain = model("dir", "embed", None, None);
        plain.descriptor_name = Some("declared".into());
        let plain = [plain];
        let resident = resident_models(&plain, &[], &[]);
        assert!(space_is_servable(&resident, "declared"));
    }

    /// Removing two equivalent routes at once: neither breaks the space
    /// *alone*, so a first-match blame would leave one model's warning missing
    /// the column it is really taking down. Both are attributed.
    #[test]
    fn removing_two_equivalent_routes_blames_both() {
        let inventory = [
            model("embed-a", "embed", None, None),
            model("embed-b", "embed", None, None),
            model("convert-a", "convert", Some("embed-a"), Some("space")),
            model("convert-b", "convert", Some("embed-b"), Some("space")),
            model(EMBED_BRIDGE, "embed-bridge", None, None),
        ];
        let before = resident_models(&inventory, &[], &[]);
        assert!(space_is_servable(&before, "space"));

        // Neither converter alone breaks it…
        for one in ["convert-a", "convert-b"] {
            let after: Vec<&InstalledModel> = before
                .iter()
                .copied()
                .filter(|m| m.dir_name != one)
                .collect();
            assert!(space_is_servable(&after, "space"));
            assert!(provides_route(&before, one, "space"));
        }
        // …but together they do, and both are named as providers.
        let after: Vec<&InstalledModel> = before
            .iter()
            .copied()
            .filter(|m| m.dir_name != "convert-a" && m.dir_name != "convert-b")
            .collect();
        assert!(!space_is_servable(&after, "space"));

        // The bridge and a converter's source embed are load-bearing too.
        assert!(provides_route(&before, EMBED_BRIDGE, "space"));
        assert!(provides_route(&before, "embed-a", "space"));
        // An unrelated model is not.
        assert!(!provides_route(&before, "embed-a", "other-space"));
    }

    /// A direct embed model is a route on its own — no bridge needed — and a
    /// column on a space nothing provides is unservable either way, so this
    /// command is not what broke it.
    #[test]
    fn a_direct_embed_model_needs_no_bridge_and_an_absent_space_is_never_servable() {
        let inventory = [model("baai-bge-m3", "embed", None, None)];
        let all: Vec<&InstalledModel> = inventory.iter().collect();
        assert!(space_is_servable(&all, "baai-bge-m3"));
        assert!(!space_is_servable(&all, "something-else"));
        assert!(!space_is_servable(&[], "baai-bge-m3"));
    }

    #[test]
    fn byte_rendering_matches_the_spec_shape() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(113_512_960), "113.5 MB");
        assert_eq!(human_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn a_readable_snippet_is_enough_to_resolve_an_embedded_root() {
        let settings = crate::config::owned::settings_from_snippet(
            "postvec.mode = 'embedded'\n\
             postvec.ninference_path = '/opt/postvec/ninference'\n\
             postvec.database = 'app'\n",
        );
        let target = target_from_settings(settings, None, Some("18/main".into())).unwrap();
        match target {
            ModelTarget::Embedded {
                root,
                context,
                cluster_id,
                ..
            } => {
                assert!(context.is_none());
                assert_eq!(cluster_id, "18/main");
                assert_eq!(
                    root.root,
                    std::path::PathBuf::from("/opt/postvec/ninference")
                );
            }
            ModelTarget::Path(_) | ModelTarget::Remote { .. } => {
                panic!("expected an embedded target")
            }
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::registry::root::{SwapModel, SwapPhase, SwapRole};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A programmable stand-in for the launcher's loopback admin listener.
    /// Each endpoint answers from a queue of scripted replies, so a test can
    /// stage a partial unload, a lost answer, or a refusal to load.
    struct FakeEngine {
        address: String,
        script: Arc<Mutex<Script>>,
        _thread: std::thread::JoinHandle<()>,
    }

    #[derive(Default)]
    struct Script {
        /// Replies for successive POSTs, in order. Running out means the
        /// connection is dropped, which the client sees as a transport error.
        unload: Vec<Reply>,
        load: Vec<Reply>,
        /// Every request, as (endpoint, models), for ordering assertions.
        seen: Vec<(String, Vec<String>)>,
    }

    #[derive(Clone)]
    enum Reply {
        /// `{model: status}` for each requested name, in request order.
        Statuses(Vec<&'static str>),
        /// Answer for a different name set than was asked for.
        Exactly(Vec<(&'static str, &'static str)>),
        /// Close the connection: a lost answer.
        Drop,
    }

    impl FakeEngine {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
            let script = Arc::new(Mutex::new(Script::default()));
            let thread = {
                let script = script.clone();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut stream) = stream else { break };
                        let mut buffer = [0u8; 8192];
                        let mut request = Vec::new();
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(n) => {
                                    request.extend_from_slice(&buffer[..n]);
                                    let text = String::from_utf8_lossy(&request);
                                    if let Some(headers) = text.split("\r\n\r\n").next() {
                                        let length: usize = headers
                                            .lines()
                                            .find(|l| {
                                                l.to_ascii_lowercase()
                                                    .starts_with("content-length:")
                                            })
                                            .and_then(|l| l.split(':').nth(1))
                                            .and_then(|v| v.trim().parse().ok())
                                            .unwrap_or(0);
                                        if text.len() >= headers.len() + 4 + length {
                                            break;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&request).into_owned();
                        let endpoint = if text.contains("/admin/unload") {
                            "unload"
                        } else {
                            "load"
                        };
                        let models: Vec<String> = text
                            .split("\r\n\r\n")
                            .nth(1)
                            .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
                            .and_then(|body| {
                                body.get("models").and_then(|m| m.as_array()).map(|list| {
                                    list.iter()
                                        .filter_map(|v| v.as_str().map(str::to_string))
                                        .collect()
                                })
                            })
                            .unwrap_or_default();

                        let reply = {
                            let mut script = script.lock().unwrap();
                            script.seen.push((endpoint.to_string(), models.clone()));
                            let queue = match endpoint {
                                "unload" => &mut script.unload,
                                _ => &mut script.load,
                            };
                            if queue.is_empty() {
                                Reply::Drop
                            } else {
                                queue.remove(0)
                            }
                        };
                        let results: Vec<serde_json::Value> = match reply {
                            Reply::Drop => {
                                drop(stream);
                                continue;
                            }
                            Reply::Statuses(statuses) => models
                                .iter()
                                .zip(statuses)
                                .map(|(model, status)| {
                                    serde_json::json!({"model": model, "status": status})
                                })
                                .collect(),
                            Reply::Exactly(pairs) => pairs
                                .iter()
                                .map(|(model, status)| {
                                    serde_json::json!({"model": model, "status": status})
                                })
                                .collect(),
                        };
                        let body = serde_json::json!({
                            "success": true, "data": { "results": results }
                        })
                        .to_string();
                        let _ = write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: \
                             application/json\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                    }
                })
            };
            FakeEngine {
                address,
                script,
                _thread: thread,
            }
        }

        fn on_unload(&self, replies: Vec<Reply>) {
            self.script.lock().unwrap().unload = replies;
        }
        fn on_load(&self, replies: Vec<Reply>) {
            self.script.lock().unwrap().load = replies;
        }
        fn seen(&self) -> Vec<(String, Vec<String>)> {
            self.script.lock().unwrap().seen.clone()
        }
    }

    fn root_fixture() -> (tempfile::TempDir, ModelRoot) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir_all(dir.path().join("models/onnx-runtime")).unwrap();
        fs::set_permissions(dir.path().join("models"), fs::Permissions::from_mode(0o755)).unwrap();
        let root = ModelRoot::new(dir.path().canonicalize().unwrap());
        (dir, root)
    }

    fn plant(root: &ModelRoot, name: &str, marker: &str) -> PathBuf {
        let dir = root.models_dir().join("onnx-runtime").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("marker"), marker).unwrap();
        dir
    }

    fn stage(root: &ModelRoot, name: &str, marker: &str) -> PathBuf {
        let staged = root.ensure_staging().unwrap().join(format!("{name}.stage"));
        fs::create_dir_all(&staged).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(staged.join("marker"), marker).unwrap();
        staged
    }

    fn swap_models(names: &[&str]) -> Vec<SwapModel> {
        names
            .iter()
            .map(|name| SwapModel {
                name: (*name).into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Replace,
                reload_on_rollback: true,
            })
            .collect()
    }

    fn marker(path: &Path) -> String {
        fs::read_to_string(path.join("marker")).unwrap()
    }

    fn quick() -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    /// A lost unload answer is an unknown partial application, never
    /// "nothing happened": it must refuse before anything moves.
    #[tokio::test]
    async fn a_lost_unload_answer_refuses() {
        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Drop]);
        let err = quiesce(Some(&engine.address), &["a".into()], quick())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("could not be asked to unload"),
            "{err}"
        );
        assert!(
            err.to_string().contains("nothing was moved on disk"),
            "{err}"
        );
    }

    /// A mixed result — some unloaded, one failing — fails the whole batch.
    #[tokio::test]
    async fn a_partial_unload_fails_the_batch() {
        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["unloaded", "error"])]);
        let err = quiesce(Some(&engine.address), &["a".into(), "b".into()], quick())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("did not complete cleanly"),
            "{err}"
        );

        // A result set that silently omits a name is equally unacceptable.
        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Exactly(vec![("a", "unloaded")])]);
        let err = quiesce(Some(&engine.address), &["a".into(), "b".into()], quick())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no result"), "{err}");
    }

    /// A crash after `begin_swap` but *before* the first rename leaves every
    /// predecessor on disk — and, if the unload had already happened, out of
    /// memory. Recovery must reload the complete recorded set, not just the
    /// names that happened to be parked.
    #[tokio::test]
    async fn recovery_reloads_every_recorded_name_even_when_none_were_parked() {
        let (_guard, root) = root_fixture();
        plant(&root, "a", "old-a");
        plant(&root, "b", "old-b");
        let txn = root.begin_swap(swap_models(&["a", "b"])).unwrap();
        drop(txn); // crash before the first rename

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["not-loaded", "not-loaded"])]);
        engine.on_load(vec![Reply::Statuses(vec!["loaded", "loaded"])]);

        let (recovered, phase) = root.pending_swap().unwrap().expect("recorded");
        assert_eq!(phase, SwapPhase::Swapping);
        quiesce(Some(&engine.address), &recovered.names(), quick())
            .await
            .unwrap();
        restore_and_prove(&root, Some(&engine.address), &recovered, quick())
            .await
            .unwrap();

        // Both names were unloaded and both reloaded, in that order.
        let seen = engine.seen();
        assert_eq!(seen[0].0, "unload");
        assert_eq!(seen[0].1, ["a", "b"]);
        assert_eq!(seen[1].0, "load");
        assert_eq!(seen[1].1, ["a", "b"]);
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// The record is the last thing to go: a predecessor that will not reload
    /// leaves a recoverable transaction behind rather than a clean-looking
    /// root over an engine in the wrong state.
    #[tokio::test]
    async fn a_failed_predecessor_reload_retains_the_transaction() {
        let (_guard, root) = root_fixture();
        let installed = plant(&root, "a", "old-a");
        let txn = root.begin_swap(swap_models(&["a"])).unwrap();
        root.swap_in(&txn, &stage(&root, "a", "new-a"), "onnx-runtime", "a")
            .unwrap();
        drop(txn);

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["unloaded"])]);
        engine.on_load(vec![Reply::Statuses(vec!["error"])]);

        let (recovered, _) = root.pending_swap().unwrap().expect("recorded");
        quiesce(Some(&engine.address), &recovered.names(), quick())
            .await
            .unwrap();
        let err = restore_and_prove(&root, Some(&engine.address), &recovered, quick())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not load"), "{err}");

        // Files are back, and the transaction survives for the next command.
        assert_eq!(marker(&installed), "old-a");
        assert!(root.pending_swap().unwrap().is_some());
    }

    /// Rollback order: everything that could hold new bytes is unloaded
    /// *before* the files it refers to are restored, so the engine can never
    /// be left serving bytes that rollback removed.
    #[tokio::test]
    async fn rollback_unloads_before_restoring() {
        let (_guard, root) = root_fixture();
        let installed = plant(&root, "a", "old-a");
        let txn = root.begin_swap(swap_models(&["a"])).unwrap();
        root.swap_in(&txn, &stage(&root, "a", "new-a"), "onnx-runtime", "a")
            .unwrap();
        assert_eq!(marker(&installed), "new-a");

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["unloaded"])]);
        engine.on_load(vec![Reply::Statuses(vec!["loaded"])]);

        quiesce(Some(&engine.address), &txn.names(), quick())
            .await
            .unwrap();
        // Only after the unload is proven does the filesystem move.
        assert_eq!(marker(&installed), "new-a");
        restore_and_prove(&root, Some(&engine.address), &txn, quick())
            .await
            .unwrap();
        assert_eq!(marker(&installed), "old-a");

        let seen = engine.seen();
        assert_eq!(seen[0].0, "unload");
        assert_eq!(seen[1].0, "load");
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// An unreachable engine keeps both copies and the record: refusing is
    /// the only safe answer, because the engine's state is unknown.
    #[tokio::test]
    async fn an_unreachable_engine_retains_everything() {
        let (_guard, root) = root_fixture();
        let installed = plant(&root, "a", "old-a");
        let txn = root.begin_swap(swap_models(&["a"])).unwrap();
        root.swap_in(&txn, &stage(&root, "a", "new-a"), "onnx-runtime", "a")
            .unwrap();
        drop(txn);

        // Nothing is listening here.
        let err = quiesce(Some("127.0.0.1:1"), &["a".into()], quick())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("nothing was moved on disk"),
            "{err}"
        );
        assert_eq!(marker(&installed), "new-a");
        assert!(root.pending_swap().unwrap().is_some());
    }

    /// A `--path` target has no engine to consult: recovery is the
    /// filesystem rollback alone, and it still clears the record.
    #[tokio::test]
    async fn offline_recovery_restores_without_an_engine() {
        let (_guard, root) = root_fixture();
        let installed = plant(&root, "a", "old-a");
        let txn = root.begin_swap(swap_models(&["a"])).unwrap();
        root.swap_in(&txn, &stage(&root, "a", "new-a"), "onnx-runtime", "a")
            .unwrap();
        drop(txn);

        let (recovered, _) = root.pending_swap().unwrap().expect("recorded");
        quiesce(None, &recovered.names(), quick()).await.unwrap();
        restore_and_prove(&root, None, &recovered, quick())
            .await
            .unwrap();
        assert_eq!(marker(&installed), "old-a");
        assert!(root.pending_swap().unwrap().is_none());
    }

    // ---------------------------------------------------------------------
    // Mixed batches: a replacement and a fresh dependency in one command.
    //
    // These are the composition cases the earlier protocol tests could not
    // reach. The replacement half could pass every one of them while fresh
    // installs sat outside the transaction entirely, which is exactly how a
    // crash could leave a fresh model installed and resident with recovery
    // unable to name it.
    // ---------------------------------------------------------------------

    fn mixed_batch() -> Vec<SwapModel> {
        vec![
            SwapModel {
                name: "replaced".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Replace,
                reload_on_rollback: true,
            },
            SwapModel {
                name: "fresh".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Install,
                reload_on_rollback: false,
            },
        ]
    }

    /// The blocker, end to end: a crash during the batched load leaves the
    /// replacement swapped in *and* the fresh dependency installed. Recovery
    /// must quiesce **both** names, restore the predecessor, **remove** the
    /// fresh install, and reload only the predecessor.
    #[tokio::test]
    async fn recovery_removes_a_fresh_install_and_restores_the_replacement() {
        let (_guard, root) = root_fixture();
        let replaced = plant(&root, "replaced", "old");
        let txn = root.begin_swap(mixed_batch()).unwrap();
        root.swap_in(
            &txn,
            &stage(&root, "replaced", "new"),
            "onnx-runtime",
            "replaced",
        )
        .unwrap();
        // The fresh dependency landed just before the crash.
        let fresh = root
            .install_staged(&stage(&root, "fresh", "fresh"), "onnx-runtime", "fresh")
            .unwrap();
        drop(txn);

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["unloaded", "unloaded"])]);
        engine.on_load(vec![Reply::Statuses(vec!["loaded"])]);

        let (recovered, phase) = root.pending_swap().unwrap().expect("recorded");
        assert_eq!(phase, SwapPhase::Swapping);
        quiesce(Some(&engine.address), &recovered.names(), quick())
            .await
            .unwrap();
        restore_and_prove(&root, Some(&engine.address), &recovered, quick())
            .await
            .unwrap();

        assert_eq!(marker(&replaced), "old", "predecessor not restored");
        assert!(!fresh.exists(), "the fresh install was left behind");
        assert!(root.pending_swap().unwrap().is_none(), "record not cleared");

        let seen = engine.seen();
        // Both names quiesced — the fresh one is resident-capable too.
        assert_eq!(seen[0].0, "unload");
        assert_eq!(seen[0].1, vec!["replaced".to_string(), "fresh".to_string()]);
        // Only the predecessor is reloaded: "fresh" no longer exists on disk,
        // so asking for it would fail the proof this step is here to make.
        assert_eq!(seen[1].0, "load");
        assert_eq!(seen[1].1, vec!["replaced".to_string()]);
    }

    /// The post-rename `fsync` case, expressed as the property that matters:
    /// the record — not in-process bookkeeping — is what makes a landed fresh
    /// install removable. `install_staged` can return `Err` from a `fsync`
    /// *after* the rename, so the caller may never learn the path; recovery
    /// must remove it anyway.
    #[tokio::test]
    async fn a_landed_fresh_install_is_removed_even_if_the_caller_never_saw_it() {
        let (_guard, root) = root_fixture();
        let txn = root
            .begin_swap(vec![SwapModel {
                name: "fresh".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Install,
                reload_on_rollback: false,
            }])
            .unwrap();
        // Land it exactly as a successful rename would, without telling the
        // caller — the state a post-rename fsync failure leaves behind.
        let fresh = root.models_dir().join("onnx-runtime").join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        fs::write(fresh.join("marker"), "fresh").unwrap();
        drop(txn);

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["not-loaded"])]);

        let (recovered, _) = root.pending_swap().unwrap().expect("recorded");
        quiesce(Some(&engine.address), &recovered.names(), quick())
            .await
            .unwrap();
        restore_and_prove(&root, Some(&engine.address), &recovered, quick())
            .await
            .unwrap();

        assert!(!fresh.exists(), "the orphaned install survived recovery");
        assert!(root.pending_swap().unwrap().is_none());
        // Nothing to reload: the batch had no predecessors.
        assert!(
            engine.seen().iter().all(|(endpoint, _)| endpoint != "load"),
            "recovery asked the engine to load something that does not exist"
        );
    }

    /// A crash *before* any fresh entry landed. Recovery must treat the
    /// absence as an ordinary no-op — the record describes intent, not proof
    /// of arrival — and still clear the record.
    #[tokio::test]
    async fn recovery_is_a_no_op_when_the_fresh_install_never_landed() {
        let (_guard, root) = root_fixture();
        let replaced = plant(&root, "replaced", "old");
        let txn = root.begin_swap(mixed_batch()).unwrap();
        drop(txn); // crash after the record, before any rename

        let engine = FakeEngine::start();
        engine.on_unload(vec![Reply::Statuses(vec!["not-loaded", "not-loaded"])]);
        engine.on_load(vec![Reply::Statuses(vec!["loaded"])]);

        let (recovered, _) = root.pending_swap().unwrap().expect("recorded");
        quiesce(Some(&engine.address), &recovered.names(), quick())
            .await
            .unwrap();
        restore_and_prove(&root, Some(&engine.address), &recovered, quick())
            .await
            .unwrap();

        assert_eq!(marker(&replaced), "old", "an untouched model was disturbed");
        assert!(!root
            .models_dir()
            .join("onnx-runtime")
            .join("fresh")
            .exists());
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// A fresh-only batch now opens a transaction too. Before this it opened
    /// none, so a crash left nothing to recover from and the install stayed.
    #[tokio::test]
    async fn a_fresh_only_batch_is_still_recorded_and_recoverable() {
        let (_guard, root) = root_fixture();
        let txn = root
            .begin_swap(vec![SwapModel {
                name: "fresh".into(),
                backend: "onnx-runtime".into(),
                role: SwapRole::Install,
                reload_on_rollback: false,
            }])
            .unwrap();
        let fresh = root
            .install_staged(&stage(&root, "fresh", "fresh"), "onnx-runtime", "fresh")
            .unwrap();
        assert!(fresh.exists());
        drop(txn);

        let (recovered, _) = root
            .pending_swap()
            .unwrap()
            .expect("a fresh-only batch must still be recorded");
        assert_eq!(recovered.replaced_names(), Vec::<String>::new());
        quiesce(None, &recovered.names(), quick()).await.unwrap();
        restore_and_prove(&root, None, &recovered, quick())
            .await
            .unwrap();
        assert!(!fresh.exists());
        assert!(root.pending_swap().unwrap().is_none());
    }

    /// A record from before roles existed is refused with the version
    /// message, not a serde error about a missing field.
    #[test]
    fn an_older_record_schema_is_refused_by_version() {
        let (_guard, root) = root_fixture();
        let dir = root.models_dir().join(".swap");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("txn.json"),
            r#"{"schema_version":1,"phase":"swapping","models":[{"name":"a","backend":"onnx-runtime"}]}"#,
        )
        .unwrap();
        let err = root.pending_swap().unwrap_err().to_string();
        assert!(err.contains("schema_version 1"), "{err}");
        assert!(!err.contains("missing field"), "{err}");
    }
}
