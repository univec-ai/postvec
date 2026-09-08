//! Fact collection shared by doctor and setup. One collector so both
//! judge the same observations.

use super::Context;
use crate::checks;
use crate::cli::{Mode, TlsPolicy};
use crate::cluster::OfflineSetting;
use crate::config::owned::Ownership;
use crate::config::MANAGED_SETTINGS;
use crate::engine;
use crate::error::Result;
use crate::facts::*;
use crate::validate;
use std::path::PathBuf;
use std::time::Duration;

/// Settings doctor reads beyond the ones the CLI manages.
const OBSERVED_SETTINGS: &[&str] = &[
    "max_worker_processes",
    "postvec.poll_interval_ms",
    "postvec.heartbeat_interval_ms",
    "postvec.worker_enabled",
    "postvec.model_refresh_interval_ms",
    "postvec.discovery_timeout_ms",
];

/// Longest a `--deep` heartbeat sample will wait. Beyond this the check is
/// skipped rather than blocking the command.
const DEEP_SAMPLE_CAP: Duration = Duration::from_secs(5);

/// Cluster-wide observations.
pub struct ClusterSnapshot {
    pub settings: SettingsSnapshot,
    /// `None` when there is no local installation to inspect.
    pub assets: Option<AssetFacts>,
    pub ownership: Option<Ownership>,
    pub offline_preload: Option<OfflineSetting>,
}

pub async fn cluster_snapshot(context: &mut Context) -> Result<ClusterSnapshot> {
    let names: Vec<&str> = MANAGED_SETTINGS
        .iter()
        .chain(OBSERVED_SETTINGS.iter())
        .copied()
        .collect();
    let settings = context.db.settings(&names).await?;
    let assets = context.cluster.assets();
    // Ownership is only meaningful where a configuration directory is known,
    // and an unreadable state file must not fail a read-only command.
    let ownership = match context.cluster.owned_paths() {
        Ok(paths) => paths.inspect().ok(),
        Err(_) => None,
    };
    let offline_preload = context
        .cluster
        .query_setting_offline("shared_preload_libraries", context.timeout)
        .await
        .ok();
    Ok(ClusterSnapshot {
        settings,
        assets,
        ownership,
        offline_preload,
    })
}

/// Inspect each named database, optionally sampling the heartbeat twice.
///
/// A database that cannot be inspected becomes a *fact* about that database,
/// not an error for the whole run: a catalog inconsistency or a permission
/// problem in one database must not take away the report for every other
/// database and for the endpoints.
pub async fn database_facts(
    context: &mut Context,
    names: &[String],
    deep: bool,
    poll: Duration,
    fresh: Duration,
) -> Result<Vec<DatabaseFacts>> {
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let inspected = {
            let mut db = context.read_only();
            db.inspect_database(name).await
        };
        let mut facts = match inspected {
            Ok(facts) => facts,
            Err(error) => {
                let mut facts = DatabaseFacts::absent(name);
                // It was named, so it is expected to exist; the check reports
                // the reason rather than claiming the database is gone.
                facts.exists = true;
                facts.unreachable = Some(error.to_string());
                out.push(facts);
                continue;
            }
        };
        if deep {
            // A failed second sample only costs the advancement check, which
            // then reports that it could not be taken.
            sample_advancement(context, name, &mut facts, poll, fresh).await;
        }
        out.push(facts);
    }
    Ok(out)
}

/// Take a second heartbeat reading and decide whether it advanced.
///
/// Read-only: two `SELECT`s and a sleep. When the poll interval is longer than
/// the sampling cap, the check is skipped instead of waiting — an operator
/// running `doctor` should not be held for a minute.
async fn sample_advancement(
    context: &mut Context,
    database: &str,
    facts: &mut DatabaseFacts,
    poll: Duration,
    fresh: Duration,
) {
    let Some(worker) = facts.worker.as_mut() else {
        return;
    };
    if worker.pid.is_none() {
        return;
    }
    let first = worker.last_beat_age_s;
    // Heartbeats are change-gated: an idle worker writes once per
    // postvec.heartbeat_interval_ms, so a beat within the freshness budget is
    // already proof of life — a short double-sample would only observe the
    // deliberate quiet and false-alarm. The double sample below is reserved
    // for a beat that is already suspiciously old.
    if let Some(first) = first {
        if first <= fresh.as_secs_f64() {
            worker.advanced = Some(true);
            return;
        }
    }
    let wait = poll + Duration::from_millis(250);
    if wait > DEEP_SAMPLE_CAP {
        worker.advanced = None;
        return;
    }
    tokio::time::sleep(wait).await;
    let sample = {
        let mut db = context.read_only();
        match db.heartbeat_sample(database).await {
            Ok(sample) => sample,
            // A transient disconnect between samples costs this one check, not
            // the report.
            Err(_) => {
                facts
                    .worker
                    .as_mut()
                    .expect("worker was present above")
                    .advanced = None;
                return;
            }
        }
    };
    let worker = facts.worker.as_mut().expect("worker was present above");
    worker.second_beat_age_s = sample.age_s;
    // The age of the *same* beat grows with the wait; an advancing worker
    // produces a younger reading than the wait would imply.
    worker.advanced = match (first, sample.age_s) {
        (Some(first), Some(second)) => Some(second < first + wait.as_secs_f64() * 0.5),
        _ => None,
    };
}

/// What was found on the inference side.
///
/// The variants differ a lot in size; boxing them would buy nothing here, since
/// exactly one of these exists per command invocation.
#[allow(clippy::large_enum_variant)]
pub enum InferenceProbe {
    Remote(RemoteProbe),
    Embedded(EmbeddedProbe),
    /// The probe itself could not be carried out. Kept as a fact rather than
    /// discarded: silently reporting "no inference findings" would let a broken
    /// probe produce a clean bill of health.
    Unavailable {
        error: String,
    },
    /// Nothing to probe: the mode could not be determined.
    None,
}

impl InferenceProbe {
    /// Whether a usable discovery path exists right now. `None` when it was not
    /// determined.
    pub fn reachable(&self) -> Option<bool> {
        match self {
            InferenceProbe::Remote(probe) => Some(
                probe.http.iter().any(|node| node.config.is_some())
                    && probe.grpc.iter().any(|node| node.connected),
            ),
            InferenceProbe::Embedded(probe) => Some(
                probe.loaded.is_some()
                    && probe
                        .grpc_listener
                        .as_ref()
                        .is_some_and(|listener| listener.connected),
            ),
            InferenceProbe::Unavailable { .. } | InferenceProbe::None => None,
        }
    }
}

/// Probe whichever inference side the cluster is configured for.
pub async fn inference_probe(
    context: &Context,
    settings: &SettingsSnapshot,
    tls: TlsPolicy,
    path_override: Option<PathBuf>,
) -> Result<InferenceProbe> {
    match settings.mode() {
        Some(Mode::Grpc) => Ok(InferenceProbe::Remote(
            probe_remote(settings, tls, context.timeout).await?,
        )),
        Some(Mode::Embedded) => Ok(InferenceProbe::Embedded(
            probe_embedded(context, settings, path_override).await?,
        )),
        // An unparseable mode is reported by cluster.mode; probing on a guess
        // would produce misleading endpoint findings.
        None => Ok(InferenceProbe::None),
    }
}

/// Probe the configured remote endpoints, recording ones that do not even parse.
pub async fn probe_remote(
    settings: &SettingsSnapshot,
    tls: TlsPolicy,
    timeout: Duration,
) -> Result<RemoteProbe> {
    let mut malformed = Vec::new();
    let mut grpc = Vec::new();
    for raw in settings.grpc_endpoints() {
        match validate::grpc_endpoint(&raw) {
            Ok(endpoint) => grpc.push(endpoint),
            Err(error) => malformed.push(MalformedEndpoint {
                value: raw,
                error: error.to_string(),
            }),
        }
    }
    let mut http = Vec::new();
    for raw in settings.http_endpoints() {
        match validate::http_endpoint(&raw) {
            Ok(endpoint) => http.push(endpoint),
            Err(error) => malformed.push(MalformedEndpoint {
                value: raw,
                error: error.to_string(),
            }),
        }
    }
    let mut probe = engine::remote::probe(&grpc, &http, tls, timeout).await?;
    probe.malformed = malformed;
    Ok(probe)
}

/// Inspect the engine root and the launcher's loopback listeners.
pub async fn probe_embedded(
    context: &Context,
    settings: &SettingsSnapshot,
    path_override: Option<PathBuf>,
) -> Result<EmbeddedProbe> {
    let (root, source) = match (settings.engine_path(), path_override) {
        (Some(path), _) => (Some(path), RootSource::Guc),
        // A snippet-only view may not carry postvec.path; the override exists
        // to diagnose a root the CLI cannot otherwise see.
        (None, Some(path)) => (Some(path), RootSource::Override),
        (None, None) => (None, RootSource::Unknown),
    };
    let mut probe = engine::embedded::inspect_root(root, source, context.cluster.owner.as_ref());
    if let Some(root) = probe.root.clone() {
        probe.readable_by_server =
            engine::embedded::readable_by(&root, context.cluster.owner.as_ref(), context.timeout)
                .await;
    }
    engine::embedded::probe_listeners(
        &mut probe,
        &settings.embedded_listen(),
        &settings.embedded_http_listen(),
        context.timeout,
    )
    .await?;
    Ok(probe)
}

/// Installed inventory plus receipt state. Under `--deep`, per-file hashes.
pub fn model_store_facts(probe: &EmbeddedProbe, deep: bool) -> Option<ModelStoreFacts> {
    let root = probe.root.clone()?;
    let store = crate::registry::root::ModelRoot::new(root.clone());
    let models = store.installed().ok()?;
    Some(ModelStoreFacts {
        root,
        models: models
            .into_iter()
            .map(|model| {
                let integrity_problems = match (&model.receipt, deep) {
                    (Some(receipt), true) => Some(receipt.verify_files(&model.path)),
                    _ => None,
                };
                crate::facts::StoredModelFacts {
                    receipt_name: model.receipt.as_ref().map(|r| r.name.clone()),
                    receipt_revision: model.receipt.as_ref().map(|r| r.revision()),
                    receipt_backend: model.receipt.as_ref().map(|r| r.backend.clone()),
                    cli_owned: model.receipt.is_some(),
                    dir_name: model.dir_name,
                    descriptor_name: model.descriptor_name,
                    backend: model.backend,
                    enabled: model.enabled,
                    receipt_error: model.receipt_error,
                    integrity_problems,
                }
            })
            .collect(),
    })
}

/// Probe the selected registry channel — `--deep` only, and read-only: it
/// fetches and validates the index, nothing else. A resolved-but-broken
/// credential is a finding, not an abort.
pub async fn registry_probe(deep: bool, timeout: Duration) -> RegistryProbeFacts {
    if !deep {
        return RegistryProbeFacts {
            attempted: false,
            channel: None,
            model_count: None,
            credential_source: None,
            error: None,
            head_revisions: Default::default(),
        };
    }
    let credential = match crate::registry::auth::resolve(None) {
        Ok(credential) => credential,
        Err(e) => {
            return RegistryProbeFacts {
                attempted: true,
                channel: None,
                model_count: None,
                credential_source: None,
                error: Some(e.to_string()),
                head_revisions: Default::default(),
            }
        }
    };
    let source = credential.as_ref().map(|c| c.source.to_string());
    let (target, bearer) = match &credential {
        Some(credential) => (
            crate::registry::urls::authenticated_index_url(),
            Some(credential.key.clone()),
        ),
        None => (crate::registry::urls::public_index_url(), None),
    };
    let outcome = async {
        let client = crate::registry::client::RegistryClient::new(timeout, target.overridden)?;
        client.fetch_index(&target.url, bearer.as_deref()).await
    }
    .await;
    match outcome {
        Ok(index) => RegistryProbeFacts {
            attempted: true,
            channel: Some(index.channel.clone()),
            model_count: Some(index.models.len()),
            credential_source: source,
            error: None,
            head_revisions: index
                .models
                .iter()
                .filter(|m| !m.withdrawn)
                .map(|m| (m.name.clone(), m.revision()))
                .collect(),
        },
        Err(e) => RegistryProbeFacts {
            attempted: true,
            channel: None,
            model_count: None,
            credential_source: source,
            error: Some(e.to_string()),
            head_revisions: Default::default(),
        },
    }
}

/// Build the check set from collected facts. Doctor and setup smoke use
/// this unchanged.
pub fn evaluate(
    context: &Context,
    snapshot: &ClusterSnapshot,
    databases: &[DatabaseFacts],
    inference: &InferenceProbe,
    identity: &crate::commands::IdentityProof,
    registry: Option<&RegistryProbeFacts>,
    deep: bool,
    tls_strict: bool,
) -> Vec<checks::CheckResult> {
    let inspected: Vec<String> = databases.iter().map(|facts| facts.name.clone()).collect();
    let mut out = checks::cluster::checks(&checks::cluster::ClusterInput {
        identity: &context.cluster.identity,
        pg_config: &context.cluster.pg_config,
        binary_major: context.cluster.binary_major,
        server: &context.server,
        settings: &snapshot.settings,
        assets: snapshot.assets.as_ref(),
        ownership: snapshot.ownership.as_ref(),
        offline_preload: snapshot.offline_preload.as_ref(),
        inspected_databases: &inspected,
        other_worker_slots: 0,
        instance_identity: identity.clone(),
    });

    let reachable = inference.reachable();
    let mode = snapshot.settings.mode();
    for facts in databases {
        out.extend(checks::database::checks(&checks::database::DatabaseInput {
            facts,
            settings: &snapshot.settings,
            mode,
            inference_reachable: reachable,
            deep,
        }));
    }

    // Per database, never unioned: see DatabaseCache.
    let cached = DatabaseCache::from_facts(databases);

    match inference {
        InferenceProbe::Remote(probe) => {
            out.extend(checks::remote::checks(&checks::remote::RemoteInput {
                probe,
                cached_models: &cached,
                tls_strict,
            }));
            // Provider files live on the server nodes in remote mode, so
            // there is nothing here to inspect — say where they are rather
            // than reporting a clean bill for something never looked at.
            out.push(checks::provider::grpc_note());
        }
        InferenceProbe::Embedded(probe) => {
            let build_info = databases
                .iter()
                .filter_map(|facts| facts.postvec.as_ref())
                .filter_map(|extension| extension.build_info.as_ref())
                .next();
            let requested = snapshot.settings.embedded_models();
            let versioned = probe
                .root
                .as_ref()
                .map(|root| engine::embedded::find_versioned_ort_libraries(&root.join("libs")))
                .unwrap_or_default();
            out.extend(checks::embedded::checks(&checks::embedded::EmbeddedInput {
                probe,
                build_info,
                requested_models: &requested,
                cached_models: &cached,
                uptime: context.uptime(),
                refresh_interval: Duration::from_millis(
                    snapshot
                        .settings
                        .int("postvec.model_refresh_interval_ms", 60_000)
                        as u64,
                ),
                log_command: context.log_command(),
                versioned_ort_libraries: versioned,
            }));
            // External providers: the providers.d files this host's engine
            // reads. Read-only, and independent of the model store — a
            // provider-backed model has no on-disk descriptor by design.
            let provider_facts = checks::provider::gather(&snapshot.settings.providers_path());
            let loaded_names = probe.loaded.as_ref().map(|inv| inv.enabled_names());
            out.extend(checks::provider::checks(&checks::provider::ProviderInput {
                facts: &provider_facts,
                served: loaded_names.as_ref(),
            }));
            // The CLI-managed model store (postvec model …): receipts,
            // activation, and — under --deep — file integrity and registry
            // reachability. Same filesystem-inspection latitude as the ORT
            // scan above.
            let store = model_store_facts(probe, deep);
            out.extend(checks::models::checks(&checks::models::ModelsInput {
                store: store.as_ref(),
                registry,
                loaded: loaded_names.as_ref(),
                allow_list: &requested,
                deep,
            }));
        }
        InferenceProbe::Unavailable { error } => {
            out.push(
                checks::CheckResult::fail(
                    "inference.probe",
                    "inference",
                    format!("the inference side could not be probed: {error}"),
                )
                .required()
                .with_fix(
                    "nothing is known about the engine or endpoints; without this the report \
                     cannot say whether inference works",
                ),
            );
        }
        InferenceProbe::None => {
            // A mode that does not parse is already reported by cluster.mode;
            // there is nothing to probe and nothing to hide.
        }
    }

    checks::sort_checks(&mut out);
    out
}

/// The worker's configured poll cadence, which the heartbeat checks and the
/// `--deep` sampling both key off.
pub fn poll_interval(settings: &SettingsSnapshot) -> Duration {
    Duration::from_millis(settings.int("postvec.poll_interval_ms", 5000).max(10) as u64)
}

/// The databases to inspect when none were named: everything the launcher is
/// configured to serve.
pub fn default_databases(settings: &SettingsSnapshot) -> Vec<String> {
    let mut databases = settings.configured_databases();
    databases.sort();
    databases.dedup();
    databases
}

/// Report metadata shared by doctor and setup.
pub fn report_cluster(context: &Context, settings: &SettingsSnapshot) -> checks::ReportCluster {
    checks::ReportCluster {
        id: context.cluster.identity.id.clone(),
        postgres_major: Some(context.server.major()),
        postgres_version: Some(context.server.short_version().to_string()),
        mode: settings
            .mode()
            .map(|mode| mode.as_guc().to_string())
            .or_else(|| settings.raw_mode().map(|raw| format!("{raw} (invalid)"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ConfigInventory, GrpcProbe, HttpProbe, InventoryModel, ListenerProbe};

    fn inventory(names: &[&str]) -> ConfigInventory {
        ConfigInventory {
            models: names
                .iter()
                .map(|name| InventoryModel {
                    name: (*name).to_string(),
                    enabled: true,
                    model_type: Some("embed".into()),
                    provider: None,
                    provider_file: None,
                    provider_endpoint: None,
                    provider_model_id: None,
                    target_dim: None,
                    space: None,
                    priority: None,
                })
                .collect(),
        }
    }

    #[test]
    fn remote_reachability_requires_both_transport_and_discovery() {
        let grpc_up = GrpcProbe {
            endpoint: validate::grpc_endpoint("a:1").unwrap(),
            resolved: vec!["1.2.3.4:1".into()],
            resolve_error: None,
            connected: true,
            connect_error: None,
            duration_ms: 1,
        };
        let http_up = HttpProbe {
            endpoint: validate::http_endpoint("https://a").unwrap(),
            health: HealthOutcome::Ok { status: 200 },
            config_status: Some(200),
            config: Some(inventory(&["m"])),
            error: None,
            tls_verified: Some(true),
            duration_ms: 1,
        };

        let both = InferenceProbe::Remote(RemoteProbe {
            grpc: vec![grpc_up.clone()],
            http: vec![http_up.clone()],
            malformed: vec![],
        });
        assert_eq!(both.reachable(), Some(true));

        // Discovery works but no gRPC: nothing can actually be embedded.
        let mut no_grpc = grpc_up.clone();
        no_grpc.connected = false;
        assert_eq!(
            InferenceProbe::Remote(RemoteProbe {
                grpc: vec![no_grpc],
                http: vec![http_up.clone()],
                malformed: vec![],
            })
            .reachable(),
            Some(false)
        );

        // gRPC works but no discovery: no model can be resolved.
        let mut no_config = http_up;
        no_config.config = None;
        assert_eq!(
            InferenceProbe::Remote(RemoteProbe {
                grpc: vec![grpc_up],
                http: vec![no_config],
                malformed: vec![],
            })
            .reachable(),
            Some(false)
        );
    }

    #[test]
    fn embedded_reachability_requires_a_live_listener_and_an_inventory() {
        let mut probe = EmbeddedProbe::new(Some(PathBuf::from("/opt/nin")), RootSource::Guc);
        probe.grpc_listener = Some(ListenerProbe {
            address: "127.0.0.1:33433".into(),
            connected: true,
            error: None,
        });
        probe.loaded = Some(inventory(&["m"]));
        let up = InferenceProbe::Embedded(probe.clone());
        assert_eq!(up.reachable(), Some(true));

        probe.loaded = None;
        assert_eq!(InferenceProbe::Embedded(probe).reachable(), Some(false));
    }

    #[test]
    fn an_undetermined_mode_probes_nothing() {
        assert_eq!(InferenceProbe::None.reachable(), None);
    }

    /// A probe that could not run must not read as "no problems found".
    #[test]
    fn a_failed_probe_is_reported_as_a_blocking_check() {
        assert_eq!(
            InferenceProbe::Unavailable {
                error: "cannot build HTTP client".into()
            }
            .reachable(),
            None
        );
    }

    #[test]
    fn default_databases_come_from_the_launcher_configuration() {
        let settings = SettingsSnapshot {
            rows: vec![SettingRow {
                name: "postvec.database".into(),
                setting: "univec, analytics ,univec".into(),
                context: "postmaster".into(),
                source: "configuration file".into(),
                sourcefile: None,
                sourceline: None,
                pending_restart: false,
            }],
            file_rows: vec![],
        };
        assert_eq!(default_databases(&settings), ["analytics", "univec"]);
    }
}
