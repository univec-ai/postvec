// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Loopback admin listener: `POST /admin/load`, `POST /admin/unload` and
//! `POST /admin/providers/reload`.
//!
//! These routes mutate the engine and have no auth. They bind `127.0.0.1`
//! only. The per-request peer check is a second line behind that bind.
//! Read-only routes are mirrored so the node-local CLI can hit `/config`
//! over plain loopback HTTP.
//!
//! `postvec-server load` catches one node's engine up to disk.

use crate::models::{self, DescriptorIndex};
use crate::state::ServerState;
use axum::extract::rejection::StringRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use engine::InferenceEngine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Request body cap. Generous for 32 short names; anything larger is not a
/// request this API has a shape for.
const BODY_LIMIT: usize = 64 * 1024;
/// At most this many models per request.
const MAX_MODELS_PER_REQUEST: usize = 32;

#[derive(serde::Deserialize)]
struct AdminRequest {
    models: Vec<String>,
}

/// A request-level refusal: `{success: false, error}` on a 4xx — the shape
/// the client decodes even for error statuses.
fn refusal(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "success": false, "error": message })))
}

/// One per-model outcome inside a 200 envelope.
pub(crate) fn model_result(model: &str, status: &str, error: Option<String>) -> Value {
    match error {
        Some(e) => json!({ "model": model, "status": status, "error": e }),
        None => json!({ "model": model, "status": status }),
    }
}

fn results_envelope(results: Vec<Value>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({ "success": true, "data": { "results": results } })),
    )
}

/// Defence in depth behind the loopback bind. The listener bind and the
/// request peer are checked in different places.
fn peer_check(peer: SocketAddr) -> Result<(), (StatusCode, Json<Value>)> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err(refusal(
            StatusCode::FORBIDDEN,
            "the admin endpoints answer loopback peers only",
        ))
    }
}

/// Decode and bound a request body. An oversized or non-UTF-8 body is
/// re-wrapped into the JSON envelope.
fn parse_request(
    body: Result<String, StringRejection>,
) -> Result<Vec<String>, (StatusCode, Json<Value>)> {
    let body = body.map_err(|rej| refusal(rej.status(), &rej.body_text()))?;
    let request: AdminRequest = serde_json::from_str(&body).map_err(|e| {
        refusal(
            StatusCode::BAD_REQUEST,
            &format!("invalid request body: {e}"),
        )
    })?;
    if request.models.is_empty() {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            "no models named in the request",
        ));
    }
    if request.models.len() > MAX_MODELS_PER_REQUEST {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_MODELS_PER_REQUEST} models per request"),
        ));
    }
    for name in &request.models {
        models::validate_model_name(name).map_err(|e| refusal(StatusCode::BAD_REQUEST, &e))?;
    }
    Ok(request.models)
}

// ---- /admin/load -------------------------------------------------------

async fn admin_load(state: Arc<ServerState>, names: Vec<String>) -> (StatusCode, Json<Value>) {
    match load_models(state, names).await {
        Ok(results) => results_envelope(results),
        Err(e) => refusal(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Load `names`, one outcome each. Shared with the registry's activate.
pub(crate) async fn load_models(
    state: Arc<ServerState>,
    names: Vec<String>,
) -> Result<Vec<Value>, String> {
    // Detach the load. If the client disconnects, this task still holds
    // the lifecycle lock and finishes.
    let lifecycle = state.lifecycle.clone();
    let root = state.settings.root.clone();
    let outcome = tokio::spawn(async move {
        let _guard = lifecycle.lock().await;
        // Index once per request, off the serving runtime.
        let index = match tokio::task::spawn_blocking(move || models::descriptor_index(&root))
            .await
            .map_err(|e| format!("descriptor scan task failed: {e}"))
            .and_then(|r| r)
        {
            Ok(index) => Arc::new(index),
            Err(e) => {
                return names
                    .iter()
                    .map(|name| model_result(name, "error", Some(e.clone())))
                    .collect()
            }
        };
        let mut results = Vec::with_capacity(names.len());
        for name in &names {
            results.push(load_one(&state, &index, name).await);
        }
        results
    })
    .await;
    outcome.map_err(|e| format!("admin load task failed: {e}"))
}

async fn load_one(state: &ServerState, index: &Arc<DescriptorIndex>, name: &str) -> Value {
    // `--models` is the eligibility policy for admin loads too. Empty
    // means any enabled descriptor.
    let allowed = &state.settings.models;
    if !allowed.is_empty() && !allowed.iter().any(|a| a == name) {
        return model_result(
            name,
            "error",
            Some(format!(
                "model {name:?} is not in the configured --models list; add it there and \
                 restart, or clear the list to allow any enabled descriptor"
            )),
        );
    }

    let path = match index.get(name).map(Vec::as_slice) {
        None | Some([]) => {
            return model_result(
                name,
                "error",
                Some(format!(
                    "model {name:?} is not on disk under {}/<backend>/ — pull it first \
                     (`postvec model pull {name}`)",
                    models::MODELS_DIR
                )),
            )
        }
        Some([path]) => path.clone(),
        Some(_) => {
            return model_result(
                name,
                "error",
                Some(format!(
                    "model {name:?} exists under multiple backends; the engine would resolve \
                     it by directory order, so remove the duplicate directory"
                )),
            )
        }
    };

    let descriptor_path = path.clone();
    let descriptor = match tokio::task::spawn_blocking(move || {
        models::read_descriptor(&descriptor_path)
    })
    .await
    {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return model_result(name, "error", Some(e)),
        Err(e) => return model_result(name, "error", Some(format!("descriptor read failed: {e}"))),
    };
    if !descriptor.enabled {
        return model_result(
            name,
            "error",
            Some(format!(
                "descriptor is deactivated ({}); run `postvec model activate {name}` first — \
                 the engine refuses a deactivated descriptor on every load path, so a load \
                 here would not survive a restart",
                path.display()
            )),
        );
    }

    // Readiness is pool AND executor, so a load in its brief optimistic
    // registration window does not report "already-loaded".
    if state.engine.is_model_ready(name) {
        return model_result(name, "already-loaded", None);
    }

    // The resident ceiling counts the whole not-yet-ready dependency
    // closure, because the requested name alone undercounts a bridge chain.
    let closure_index = index.clone();
    let closure_name = name.to_string();
    let closure = match tokio::task::spawn_blocking(move || {
        models::admin_load_closure(&closure_index, &closure_name)
    })
    .await
    {
        Ok(Ok(closure)) => closure,
        Ok(Err(e)) => return model_result(name, "error", Some(e)),
        Err(e) => return model_result(name, "error", Some(format!("closure scan failed: {e}"))),
    };
    let ceiling = state.settings.max_resident_models;
    let resident = state.engine.get_active_models().len();
    let additions = closure
        .iter()
        .filter(|candidate| !state.engine.is_model_loaded(candidate))
        .count();
    if resident + additions > ceiling {
        return model_result(
            name,
            "error",
            Some(format!(
                "loading {name:?} would raise resident models to {} (ceiling {ceiling}); \
                 unload something first or raise --max-resident-models",
                resident + additions
            )),
        );
    }

    match state.engine.load_model(name).await {
        Ok(()) if state.engine.get_active_models().len() <= ceiling => {
            state.metrics.admin_loaded(1);
            log::info!("loaded model {name:?} through the admin port");
            model_result(name, "loaded", None)
        }
        Ok(()) => model_result(
            name,
            "error",
            Some(format!(
                "the engine exceeded the resident ceiling of {ceiling} despite the preflight; \
                 unload models before serving inference"
            )),
        ),
        Err(e) => model_result(name, "error", Some(e.to_string())),
    }
}

// ---- /admin/unload -----------------------------------------------------

/// Best-effort direct-dependency edges, read from the same descriptors
/// `load_model` parses. A missing or broken descriptor skips ordering
/// for that model; the unload still proceeds.
fn request_dependencies(index: &DescriptorIndex, names: &[String]) -> HashMap<String, Vec<String>> {
    let mut deps = HashMap::new();
    for name in names {
        if let Some([path]) = index.get(name).map(Vec::as_slice) {
            if let Ok(descriptor) = models::read_descriptor(path) {
                deps.insert(name.clone(), descriptor.dependencies);
            }
        }
    }
    deps
}

/// Order the requested set so a dependent unloads before anything it
/// depends on, the reverse of `load_model`'s dependencies-first recursion.
/// Duplicates collapse to their first occurrence; a cycle falls back to
/// request order. Quadratic, and fine: at most [`MAX_MODELS_PER_REQUEST`] names.
fn reverse_dependency_order(names: &[String], deps: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut remaining: Vec<&str> = Vec::new();
    for name in names {
        if !remaining.contains(&name.as_str()) {
            remaining.push(name);
        }
    }
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let next = remaining
            .iter()
            .position(|&candidate| {
                !remaining.iter().any(|&other| {
                    other != candidate
                        && deps
                            .get(other)
                            .is_some_and(|d| d.iter().any(|dep| dep == candidate))
                })
            })
            .unwrap_or(0);
        ordered.push(remaining.remove(next).to_string());
    }
    ordered
}

async fn admin_unload(state: Arc<ServerState>, names: Vec<String>) -> (StatusCode, Json<Value>) {
    match unload_models(state, names).await {
        Ok(results) => results_envelope(results),
        Err(e) => refusal(StatusCode::INTERNAL_SERVER_ERROR, &e),
    }
}

/// Unload `names`, dependants first. Shared with the registry's deactivate.
pub(crate) async fn unload_models(
    state: Arc<ServerState>,
    names: Vec<String>,
) -> Result<Vec<Value>, String> {
    let lifecycle = state.lifecycle.clone();
    let root = state.settings.root.clone();
    let outcome = tokio::spawn(async move {
        let _guard = lifecycle.lock().await;
        let dep_names = names.clone();
        let deps = tokio::task::spawn_blocking(move || {
            models::descriptor_index(&root)
                .map(|index| request_dependencies(&index, &dep_names))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default();

        let mut results = Vec::with_capacity(names.len());
        for name in reverse_dependency_order(&names, &deps) {
            // `unload_model` is synchronous: it takes the engine write
            // locks and tears down native sessions as the pool drops.
            let engine: Arc<InferenceEngine> = state.engine.clone();
            let model = name.clone();
            let result = tokio::task::spawn_blocking(move || engine.unload_model(&model)).await;
            results.push(match result {
                Ok(true) => {
                    state.metrics.admin_unloaded(1);
                    log::info!("unloaded model {name:?} through the admin port");
                    model_result(&name, "unloaded", None)
                }
                Ok(false) => model_result(&name, "not-loaded", None),
                Err(e) => model_result(&name, "error", Some(format!("unload task failed: {e}"))),
            });
        }
        results
    })
    .await;
    outcome.map_err(|e| format!("admin unload task failed: {e}"))
}

// ---- /admin/providers/reload --------------------------------------------

/// Rescan providers.d and swap the gateway snapshot in atomically.
/// `postvec provider add/rm --path <root>` calls this after writing
/// files; a restart also picks changes up. A failed structural reload
/// keeps the previous snapshot.
async fn providers_reload(state: Arc<ServerState>) -> (StatusCode, Json<Value>) {
    let gateway = state.gateway.clone();
    let path = state.settings.providers_path.clone();
    // Read fresh at reload time, not at boot: a provider file claiming a name
    // the engine now owns must be refused now.
    let engine = state.engine.clone();
    let root = state.settings.root.clone();
    // Directory this node reads. Reported in the body so `postvec provider
    // ... --path DIR` can tell which loopback host answered.
    let served_path = path.display().to_string();
    // providers.d scanning is filesystem work (stat, read, key files) —
    // keep it off the serving runtime's workers like the other admin routes.
    let outcome = tokio::task::spawn_blocking(move || {
        // Fail closed: a scan failure keeps the previous snapshot.
        let local = models::reserved_local_names(&root, &engine)?;
        gateway.reload(&path, &local)
    })
    .await;
    match outcome {
        Ok(Err(e)) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("reload failed; previous providers kept: {e}"),
        ),
        Err(e) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("reload task failed: {e}"),
        ),
        Ok(Ok(report)) => (
            StatusCode::OK,
            Json(json!({
                "success": true,
                "data": {
                    "path": served_path,
                    "providers": report.providers,
                    "models": report.models,
                    "errors": report.errors,
                }
            })),
        ),
    }
}

// ---- Listener ----------------------------------------------------------

pub fn router(state: Arc<ServerState>) -> Router {
    let load_state = state.clone();
    let unload_state = state.clone();
    let reload_state = state.clone();
    // Read-only routes ride along so the node-local CLI can read `/config`
    // over plain loopback HTTP.
    let router = crate::http::router(state.settings.metrics, true)
        .merge(crate::managed::routes())
        .route(
            "/admin/load",
            post(
                move |ConnectInfo(peer): ConnectInfo<SocketAddr>,
                      body: Result<String, StringRejection>| {
                    let state = load_state.clone();
                    async move {
                        if let Err(refused) = peer_check(peer) {
                            state.metrics.admin_refused();
                            return refused;
                        }
                        match parse_request(body) {
                            Ok(models) => admin_load(state, models).await,
                            Err(refused) => {
                                state.metrics.admin_refused();
                                refused
                            }
                        }
                    }
                },
            ),
        )
        .route(
            "/admin/unload",
            post(
                move |ConnectInfo(peer): ConnectInfo<SocketAddr>,
                      body: Result<String, StringRejection>| {
                    let state = unload_state.clone();
                    async move {
                        if let Err(refused) = peer_check(peer) {
                            state.metrics.admin_refused();
                            return refused;
                        }
                        match parse_request(body) {
                            Ok(models) => admin_unload(state, models).await,
                            Err(refused) => {
                                state.metrics.admin_refused();
                                refused
                            }
                        }
                    }
                },
            ),
        )
        .route(
            "/admin/providers/reload",
            post(move |ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                let state = reload_state.clone();
                async move {
                    if let Err(refused) = peer_check(peer) {
                        state.metrics.admin_refused();
                        return refused;
                    }
                    providers_reload(state).await
                }
            }),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT));
    crate::http::finish_ui(router, state)
}

/// Bind and spawn the loopback admin listener.
pub fn spawn(
    state: Arc<ServerState>,
    std_listener: std::net::TcpListener,
) -> Result<crate::http::Listener, String> {
    let bound = std_listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    // Fail the boot rather than expose an unauthenticated mutation surface.
    if !bound.ip().is_loopback() {
        return Err(format!(
            "the admin listener is bound to {bound}, which is not loopback; these routes \
             mutate the engine and have no authentication, so they never bind a routable \
             address"
        ));
    }
    log::info!("admin listening on http://{bound} (loopback only)");

    let handle = axum_server::Handle::new();
    let service = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let task = {
        let handle = handle.clone();
        tokio::spawn(async move {
            axum_server::from_tcp(std_listener)
                .handle(handle)
                .serve(service)
                .await
        })
    };

    Ok(crate::http::Listener {
        bound,
        handle,
        task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn body(json: &str) -> Result<String, StringRejection> {
        Ok(json.to_string())
    }

    #[test]
    fn non_loopback_peers_are_refused() {
        let (status, _) = peer_check("10.0.0.5:5000".parse().unwrap()).unwrap_err();
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(peer_check("127.0.0.1:5000".parse().unwrap()).is_ok());
        assert!(peer_check("[::1]:5000".parse().unwrap()).is_ok());
    }

    #[test]
    fn requests_are_bounded_and_validated() {
        assert_eq!(
            parse_request(body(r#"{"models":["a","b"]}"#)).unwrap(),
            names(&["a", "b"])
        );

        let (status, Json(e)) = parse_request(body(r#"{"models":[]}"#)).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("no models"));

        let many = format!(
            r#"{{"models":[{}]}}"#,
            (0..=MAX_MODELS_PER_REQUEST)
                .map(|i| format!("\"m{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let (status, _) = parse_request(body(&many)).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Path-like names never reach the filesystem.
        let (status, Json(e)) =
            parse_request(body(r#"{"models":["../../etc/passwd"]}"#)).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("must start with"));

        let (status, _) = parse_request(body("not json")).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn dependents_unload_before_their_dependencies() {
        let mut deps = HashMap::new();
        deps.insert("bridge".to_string(), names(&["converter"]));
        deps.insert("converter".to_string(), names(&["base"]));
        let ordered = reverse_dependency_order(&names(&["base", "converter", "bridge"]), &deps);
        assert_eq!(ordered, names(&["bridge", "converter", "base"]));
    }

    #[test]
    fn unload_ordering_tolerates_duplicates_and_cycles() {
        let mut deps = HashMap::new();
        deps.insert("a".to_string(), names(&["b"]));
        deps.insert("b".to_string(), names(&["a"]));
        let ordered = reverse_dependency_order(&names(&["a", "b", "a"]), &deps);
        assert_eq!(ordered.len(), 2, "duplicates collapse: {ordered:?}");

        let ordered = reverse_dependency_order(&names(&["solo"]), &HashMap::new());
        assert_eq!(ordered, names(&["solo"]));
    }

    /// The reload route swaps the gateway snapshot in and widens the
    /// ingress permits by the new provider budget.
    #[tokio::test]
    async fn providers_reload_swaps_the_gateway_and_widens_ingress() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("models").join("onnx-runtime")).unwrap();
        let settings = crate::config::resolve(
            &crate::cli::ServeArgs::default(),
            &crate::config::FileConfig::default(),
            &std::collections::BTreeMap::new(),
            root.path().to_path_buf(),
            None,
        )
        .unwrap();
        let engine = Arc::new(InferenceEngine::new(Arc::new(engine::EngineConfig {
            root_path: root.path().to_path_buf(),
            host_policy: Default::default(),
        })));
        let identity = crate::state::NodeIdentity {
            advertise: "127.0.0.1".parse().unwrap(),
            api_address: "http://127.0.0.1:22222".to_string(),
            grpc_address: "127.0.0.1:33333".to_string(),
            frontend: "http://127.0.0.1:22222".to_string(),
        };
        // Boot with an empty (nonexistent) providers.d: budget 0.
        let gateway = Arc::new(providers::gateway::Gateway::load(
            &settings.providers_path,
            &Default::default(),
        ));
        let providers_path = settings.providers_path.clone();
        let state = ServerState::new(
            engine,
            Arc::new(settings),
            identity,
            Arc::new(crate::metrics::Metrics::new()),
            None,
            gateway,
        );
        let ingress = state.gateway.ingress(1);
        assert_eq!(ingress.available_permits(), 1);

        // The operator adds the first provider file, then reloads.
        std::fs::create_dir_all(&providers_path).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&providers_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        // …and its parent: the loader refuses a providers.d whose ancestor is
        // group-writable, and `tempfile`/`create_dir_all` honour the umask.
        if let Some(parent) = &providers_path.parent() {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let file = providers_path.join("openai.toml");
        std::fs::write(
            &file,
            "provider = \"openai\"\napi_key = \"sk-test\"\n\n[[models]]\n\
             name = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

        let (status, Json(body)) = providers_reload(state.clone()).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["models"], json!(1));
        assert_eq!(
            ingress.available_permits(),
            1 + state.gateway.inflight_budget()
        );
        // The directory this node reads, so `postvec provider … --path DIR`
        // can tell "I reloaded your files" from "some other host answered".
        assert_eq!(
            body["data"]["path"],
            json!(providers_path.display().to_string())
        );
        assert!(state.gateway.owns("openai-text-embedding-3-small"));
    }

    #[test]
    fn per_model_outcomes_carry_their_error() {
        let ok = model_result("m", "loaded", None);
        assert_eq!(ok, json!({"model": "m", "status": "loaded"}));
        let bad = model_result("m", "error", Some("boom".into()));
        assert_eq!(bad["error"], json!("boom"));
    }
}
