// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Public HTTP listener: discovery, health, metrics, native `/api/{model}`,
//! the OpenAI embeddings adaptor, and the optional SPA.
//!
//! Admin mutation routes live on the loopback listener in [`crate::admin`].
//!
//! `GET /config` `data.models` is the compatibility surface: an array of
//! `{name, status, configuration}`. Additions are safe. Renaming or nesting
//! `models` is not. The list is what is loaded now, not what is on disk.

use crate::api;
use crate::cluster::ClusterMember;
use crate::metrics::Snapshot;
use crate::models;
use crate::state::ServerState;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use engine::{InferenceEngine, ModelConfiguration};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};

/// How long a graceful shutdown waits for in-flight HTTP work.
pub const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(10);

/// The `models` array, byte-compatible with what `discovery::parse_config`
/// expects. Pure, so the wire shape is testable without an engine.
pub fn config_models(configs: &[ModelConfiguration]) -> Vec<Value> {
    configs
        .iter()
        .map(|cfg| {
            json!({
                "name": cfg.name,
                "status": "local",
                "configuration": serde_json::to_value(cfg).unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// [`config_models`] plus the provider gateway's descriptors (already in
/// the nested HubModel shape), appended after the engine's own models. A
/// public name that collides with a local model is skipped with a warning:
/// the local model wins, same rule as the gRPC dispatch.
pub fn merged_config_models(
    configs: &[ModelConfiguration],
    gateway: &providers::gateway::Gateway,
) -> Vec<Value> {
    let mut models = config_models(configs);
    for descriptor in gateway.models() {
        let name = descriptor["name"].as_str().unwrap_or_default();
        if configs.iter().any(|cfg| cfg.name == name) {
            // Debug, not warn: `/config` is polled on every discovery
            // refresh (once a minute per database, per node), and a
            // deliberate collision is a *steady state*, not an event. At
            // warn this printed the same line forever. `provider ls` and
            // `postvec doctor` report the collision where it is actionable.
            log::debug!(
                "provider model {name:?} collides with a local engine model; \
                 the local model wins and the provider entry is not served"
            );
            continue;
        }
        models.push(descriptor);
    }
    models
}

fn loaded_configs(engine: &InferenceEngine) -> Vec<ModelConfiguration> {
    engine
        .get_active_models()
        .into_iter()
        .filter_map(|name| engine.get_model(&name).ok())
        .map(|model| model.configuration().clone())
        .collect()
}

/// Memory, from `/proc/meminfo` where it exists.
///
/// Memory is the binding constraint on an inference node — every resident
/// model is a set of native sessions — so a discovery read that already
/// costs a round trip may as well carry it. Zeroes elsewhere, matching what
/// the upstream engine reports when it cannot measure.
fn system_object() -> Value {
    let (total, available) = read_meminfo().unwrap_or((0, 0));
    json!({
        "memory_total_bytes": total,
        "memory_available_bytes": available,
        "memory_used_bytes": total.saturating_sub(available),
    })
}

fn read_meminfo() -> Option<(u64, u64)> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        raw.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024))
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

fn server_object(state: &ServerState) -> Value {
    json!({
        "name": "postvec-server",
        "version": env!("CARGO_PKG_VERSION"),
        "features": crate::metrics::features(),
        "uptime_seconds": state.uptime_seconds(),
        "draining": state.draining(),
        "manage": state.settings.manage,
        "predict_timeout_ms": state.settings.predict_timeout.as_millis() as u64,
        // The engine root, so `postvec-server status` can read the on-disk
        // inventory without being told which tree this node was started with.
        "root": state.settings.root.to_string_lossy(),
        "frontend": state.identity.frontend,
        // The `--models` allow-list, empty when there is none. Without it a
        // client comparing disk against loaded reports every deliberately
        // excluded model as a missing one.
        "models_allowed": state.settings.models,
        "managed": state.managed.snapshot(),
    })
}

/// This node, as a cluster member. Used when gossip is down so the UI still
/// has a peer to send `/api/{model}` at (this process).
fn current_member(state: &ServerState) -> ClusterMember {
    ClusterMember {
        address: state.identity.api_address.clone(),
        grpc: state.identity.grpc_address.clone(),
        group: state.settings.group.clone(),
        status: "Alive".to_string(),
        frontend_address: Some(state.identity.frontend.clone()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        current: true,
    }
}

async fn cluster_object(state: &ServerState) -> Value {
    let mut nodes = match &state.cluster {
        Some(cluster) => cluster.members().await,
        None => Vec::new(),
    };
    // A node that could not start gossip, or that has not yet seen itself in
    // the memberlist, still serves. The dashboard queries `cluster.nodes`;
    // an empty list would look like "no live nodes" on a working process.
    if !nodes.iter().any(|n| n.current) {
        nodes.insert(0, current_member(state));
    }
    json!({
        "group": state.settings.group,
        "gossip_port": state.settings.gossip_port,
        "advertise": state.identity.advertise.to_string(),
        "frontend": state.identity.frontend,
        "enabled": state.cluster.is_some(),
        "nodes": nodes,
    })
}

/// The full discovery envelope.
pub async fn config_envelope(state: &ServerState) -> Value {
    let mut data = Map::new();
    data.insert(
        "models".to_string(),
        Value::Array(merged_config_models(
            &loaded_configs(&state.engine),
            &state.gateway,
        )),
    );
    data.insert("server".to_string(), server_object(state));
    data.insert("cluster".to_string(), cluster_object(state).await);
    data.insert("system".to_string(), system_object());
    json!({ "success": true, "data": Value::Object(data) })
}

async fn handle_config(State(state): State<Arc<ServerState>>) -> Json<Value> {
    state.metrics.config_served();
    Json(config_envelope(&state).await)
}

/// Liveness. Stays 200 through a drain: the process is alive and finishing
/// work, and a supervisor that restarts it now would kill requests that were
/// about to succeed.
async fn handle_health(State(state): State<Arc<ServerState>>) -> Json<Value> {
    Json(json!({
        "success": true,
        "data": {
            "status": "ok",
            "draining": state.draining(),
            "uptime_seconds": state.uptime_seconds(),
        }
    }))
}

/// Readiness. 503 while draining, and 503 before any model can answer, so a
/// compose healthcheck or load balancer waits for a node that has actually
/// finished loading.
async fn handle_ready(State(state): State<Arc<ServerState>>) -> Response {
    let models = state.ready_models();
    let draining = state.draining();
    let ready = !draining && !models.is_empty();
    let reason = if draining {
        "draining"
    } else if models.is_empty() {
        "no model is loaded and ready, and no external provider is configured"
    } else {
        "ok"
    };
    let body = Json(json!({
        "success": ready,
        "data": { "ready": ready, "reason": reason, "models": models },
    }));
    if ready {
        (StatusCode::OK, body).into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    }
}

async fn handle_metrics(State(state): State<Arc<ServerState>>) -> Response {
    let cluster_members = match &state.cluster {
        Some(cluster) => cluster.members().await.len(),
        None => 1,
    };
    let enabled_on_disk = models::inventory(&state.settings.root)
        .map(|inv| inv.enabled().len())
        .unwrap_or(0);
    let snapshot = Snapshot {
        version: env!("CARGO_PKG_VERSION"),
        features: crate::metrics::features(),
        start_unix_seconds: state.start_unix_seconds(),
        models_loaded: state.engine.get_active_models().len(),
        models_enabled_on_disk: enabled_on_disk,
        ready: state.ready(),
        draining: state.draining(),
        cluster_members,
    };
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        format!(
            "{}{}",
            state.metrics.render(&snapshot),
            state.managed.metrics()
        ),
    )
        .into_response()
}

/// Where the packages install the dashboard, relative to the engine root.
pub const WEB_UI_DIR: &str = "server/ui";

/// Locate the built dashboard (a directory with `index.html`). An explicit
/// `--web-ui` / `web_ui` / `POSTVEC_SERVER_WEB_UI` is the only candidate
/// when set; otherwise `<root>/server/ui`, where the packages put it.
pub fn resolve_web_ui(explicit: Option<&Path>, root: &Path) -> Option<PathBuf> {
    let dir = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(|| root.join(WEB_UI_DIR));
    if dir.join("index.html").is_file() {
        return Some(dir);
    }
    if explicit.is_some() {
        log::warn!(
            "web UI path {} has no index.html; the dashboard will not be served",
            dir.display()
        );
    }
    None
}

async fn no_spa() -> Json<Value> {
    Json(json!({
        "success": true,
        "data": "Web front-end is not configured. Build postvec-server/web-ui and pass --web-ui <dir>, or install it at <root>/server/ui."
    }))
}

/// Shared routes (discovery, health, native `/api/{model}`, OpenAI adaptor,
/// the registry). The public listener adds CORS + the SPA fallback; the
/// admin listener merges mutation routes and serves the dashboard without
/// cross-origin access. `manage` mounts the registry's mutating routes too: always on
/// the admin listener, on the public one only by explicit choice.
pub fn router(metrics_enabled: bool, manage: bool) -> Router<Arc<ServerState>> {
    // Native `/api/{model}`, the OpenAI adaptor and the registry share this
    // nest. Their static paths are two segments, so they never collide with
    // `/{model_name}`. Body cap matches the gRPC decode ceiling.
    let api = crate::registry::router(manage)
        .route("/openai/embeddings", post(api::openai_embeddings))
        .route("/:model_name", get(api::model_details).post(api::predict))
        .fallback(api::api_not_found)
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024));

    let mut router = Router::new()
        .route("/config", get(handle_config))
        .route("/health", get(handle_health))
        .route("/ready", get(handle_ready))
        .nest("/api", api);
    if metrics_enabled {
        router = router.route("/metrics", get(handle_metrics));
    }
    router
}

/// CORS + optional SPA. Applied only on the published listener.
pub fn finish_public(router: Router<Arc<ServerState>>, state: Arc<ServerState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    finish_ui(router.layer(cors), state)
}

pub(crate) fn finish_ui(router: Router<Arc<ServerState>>, state: Arc<ServerState>) -> Router {
    let web_ui = resolve_web_ui(state.settings.web_ui.as_deref(), &state.settings.root);
    let router = if let Some(dir) = web_ui {
        log::info!("serving UI from {}", dir.display());
        let index = dir.join("index.html");
        // `fallback` (not `not_found_service`): SPA client routes must stay
        // HTTP 200 with index.html. `not_found_service` forces 404.
        router.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index)))
    } else {
        router.route("/", get(no_spa))
    };
    router.with_state(state)
}

/// A running listener, with the handle that drains it.
pub struct Listener {
    pub bound: SocketAddr,
    pub handle: axum_server::Handle,
    pub task: tokio::task::JoinHandle<std::io::Result<()>>,
}

/// Spawn the public listener on an already-reserved socket.
pub fn spawn(
    state: Arc<ServerState>,
    std_listener: std::net::TcpListener,
) -> Result<Listener, String> {
    let bound = std_listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;

    let handle = axum_server::Handle::new();
    let service = finish_public(
        router(state.settings.metrics, state.settings.manage),
        state.clone(),
    )
    .into_make_service();
    let tls = state.settings.tls.clone();

    let task = match tls {
        Some(paths) => {
            let config =
                axum_server::tls_openssl::OpenSSLConfig::from_pem_file(&paths.cert, &paths.key)
                    .map_err(|e| {
                        format!(
                            "cannot load the TLS certificate ({}) and key ({}): {e}\n\
                     (generate a pair, point --ssl-cert/--ssl-cert-key at one, or pass \
                     --insecure to serve discovery over plain HTTP)",
                            paths.cert.display(),
                            paths.key.display()
                        )
                    })?;
            log::info!("discovery listening on https://{bound}");
            let acceptor = axum_server::tls_openssl::OpenSSLAcceptor::new(config);
            let handle = handle.clone();
            tokio::spawn(async move {
                axum_server::from_tcp(std_listener)
                    .acceptor(acceptor)
                    .handle(handle)
                    .serve(service)
                    .await
            })
        }
        None => {
            log::warn!(
                "discovery listening on http://{bound} — TLS is disabled (--insecure); use it \
                 for local development and CI, not for a deployed fleet"
            );
            let handle = handle.clone();
            tokio::spawn(async move {
                axum_server::from_tcp(std_listener)
                    .handle(handle)
                    .serve(service)
                    .await
            })
        }
    };

    Ok(Listener {
        bound,
        handle,
        task,
    })
}

/// Warn once, loudly, when the reachable surfaces are bound to every
/// interface. There is no authentication on either of them.
pub fn warn_about_exposure(bind: std::net::IpAddr, grpc_port: u16, http_port: u16, manage: bool) {
    if bind.is_unspecified() {
        log::warn!(
            "gRPC ({grpc_port}) and HTTP ({http_port}) are bound to every interface. \
             Neither is authenticated{}. \
             Restrict them to a private network with a firewall, a security group, or \
             --bind <private-ip>.",
            if manage {
                ", and HTTP model management is enabled"
            } else {
                ""
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    // Local discovery envelope. postvec cannot be linked from here
    // (pgrx, workspace-excluded). If a change here breaks these
    // structs, it breaks every installed extension.

    #[derive(Debug, Deserialize)]
    struct Envelope {
        #[serde(default)]
        success: bool,
        data: Option<ConfigData>,
    }

    #[derive(Debug, Deserialize)]
    struct ConfigData {
        #[serde(default)]
        models: Vec<HubModel>,
    }

    #[derive(Debug, Deserialize)]
    struct HubModel {
        name: String,
        configuration: Option<Cfg>,
        #[serde(flatten)]
        #[allow(dead_code)]
        rest: Map<String, Value>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Cfg {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        params: Params,
        #[serde(flatten)]
        #[allow(dead_code)]
        rest: Map<String, Value>,
    }

    #[derive(Debug, Deserialize, Default)]
    struct Params {
        model_type: Option<String>,
        source_model: Option<String>,
        target_model: Option<String>,
        source_dim: Option<u32>,
        target_dim: Option<u32>,
        #[serde(flatten)]
        #[allow(dead_code)]
        rest: Map<String, Value>,
    }

    fn model(name: &str, enabled: bool, params: &[(&str, Value)]) -> ModelConfiguration {
        let mut cfg = ModelConfiguration {
            name: name.to_string(),
            enabled,
            ..Default::default()
        };
        for (k, v) in params {
            cfg.params.insert(k.to_string(), v.clone());
        }
        cfg
    }

    /// The envelope postvec parses, with every addition present.
    fn full_envelope() -> Value {
        let models = config_models(&[
            model(
                "baai-bge-m3",
                true,
                &[("model_type", json!("embed")), ("target_dim", json!(1024))],
            ),
            model(
                "convert-a-to-b",
                true,
                &[
                    ("model_type", json!("convert")),
                    ("source_model", json!("a")),
                    ("target_model", json!("b")),
                    ("source_dim", json!(768)),
                    ("target_dim", json!(1024)),
                ],
            ),
        ]);
        json!({
            "success": true,
            "data": {
                "models": models,
                "server": { "name": "postvec-server", "version": "0.1.0", "draining": false },
                "cluster": { "group": "postvec", "nodes": [] },
                "system": { "memory_total_bytes": 0 },
            }
        })
    }

    #[test]
    fn postvecs_discovery_client_parses_this_envelope() {
        let raw = serde_json::to_string(&full_envelope()).unwrap();
        let envelope: Envelope = serde_json::from_str(&raw).expect("postvec must parse /config");
        assert!(envelope.success);
        let data = envelope.data.expect("data object");
        assert_eq!(data.models.len(), 2);

        let embed = &data.models[0];
        assert_eq!(embed.name, "baai-bge-m3");
        let cfg = embed.configuration.as_ref().expect("configuration");
        assert!(cfg.enabled, "enabled gates inclusion parser-side");
        assert_eq!(cfg.params.model_type.as_deref(), Some("embed"));
        assert_eq!(cfg.params.target_dim, Some(1024));

        let convert = data.models[1].configuration.as_ref().unwrap();
        assert_eq!(convert.params.source_model.as_deref(), Some("a"));
        assert_eq!(convert.params.target_model.as_deref(), Some("b"));
        assert_eq!(convert.params.source_dim, Some(768));
    }

    /// The additions are additions: an older parser ignores them.
    #[test]
    fn extra_envelope_keys_do_not_disturb_the_parser() {
        let mut envelope = full_envelope();
        envelope["data"]["something_from_the_future"] = json!({"a": 1});
        envelope["data"]["models"][0]["another_new_field"] = json!("x");
        let parsed: Envelope = serde_json::from_value(envelope).unwrap();
        assert_eq!(parsed.data.unwrap().models.len(), 2);
    }

    /// A disabled model still appears; the parser is what filters it. Keeping
    /// that split means the server never has to guess which consumers care.
    #[test]
    fn disabled_models_are_rendered_and_filtered_by_the_consumer() {
        let raw = json!({
            "success": true,
            "data": { "models": config_models(&[model("off", false, &[])]) }
        });
        let parsed: Envelope = serde_json::from_value(raw).unwrap();
        let models = parsed.data.unwrap().models;
        assert_eq!(models.len(), 1);
        assert!(!models[0].configuration.as_ref().unwrap().enabled);
    }

    #[test]
    fn the_model_entry_shape_is_exactly_name_status_configuration() {
        let rendered = config_models(&[model("m", true, &[])]);
        let object = rendered[0].as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["configuration", "name", "status"]);
        assert_eq!(object["status"], json!("local"));
    }

    /// Provider entries merge after the engine's own models in the nested
    /// HubModel shape, collisions resolve local-wins, and the transcribed
    /// postvec parser reads the provider row exactly like a local embed
    /// model (with the provider recorded as a top-level extra).
    #[test]
    fn provider_models_merge_into_the_envelope_and_local_wins() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `tempfile` honours the umask.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // …and its parent (`/tmp` is sticky, so only the tempdir matters).
        let path = dir.path().join("openai.toml");
        std::fs::write(
            &path,
            "provider = \"openai\"\napi_key = \"sk-test\"\n\n\
             [[models]]\nname = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 1536\nmax_tokens = 8191\n\n\
             [[models]]\nname = \"baai-bge-m3\"\nprovider_model_id = \"collides\"\ndim = 999\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let gateway = providers::gateway::Gateway::load(dir.path(), &Default::default());

        let configs = [model(
            "baai-bge-m3",
            true,
            &[("model_type", json!("embed")), ("target_dim", json!(1024))],
        )];
        let rendered = merged_config_models(&configs, &gateway);
        let raw = json!({ "success": true, "data": { "models": rendered } });
        let parsed: Envelope = serde_json::from_value(raw).unwrap();
        let models = parsed.data.unwrap().models;

        // Local wins the collision: one row, the engine's dim.
        let local: Vec<&HubModel> = models.iter().filter(|m| m.name == "baai-bge-m3").collect();
        assert_eq!(local.len(), 1, "local model wins the name collision");
        assert_eq!(
            local[0].configuration.as_ref().unwrap().params.target_dim,
            Some(1024)
        );

        let provider = models
            .iter()
            .find(|m| m.name == "openai-text-embedding-3-small")
            .expect("provider model served");
        let cfg = provider.configuration.as_ref().unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.params.model_type.as_deref(), Some("embed"));
        assert_eq!(
            cfg.params.target_model.as_deref(),
            Some("openai-text-embedding-3-small")
        );
        assert_eq!(cfg.params.target_dim, Some(1536));
        // The provider marker rides as a top-level extra (raw.extra in
        // postvec's cache) — what the NOTICE and the fleet labeling read.
        assert_eq!(provider.rest.get("provider"), Some(&json!("openai")));
    }

    #[test]
    fn the_system_object_has_the_expected_keys() {
        let system = system_object();
        for key in [
            "memory_total_bytes",
            "memory_available_bytes",
            "memory_used_bytes",
        ] {
            assert!(system.get(key).is_some(), "missing {key}");
        }
    }
}
