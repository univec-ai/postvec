//! The engine host's loopback HTTP listener (embedded mode): `GET /config`
//! model discovery plus the `POST /admin/load` / `POST /admin/unload`
//! controls.
//!
//! `/config` serves the same `{success, data: {models: [...]}}` envelope
//! the standalone inference host's HTTP API serves, rendered from the in-process engine's
//! loaded models — so postvec's existing discovery client
//! (`client/discovery.rs`) consumes it unchanged. This is what lets the
//! launcher-hosted engine feed the model caches of per-database workers
//! (which have no engine of their own), and what makes the SQL
//! `refresh_models()` work in embedded mode.
//!
//! The `/admin/*` routes are what `postvec model pull`/`rm`/`activate` use
//! to hot-(un)load models without a PostgreSQL restart. They answer the
//! same envelope shape: per-model outcomes ride a 200, request-level
//! refusals are a 4xx with `{success: false, error}`.
//!
//! Runs entirely on the engine runtime. Nothing here may touch Postgres
//! (no SPI, no pgrx `elog`) — logging goes through the `log` crate, which
//! embedded mode bridges to stderr.

use axum::extract::rejection::StringRejection;
use axum::extract::{ConnectInfo, DefaultBodyLimit};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use engine::{InferenceEngine, ModelConfiguration};
use providers::gateway::Gateway;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Request body cap for the admin routes. Generous for 32 short names;
/// anything bigger is not a postvec-cli request.
const ADMIN_BODY_LIMIT: usize = 64 * 1024;

/// At most this many models per admin request.
const ADMIN_MAX_MODELS: usize = 32;

/// Longest accepted model (directory) name, in bytes.
const ADMIN_MAX_NAME_BYTES: usize = 128;

/// Render the `/config` envelope for a set of model configurations plus the
/// provider gateway's models — byte-compatible with what
/// `discovery::parse_config` expects (`configuration.enabled` gates
/// inclusion parser-side, `configuration.params` carries
/// model_type/source/target/dims). Provider entries come after the engine's
/// own, already in the nested HubModel shape; a
/// public name that collides with a local model is skipped with a warning —
/// the local model wins, deterministically. Pure, so the wire shape is
/// testable without an engine.
pub(crate) fn config_envelope(configs: &[ModelConfiguration], gateway: &Gateway) -> Value {
    let mut models: Vec<Value> = configs
        .iter()
        .map(|cfg| {
            json!({
                "name": cfg.name,
                "status": "local",
                "configuration": serde_json::to_value(cfg).unwrap_or(Value::Null),
            })
        })
        .collect();
    for descriptor in gateway.models() {
        let name = descriptor["name"].as_str().unwrap_or_default();
        if configs.iter().any(|cfg| cfg.name == name) {
            // Local-by-default: an on-disk engine model keeps its name even
            // when a provider file claims it.
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
    json!({ "success": true, "data": { "models": models } })
}

fn engine_envelope(engine: &InferenceEngine, gateway: &Gateway) -> Value {
    let configs: Vec<ModelConfiguration> = engine
        .get_active_models()
        .into_iter()
        .filter_map(|name| engine.get_model(&name).ok())
        .map(|model| model.configuration().clone())
        .collect();
    config_envelope(&configs, gateway)
}

// ---- Admin request plumbing -------------------------------------------

#[derive(serde::Deserialize)]
struct AdminRequest {
    models: Vec<String>,
}

/// A deliberately minimal, tolerant descriptor schema — the extension-side
/// twin of postvec-cli's `DescriptorFile` (postvec-cli/src/engine/embedded.rs).
/// Not the engine's `ModelConfiguration`: the handler only needs admission
/// facts (`enabled`, `dependencies`); `load_model` re-parses the real schema
/// and stays authoritative for everything else. Absent `enabled` reads as
/// false, exactly as it does for the engine (`#[serde(default)] bool`).
#[derive(serde::Deserialize)]
struct AdminDescriptor {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// A request-level refusal: `{success: false, error}` on a 4xx, the shape
/// the CLI client decodes even for error statuses.
fn refusal(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "success": false, "error": message })))
}

/// One per-model outcome inside a 200 envelope.
fn model_result(model: &str, status: &str, error: Option<String>) -> Value {
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

/// `spawn` only warns about a non-loopback bind (a `/config` read exposes
/// what the inference host already advertises), but the admin routes mutate the
/// engine — so even on a hand-configured non-loopback listener they refuse
/// any peer that is not loopback. v1 deliberately trusts local OS users
/// and nobody else.
fn admin_peer_check(peer: SocketAddr) -> Result<(), (StatusCode, Json<Value>)> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err(refusal(
            StatusCode::FORBIDDEN,
            "admin endpoints answer loopback peers only",
        ))
    }
}

/// Validate a request-supplied model name *before* it goes anywhere near the
/// filesystem: names are joined into `<root>/models/<backend>/<name>/`, so
/// anything path-like is a request-level 400, never a per-model result.
fn validate_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model names must be non-empty".to_string());
    }
    if name.len() > ADMIN_MAX_NAME_BYTES {
        return Err(format!("model name exceeds {ADMIN_MAX_NAME_BYTES} bytes"));
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!("model name {name:?} must start with [a-z0-9]"));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "model name {name:?} contains characters outside [a-z0-9._-]"
        ));
    }
    Ok(())
}

/// Decode and bound an admin request body. An oversized or non-UTF-8 body
/// surfaces as the extractor rejection (413/400) re-wrapped into the JSON
/// envelope, so the CLI never sees a plain-text axum error page.
fn parse_admin_request(
    body: Result<String, StringRejection>,
) -> Result<Vec<String>, (StatusCode, Json<Value>)> {
    let body = body.map_err(|rej| refusal(rej.status(), &rej.body_text()))?;
    let request: AdminRequest = serde_json::from_str(&body).map_err(|e| {
        refusal(
            StatusCode::BAD_REQUEST,
            &format!("invalid request body: {e}"),
        )
    })?;
    if request.models.len() > ADMIN_MAX_MODELS {
        return Err(refusal(
            StatusCode::BAD_REQUEST,
            &format!("at most {ADMIN_MAX_MODELS} models per request"),
        ));
    }
    for name in &request.models {
        validate_model_name(name).map_err(|e| refusal(StatusCode::BAD_REQUEST, &e))?;
    }
    Ok(request.models)
}

// ---- Descriptor admission ---------------------------------------------

/// Locate `<root>/models/<backend>/<name>/ninference.hub.json` — exactly the
/// two-level layout the engine's (private) `find_model_config_path` scans.
/// Dot-directories (the CLI's `models/.staging`, for one) are never
/// backends.
///
/// A name present under **more than one** backend is an error, never a
/// first-match guess. The engine resolves by first
/// directory-name match, so acting on either copy from here would be
/// nondeterministic relative to what the engine actually loaded.
fn find_descriptor(root: &Path, name: &str) -> Result<Option<PathBuf>, String> {
    let Ok(backends) = std::fs::read_dir(root.join("models")) else {
        return Ok(None);
    };
    let mut hits: Vec<PathBuf> = Vec::new();
    for entry in backends.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let backend_dir = entry.path();
        if !backend_dir.is_dir() {
            continue;
        }
        let descriptor = backend_dir.join(name).join("ninference.hub.json");
        if descriptor.is_file() {
            hits.push(descriptor);
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => Ok(Some(hits.remove(0))),
        _ => Err(format!(
            "model {name:?} exists under multiple backends ({}); refusing an ambiguous \
             resolution — remove the duplicate directory",
            hits.iter()
                .filter_map(|p| p.parent().and_then(|d| d.parent()))
                .filter_map(|b| b.file_name())
                .map(|n| n.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn read_descriptor(path: &Path) -> Result<AdminDescriptor, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let descriptor: AdminDescriptor =
        serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
    if descriptor.name.trim().is_empty() {
        return Err(format!("{}: field \"name\" is empty", path.display()));
    }
    Ok(descriptor)
}

// ---- /admin/load -------------------------------------------------------

/// Admission control, then `load_model`, per requested model. `load_model`
/// itself recurses into `dependencies`, refuses a descriptor-name ↔
/// directory mismatch, rolls back partial loads and rebuilds the resolver —
/// the handler adds only what it does *not* do: the `enabled` check. The
/// startup scan honours that flag; the JIT path assumes its caller decided
/// Without the check, `/admin/load` would run a descriptor the
/// operator disabled — and since `resolver.rebuild` skips disabled configs,
/// the model would sit in the engine's model/executor maps while being
/// absent from the resolver indices: an inconsistent state.
async fn admin_load(
    engine: &Arc<InferenceEngine>,
    root: &Path,
    allowed: &[String],
    models: Vec<String>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
) -> (StatusCode, Json<Value>) {
    // Detach the WHOLE policy-check + load sequence, not just the inner
    // engine future. If the HTTP client disconnects, tokio drops this
    // handler but the spawned task retains the lifecycle mutex and finishes
    // commit-or-rollback. The same mutex serializes unloads, making the
    // resident-count check an atomic admission decision rather than a stale
    // observation shared by concurrent admin requests.
    let task_engine = engine.clone();
    let task_root = root.to_path_buf();
    let task_allowed = allowed.to_vec();
    let outcome = tokio::spawn(async move {
        let _lifecycle = lifecycle.lock().await;
        // All descriptor filesystem work runs on blocking threads: the
        // 2-worker engine runtime also serves loopback inference, and a
        // cold-cache/NFS model root must not stall it. The index is built
        // ONCE per request and shared by every per-name check below,
        // instead of rescanning the whole model root per requested name.
        let index = match tokio::task::spawn_blocking(move || {
            crate::client::embedded::descriptor_index(&task_root)
        })
        .await
        .map_err(|e| format!("descriptor scan task failed: {e}"))
        .and_then(|r| r)
        {
            Ok(index) => Arc::new(index),
            Err(e) => {
                return models
                    .iter()
                    .map(|name| model_result(name, "error", Some(e.clone())))
                    .collect();
            }
        };
        let mut results = Vec::with_capacity(models.len());
        for name in &models {
            results.push(load_one(&task_engine, &index, &task_allowed, name).await);
        }
        results
    })
    .await;
    match outcome {
        Ok(results) => results_envelope(results),
        Err(e) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("admin load task failed: {e}"),
        ),
    }
}

/// The number of models this loader may keep resident, shared with the
/// startup scan's ceiling: this is the ONLY post-startup load path (round 8
/// removed inference-request JIT loading), so together they bound resident
/// pools/sessions/threads for the process lifetime.
const MAX_RESIDENT_MODELS: usize = super::MAX_SCAN_LOADED_MODELS;

async fn load_one(
    engine: &Arc<InferenceEngine>,
    index: &Arc<std::collections::BTreeMap<String, Vec<PathBuf>>>,
    allowed: &[String],
    name: &str,
) -> Value {
    // `postvec.embedded_models` is the eligibility policy for every load,
    // not only the startup preload (round 8): with an explicit list set,
    // /admin/load may only name listed models (their descriptor
    // dependencies stay implicitly eligible, exactly as at startup). An
    // empty list means scan semantics — any enabled on-disk descriptor.
    if !allowed.is_empty() && !allowed.iter().any(|a| a == name) {
        return model_result(
            name,
            "error",
            Some(format!(
                "model {name:?} is not in postvec.embedded_models; add it there (and reload \
                 config with a restart) or clear the list to allow any enabled descriptor"
            )),
        );
    }
    // Same 0/1/many semantics as `find_descriptor`, served from the shared
    // per-request index instead of another directory walk.
    let path = match index.get(name).map(Vec::as_slice) {
        None | Some([]) => {
            return model_result(
                name,
                "error",
                Some(format!(
                    "model {name:?} not found on disk under models/<backend>/"
                )),
            )
        }
        Some([path]) => path.clone(),
        Some(_) => {
            return model_result(
                name,
                "error",
                Some(format!(
                    "model {name:?} exists under multiple backends; refusing an ambiguous \
                     resolution — remove the duplicate directory"
                )),
            )
        }
    };
    let descriptor_path = path.clone();
    let descriptor = match tokio::task::spawn_blocking(move || read_descriptor(&descriptor_path))
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
                "descriptor is disabled ({}); run `postvec model activate {name}`",
                path.display()
            )),
        );
    }
    // Readiness = pool AND executor: do not report "already-loaded" during
    // the load transaction's brief optimistic pool-registration window.
    if engine.is_model_ready(name) {
        return model_result(name, "already-loaded", None);
    }
    // Resident ceiling, counting the whole not-yet-ready dependency closure
    // this load would pull in — the request-visible name alone undercounts
    // bridge chains. The same walk refuses a closure containing a deactivated
    // model: `load_model` would refuse it mid-closure anyway, so catching it
    // here names the deactivated model and `postvec model activate` rather
    // than surfacing an engine error about a name the caller never asked for.
    // The traversal reads dependency descriptors from disk, so it runs on a
    // blocking thread too.
    let closure_index = index.clone();
    let closure_name = name.to_string();
    let closure = match tokio::task::spawn_blocking(move || {
        crate::client::embedded::admin_load_closure(&closure_index, &closure_name)
    })
    .await
    {
        Ok(Ok(closure)) => closure,
        Ok(Err(e)) => return model_result(name, "error", Some(e)),
        Err(e) => return model_result(name, "error", Some(format!("closure scan failed: {e}"))),
    };
    let resident = engine.get_active_models().len();
    let additions = closure
        .iter()
        .filter(|candidate| !engine.is_model_loaded(candidate))
        .count();
    if resident + additions > MAX_RESIDENT_MODELS {
        return model_result(
            name,
            "error",
            Some(format!(
                "loading {name:?} would raise resident models to {} (ceiling \
                 {MAX_RESIDENT_MODELS}); unload something first",
                resident + additions
            )),
        );
    }
    match engine.load_model(name).await {
        Ok(()) if engine.get_active_models().len() <= MAX_RESIDENT_MODELS => {
            model_result(name, "loaded", None)
        }
        Ok(()) => model_result(
            name,
            "error",
            Some(format!(
                "engine exceeded the hard resident ceiling of {MAX_RESIDENT_MODELS} despite \
                 preflight; unload models before inference"
            )),
        ),
        Err(e) => model_result(name, "error", Some(e.to_string())),
    }
}

// ---- /admin/unload -----------------------------------------------------

/// Best-effort direct-dependency edges for the requested set, read from the
/// same on-disk descriptors `load_model` parses. A missing or broken
/// descriptor just means no ordering information for that model — the
/// unload itself still proceeds.
fn request_dependencies(root: &Path, models: &[String]) -> HashMap<String, Vec<String>> {
    let mut deps = HashMap::new();
    for name in models {
        // An ambiguous (duplicate-backend) name yields no ordering
        // information — the unload itself is engine-keyed by name and still
        // proceeds; only *load* refuses ambiguity outright.
        if let Ok(Some(path)) = find_descriptor(root, name) {
            if let Ok(descriptor) = read_descriptor(&path) {
                deps.insert(name.clone(), descriptor.dependencies);
            }
        }
    }
    deps
}

/// Order the requested set so a requested dependent unloads before any
/// requested model it (transitively, within the set) depends on — the
/// mirror of `load_model`'s dependencies-first recursion. Duplicates
/// collapse to their first occurrence; a dependency cycle (impossible on a
/// well-formed root) degrades to request order for whatever remains.
/// Quadratic, and fine: requests hold at most [`ADMIN_MAX_MODELS`] names.
fn reverse_dependency_order(models: &[String], deps: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut remaining: Vec<&str> = Vec::new();
    for name in models {
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
            .unwrap_or(0); // cycle: fall back to request order
        ordered.push(remaining.remove(next).to_string());
    }
    ordered
}

async fn admin_unload(
    engine: &Arc<InferenceEngine>,
    root: &Path,
    models: Vec<String>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
) -> (StatusCode, Json<Value>) {
    let task_engine = engine.clone();
    let task_root = root.to_path_buf();
    let outcome = tokio::spawn(async move {
        let _lifecycle = lifecycle.lock().await;
        // Descriptor reads are filesystem work — keep them off the engine
        // runtime's 2 worker threads.
        let dep_models = models.clone();
        let deps =
            tokio::task::spawn_blocking(move || request_dependencies(&task_root, &dep_models))
                .await
                .unwrap_or_default();
        let ordered = reverse_dependency_order(&models, &deps);
        let mut results = Vec::with_capacity(ordered.len());
        for name in ordered {
            // `unload_model` is synchronous, takes all three engine write
            // locks, and dropping the pool tears down native sessions.
            let engine = task_engine.clone();
            let model = name.clone();
            let result = tokio::task::spawn_blocking(move || engine.unload_model(&model)).await;
            results.push(match result {
                Ok(true) => model_result(&name, "unloaded", None),
                Ok(false) => model_result(&name, "not-loaded", None),
                Err(e) => model_result(&name, "error", Some(format!("unload task failed: {e}"))),
            });
        }
        results
    })
    .await;
    match outcome {
        Ok(results) => results_envelope(results),
        Err(e) => refusal(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("admin unload task failed: {e}"),
        ),
    }
}

// ---- Listener ----------------------------------------------------------

/// Spawn the listener on the engine runtime. Binding happens synchronously
/// so a taken port / bad address fails the init attempt with a precise
/// error instead of a silent dead listener. `root` is the engine root the
/// admin handlers read descriptors from (the `enabled` admission check and
/// the unload ordering both live on disk, not in the engine).
/// Everything the listener needs about external providers: the shared
/// gateway, where its files live (`/admin/providers/reload` rescans them),
/// and the inflight budget the gRPC ingress limit was sized with at
/// spawn — a reload can grow the gateway past it; serving stays correct,
/// but full provider throughput needs a restart, and the reload handler
/// warns when that happens.
pub(super) struct ProviderState {
    pub(super) gateway: Arc<Gateway>,
    pub(super) providers_path: PathBuf,
    pub(super) startup_budget: usize,
}

pub(super) fn spawn(
    engine: Arc<InferenceEngine>,
    runtime: &tokio::runtime::Runtime,
    listen: &str,
    root: &Path,
    allowed_models: Vec<String>,
    providers: ProviderState,
) -> Result<(tokio::task::JoinHandle<()>, SocketAddr), String> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|e| format!("invalid postvec.embedded_http_listen {listen:?}: {e}"))?;
    // The listener has no authentication and its /admin routes mutate the
    // engine, so a non-loopback bind is refused outright. Failing engine
    // init is strictly better than exposing an unauthenticated admin
    // surface. The per-request loopback peer check below stays as
    // defence in depth.
    if !addr.ip().is_loopback() {
        return Err(format!(
            "postvec.embedded_http_listen {listen} is not a loopback address; the embedded \
             /config+/admin listener is unauthenticated and only ever binds loopback"
        ));
    }

    let std_listener =
        std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"))?;
    std_listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;
    // Registering the listener with tokio requires the runtime's reactor.
    let listener = {
        let _guard = runtime.enter();
        tokio::net::TcpListener::from_std(std_listener).map_err(|e| format!("from_std: {e}"))?
    };
    let bound = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;

    let config_engine = engine.clone();
    let config_gateway = providers.gateway.clone();
    let reload_gateway = providers.gateway;
    let providers_path = providers.providers_path;
    let startup_provider_budget = providers.startup_budget;
    let load_engine = engine.clone();
    let load_root = root.to_path_buf();
    let load_allowed = Arc::new(allowed_models);
    let admin_lifecycle = Arc::new(tokio::sync::Mutex::new(()));
    let load_lifecycle = admin_lifecycle.clone();
    // The reload route needs the engine and the model root: a provider file
    // claiming a name the engine *owns* — loaded or merely configured — is
    // refused at reload time, not left dormant.
    let reload_engine = engine.clone();
    let reload_root = root.to_path_buf();
    let unload_engine = engine;
    let unload_root = root.to_path_buf();
    let unload_lifecycle = admin_lifecycle;
    let app = Router::new()
        .route(
            "/config",
            get(move || {
                let engine = config_engine.clone();
                let gateway = config_gateway.clone();
                async move { Json(engine_envelope(&engine, &gateway)) }
            }),
        )
        .route(
            "/admin/providers/reload",
            post(move |ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                let gateway = reload_gateway.clone();
                let engine = reload_engine.clone();
                let root = reload_root.clone();
                let path = providers_path.clone();
                // The directory this host actually reads. Reported in the
                // body because `postvec provider … --path DIR` has to try
                // loopback listeners blind: without it, an embedded host
                // that happens to be running would answer a reload meant
                // for some other root and the CLI would report the change
                // as applied when nothing read it.
                let served_path = providers_path.display().to_string();
                async move {
                    if let Err(refused) = admin_peer_check(peer) {
                        return refused;
                    }
                    // providers.d scanning is filesystem work (stat, read,
                    // key files) — keep it off the 2-thread engine runtime,
                    // like the other admin handlers.
                    let outcome = tokio::task::spawn_blocking(move || {
                        // Fail closed: a scan failure keeps the previous
                        // snapshot rather than reloading against a narrowed
                        // reservation.
                        let local = crate::client::embedded::reserved_local_names(&root, &engine)?;
                        let report = gateway.reload(&path, &local)?;
                        Ok::<_, String>((report, gateway.inflight_budget()))
                    })
                    .await;
                    match outcome {
                        // A failed reload kept the previous snapshot; say so.
                        Ok(Err(e)) => refusal(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("reload failed; previous providers kept: {e}"),
                        ),
                        Err(e) => refusal(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("reload task failed: {e}"),
                        ),
                        Ok(Ok((report, budget))) => {
                            // The gRPC ingress width is fixed at spawn.
                            // Providers added by reload still serve, but
                            // they share the startup width until a restart.
                            // Say so once, here, and put the same fact in
                            // the body so `postvec provider add` can print
                            // it without scraping logs.
                            let restart_needed = budget > startup_provider_budget;
                            if restart_needed {
                                log::warn!(
                                    "provider reload raised the outbound concurrency budget \
                                     ({startup_provider_budget} -> {budget}); provider models \
                                     serve now, but full provider throughput needs a \
                                     PostgreSQL restart to widen the ingress limit"
                                );
                            }
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "success": true,
                                    "data": {
                                        "path": served_path,
                                        "providers": report.providers,
                                        "models": report.models,
                                        "errors": report.errors,
                                        "restart_needed": restart_needed,
                                    }
                                })),
                            )
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/load",
            post(
                move |ConnectInfo(peer): ConnectInfo<SocketAddr>,
                      body: Result<String, StringRejection>| {
                    let engine = load_engine.clone();
                    let root = load_root.clone();
                    let allowed = load_allowed.clone();
                    let lifecycle = load_lifecycle.clone();
                    async move {
                        if let Err(refused) = admin_peer_check(peer) {
                            return refused;
                        }
                        match parse_admin_request(body) {
                            Ok(models) => {
                                admin_load(&engine, &root, &allowed, models, lifecycle).await
                            }
                            Err(refused) => refused,
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
                    let engine = unload_engine.clone();
                    let root = unload_root.clone();
                    let lifecycle = unload_lifecycle.clone();
                    async move {
                        if let Err(refused) = admin_peer_check(peer) {
                            return refused;
                        }
                        match parse_admin_request(body) {
                            Ok(models) => admin_unload(&engine, &root, models, lifecycle).await,
                            Err(refused) => refused,
                        }
                    }
                },
            ),
        )
        .layer(DefaultBodyLimit::max(ADMIN_BODY_LIMIT));

    let handle = runtime.spawn(async move {
        // `with_connect_info` gives the admin handlers the peer address for
        // their loopback check (`admin_peer_check`) — a bind-time warning is
        // not enough once an operator hand-configures a non-loopback listen.
        let service = app.into_make_service_with_connect_info::<SocketAddr>();
        if let Err(e) = axum::serve(listener, service).await {
            // Engine-runtime thread: log-crate only (never pgrx elog here).
            log::error!("postvec embedded /config server exited: {e}");
        }
    });
    Ok((handle, bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::discovery;
    use serde_json::json;
    use std::time::Duration;

    /// `spawn` with an empty gateway and a (nonexistent) providers.d under
    /// the engine root — the zero-config shape most tests want.
    fn spawn_empty_gateway(
        engine: Arc<InferenceEngine>,
        runtime: &tokio::runtime::Runtime,
        root: &Path,
    ) -> (tokio::task::JoinHandle<()>, SocketAddr) {
        spawn(
            engine,
            runtime,
            "127.0.0.1:0",
            root,
            Vec::new(),
            ProviderState {
                gateway: Arc::new(Gateway::empty()),
                providers_path: root.join("providers.d"),
                startup_budget: 0,
            },
        )
        .unwrap()
    }

    /// Write a 0600 provider file into `<root>/providers.d`.
    fn plant_provider(root: &Path, file: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join("providers.d");
        std::fs::create_dir_all(&dir).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // …and its parent: the loader refuses a providers.d whose ancestor is
        // group-writable, and `tempfile`/`create_dir_all` honour the umask.
        if let Some(parent) = &dir.parent() {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.join(file);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        dir
    }

    const OPENAI_PROVIDER_TOML: &str = r#"
provider = "openai"
api_key = "sk-test"

[[models]]
name = "openai-text-embedding-3-small"
provider_model_id = "text-embedding-3-small"
dim = 1536
max_tokens = 8191

[[models]]
name = "baai-bge-m3"
provider_model_id = "collides-with-local"
dim = 999
"#;

    fn cfg(name: &str, enabled: bool, params: &[(&str, Value)]) -> ModelConfiguration {
        let mut cfg = ModelConfiguration {
            name: name.to_string(),
            enabled,
            ..Default::default()
        };
        for (k, v) in params {
            cfg.params.insert((*k).to_string(), v.clone());
        }
        cfg
    }

    /// The envelope must round-trip through postvec's own discovery parser —
    /// that parser is the wire contract with real inference nodes, so this
    /// pins the embedded /config to the exact same shape (model_type
    /// inference, dims, and the parser-side enabled filter included).
    #[test]
    fn config_envelope_round_trips_through_the_discovery_parser() {
        let configs = vec![
            cfg(
                "baai-bge-m3",
                true,
                &[
                    ("model_type", json!("embed")),
                    ("target_model", json!("baai-bge-m3")),
                    ("target_dim", json!(1024)),
                    ("sequence_len", json!(8192)),
                ],
            ),
            cfg(
                "convert-bge-to-cohere",
                true,
                &[
                    ("source_model", json!("baai-bge-m3")),
                    ("target_model", json!("cohere-embed-v4.0")),
                    ("source_dim", json!(1024)),
                    ("target_dim", json!(1536)),
                ],
            ),
            cfg(
                "embed-bridge",
                true,
                &[("model_type", json!("embed-bridge"))],
            ),
            cfg("ghost", false, &[("model_type", json!("embed"))]),
        ];
        let body = config_envelope(&configs, &Gateway::empty()).to_string();
        let models = discovery::parse_config(&body).expect("parses like a nin /config");

        assert_eq!(models.len(), 3, "the parser filters the disabled model");
        let embed = models.iter().find(|m| m.name == "baai-bge-m3").unwrap();
        assert_eq!(embed.model_type, "embed");
        assert_eq!(embed.target_dim, Some(1024));
        assert_eq!(embed.sequence_len, Some(8192));
        let convert = models
            .iter()
            .find(|m| m.name == "convert-bge-to-cohere")
            .unwrap();
        assert_eq!(convert.model_type, "convert", "inferred from source+target");
        assert_eq!(convert.source_model.as_deref(), Some("baai-bge-m3"));
        assert_eq!(convert.target_dim, Some(1536));
        let bridge = models.iter().find(|m| m.name == "embed-bridge").unwrap();
        assert_eq!(bridge.model_type, "embed-bridge");
    }

    /// Provider entries merge into the envelope in the nested HubModel
    /// shape and round-trip through the production discovery parser. A
    /// flat object would be silently dropped there. A name collision with
    /// a local model resolves local-wins.
    #[test]
    fn provider_models_merge_into_the_envelope_and_local_wins() {
        let root = super::super::tests::empty_engine_root();
        let dir = plant_provider(&root, "openai.toml", OPENAI_PROVIDER_TOML);
        let gateway = Gateway::load(&dir, &Default::default());

        let configs = vec![cfg(
            "baai-bge-m3",
            true,
            &[
                ("model_type", json!("embed")),
                ("target_model", json!("baai-bge-m3")),
                ("target_dim", json!(1024)),
            ],
        )];
        let body = config_envelope(&configs, &gateway).to_string();
        let models = discovery::parse_config(&body).expect("parses like a nin /config");

        // The collision resolves local-wins: one row, the engine's dim.
        let local: Vec<_> = models.iter().filter(|m| m.name == "baai-bge-m3").collect();
        assert_eq!(local.len(), 1, "local model wins the name collision");
        assert_eq!(local[0].target_dim, Some(1024));

        // The provider model is a plain embed row the resolver can use
        // unchanged, with the provider recorded under raw.extra for the
        // enable()/adopt() NOTICE (raw->'extra'->>'provider').
        let provider = models
            .iter()
            .find(|m| m.name == "openai-text-embedding-3-small")
            .expect("provider model served");
        assert_eq!(provider.model_type, "embed");
        assert_eq!(
            provider.target_model.as_deref(),
            Some("openai-text-embedding-3-small")
        );
        assert_eq!(provider.target_dim, Some(1536));
        assert_eq!(provider.sequence_len, Some(8191));
        assert_eq!(provider.raw["extra"]["provider"], json!("openai"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// POST /admin/providers/reload picks up a providers.d change without a
    /// restart: the /config the discovery client sees gains the new model.
    #[test]
    fn admin_providers_reload_updates_the_served_config() {
        let root = super::super::tests::empty_engine_root();
        let runtime = super::super::tests::engine_runtime();
        let engine = super::super::tests::test_engine(&root);
        // Start with an empty (nonexistent) providers.d.
        let providers_dir = root.join("providers.d");
        let gateway = Arc::new(Gateway::load(&providers_dir, &Default::default()));
        let (server, addr) = spawn(
            engine,
            &runtime,
            "127.0.0.1:0",
            &root,
            Vec::new(),
            ProviderState {
                gateway,
                providers_path: providers_dir.clone(),
                startup_budget: 0,
            },
        )
        .unwrap();

        let endpoint = format!("http://{addr}");
        let before = crate::runtime::block_on(discovery::fetch_models_report(
            std::slice::from_ref(&endpoint),
            Duration::from_secs(5),
        ))
        .unwrap();
        assert!(before.models.is_empty(), "zero-config serves nothing");

        plant_provider(&root, "openai.toml", OPENAI_PROVIDER_TOML);
        let (status, body) = post_json(addr, "/admin/providers/reload", json!({}));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["success"], json!(true));
        assert_eq!(body["data"]["models"], json!(2));
        // The spawn-time budget was 0 and the reload raised it: the body
        // says a restart is needed for full provider throughput (the CLI
        // prints this after `provider add`).
        assert_eq!(body["data"]["restart_needed"], json!(true));
        // Which directory this host reads. `postvec provider … --path DIR`
        // tries loopback listeners blind, so without this it could credit
        // an unrelated host with applying a change it never saw.
        assert_eq!(
            body["data"]["path"],
            json!(providers_dir.display().to_string())
        );

        let after = crate::runtime::block_on(discovery::fetch_models_report(
            std::slice::from_ref(&endpoint),
            Duration::from_secs(5),
        ))
        .unwrap();
        assert!(
            after
                .models
                .iter()
                .any(|m| m.name == "openai-text-embedding-3-small"),
            "reload serves the new provider model"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End to end over loopback HTTP: the production discovery client fetches
    /// a (model-less) engine host's /config and reports a complete refresh —
    /// exactly the path per-DB workers and refresh_models() use.
    #[test]
    fn discovery_client_fetches_the_embedded_config_listener() {
        let root = super::super::tests::empty_engine_root();
        let runtime = super::super::tests::engine_runtime();
        let engine = super::super::tests::test_engine(&root);

        let (server, addr) = spawn_empty_gateway(engine, &runtime, &root);
        let endpoint = format!("http://{addr}");

        let report = crate::runtime::block_on(discovery::fetch_models_report(
            std::slice::from_ref(&endpoint),
            Duration::from_secs(5),
        ))
        .expect("complete fetch from the embedded listener");
        assert!(report.complete, "one answering node = complete refresh");
        assert_eq!((report.ok_nodes, report.failed_nodes), (1, 0));
        assert!(report.models.is_empty(), "model-less engine, empty list");

        // Any other path is a 404 — which discovery classifies as a node
        // (transport) problem, not garbage JSON.
        let res = crate::runtime::block_on(discovery::fetch_models_report(
            &[format!("http://{addr}/nope")],
            Duration::from_secs(5),
        ));
        assert!(
            matches!(res, Err(crate::client::PvError::Transport { .. })),
            "got {res:?}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- /admin/load and /admin/unload ---------------------------------

    /// A running (model-less) listener over a fresh engine root, plus the
    /// root path for descriptor fixtures. Callers abort the handle and
    /// remove the root.
    fn spawn_admin_server() -> (
        std::path::PathBuf,
        tokio::runtime::Runtime,
        tokio::task::JoinHandle<()>,
        SocketAddr,
    ) {
        let root = super::super::tests::empty_engine_root();
        let runtime = super::super::tests::engine_runtime();
        let engine = super::super::tests::test_engine(&root);
        let (server, addr) = spawn_empty_gateway(engine, &runtime, &root);
        (root, runtime, server, addr)
    }

    /// Round 8: with an explicit `postvec.embedded_models` list, the admin
    /// load route is bound by it — the eligibility policy applies to every
    /// load path, not only the startup preload.
    #[test]
    fn admin_load_enforces_the_allow_list() {
        let root = super::super::tests::empty_engine_root();
        let runtime = super::super::tests::engine_runtime();
        let engine = super::super::tests::test_engine(&root);
        let (server, addr) = spawn(
            engine,
            &runtime,
            "127.0.0.1:0",
            &root,
            vec!["permitted-model".to_string()],
            ProviderState {
                gateway: Arc::new(Gateway::empty()),
                providers_path: root.join("providers.d"),
                startup_budget: 0,
            },
        )
        .unwrap();

        let (status, body) = post_json(
            addr,
            "/admin/load",
            serde_json::json!({"models": ["other-model"]}),
        );
        assert_eq!(status, 200, "refusals are envelope-shaped: {body}");
        let detail = body["data"]["results"][0]["error"]
            .as_str()
            .unwrap_or_default();
        assert!(
            detail.contains("embedded_models"),
            "refusal must point at the allow-list, got: {body}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// POST a JSON body and decode the JSON answer (every admin response,
    /// success or refusal, is envelope-shaped).
    fn post_json(addr: SocketAddr, path: &str, body: Value) -> (u16, Value) {
        crate::runtime::block_on(async move {
            let response = reqwest::Client::new()
                .post(format!("http://{addr}{path}"))
                .json(&body)
                .send()
                .await
                .expect("admin request");
            let status = response.status().as_u16();
            let body: Value = response.json().await.expect("JSON envelope");
            (status, body)
        })
    }

    fn results(body: &Value) -> &Vec<Value> {
        body["data"]["results"]
            .as_array()
            .expect("data.results array")
    }

    fn plant_generic(root: &Path, name: &str) {
        let dir = root.join("models").join("generic").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ninference.hub.json"),
            serde_json::json!({
                "name": name,
                "backend": "generic",
                "enabled": true,
                "executor": { "key": "dummy" }
            })
            .to_string(),
        )
        .unwrap();
    }

    /// The resident check and load are one serialized lifecycle operation.
    /// With 15 models resident, two simultaneous requests may load exactly
    /// one more — never both based on the same stale count.
    #[test]
    fn concurrent_admin_loads_cannot_race_the_resident_ceiling() {
        let root = super::super::tests::empty_engine_root();
        let runtime = super::super::tests::engine_runtime();
        let engine = super::super::tests::test_engine(&root);
        for i in 0..=16 {
            plant_generic(&root, &format!("m{i}"));
        }
        runtime.block_on(async {
            for i in 0..15 {
                engine.load_model(&format!("m{i}")).await.unwrap();
            }
        });
        assert_eq!(engine.get_active_models().len(), 15);

        let (server, addr) = spawn_empty_gateway(engine.clone(), &runtime, &root);
        let (left, right) = runtime.block_on(async {
            let client = reqwest::Client::new();
            tokio::join!(
                client
                    .post(format!("http://{addr}/admin/load"))
                    .json(&json!({"models": ["m15"]}))
                    .send(),
                client
                    .post(format!("http://{addr}/admin/load"))
                    .json(&json!({"models": ["m16"]}))
                    .send()
            )
        });
        let bodies = runtime.block_on(async {
            (
                left.unwrap().json::<Value>().await.unwrap(),
                right.unwrap().json::<Value>().await.unwrap(),
            )
        });
        let statuses = [
            bodies.0["data"]["results"][0]["status"].as_str().unwrap(),
            bodies.1["data"]["results"][0]["status"].as_str().unwrap(),
        ];
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == "loaded")
                .count(),
            1
        );
        assert_eq!(engine.get_active_models().len(), MAX_RESIDENT_MODELS);

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A model absent from disk is a per-model "error" outcome riding a 200
    /// — the request itself was well-formed.
    #[test]
    fn admin_load_reports_a_missing_model_per_model() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        let (status, body) = post_json(addr, "/admin/load", json!({ "models": ["no-such-model"] }));
        assert_eq!(status, 200);
        assert_eq!(body["success"], json!(true));
        let results = results(&body);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["model"], "no-such-model");
        assert_eq!(results[0]["status"], "error");
        let error = results[0]["error"].as_str().unwrap();
        assert!(error.contains("not found"), "error: {error}");

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `enabled` admission check: a disabled on-disk descriptor is
    /// refused by the handler — `load_model` itself would happily run it.
    #[test]
    fn admin_load_refuses_a_disabled_descriptor() {
        let (root, _runtime, server, addr) = spawn_admin_server();
        let model_dir = root.join("models").join("onnx-runtime").join("m");
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            model_dir.join("ninference.hub.json"),
            r#"{ "name": "m", "enabled": false }"#,
        )
        .unwrap();

        let (status, body) = post_json(addr, "/admin/load", json!({ "models": ["m"] }));
        assert_eq!(status, 200);
        let results = results(&body);
        assert_eq!(results[0]["status"], "error");
        let error = results[0]["error"].as_str().unwrap();
        assert!(error.contains("disabled"), "error: {error}");

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Path-like, malformed and oversized names never reach the filesystem:
    /// the whole request is a 400 refusal.
    #[test]
    fn admin_load_refuses_invalid_model_names() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        for bad in ["../x", "A B", &"a".repeat(200), ""] {
            let (status, body) = post_json(addr, "/admin/load", json!({ "models": [bad] }));
            assert_eq!(status, 400, "name {bad:?}");
            assert_eq!(body["success"], json!(false), "name {bad:?}");
            assert!(body["error"].is_string(), "name {bad:?}");
        }

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// More than [`ADMIN_MAX_MODELS`] names is a request-level 400.
    #[test]
    fn admin_requests_are_bounded_in_model_count() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        let names: Vec<String> = (0..=ADMIN_MAX_MODELS).map(|i| format!("m{i}")).collect();
        let (status, body) = post_json(addr, "/admin/load", json!({ "models": names }));
        assert_eq!(status, 400);
        assert_eq!(body["success"], json!(false));

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The body cap answers in the same JSON envelope, not axum's plain-text
    /// rejection page — the CLI decodes the body even on an error status.
    #[test]
    fn admin_oversized_bodies_are_refused_as_json() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        let (status, body) = post_json(
            addr,
            "/admin/load",
            json!({ "models": [], "padding": "x".repeat(ADMIN_BODY_LIMIT + 1) }),
        );
        assert_eq!(status, 413);
        assert_eq!(body["success"], json!(false));

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Wrong methods never reach a handler.
    #[test]
    fn admin_and_config_reject_wrong_methods() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        let (get_admin, post_config) = crate::runtime::block_on(async move {
            let client = reqwest::Client::new();
            let get_admin = client
                .get(format!("http://{addr}/admin/load"))
                .send()
                .await
                .expect("GET /admin/load")
                .status()
                .as_u16();
            let post_config = client
                .post(format!("http://{addr}/config"))
                .send()
                .await
                .expect("POST /config")
                .status()
                .as_u16();
            (get_admin, post_config)
        });
        assert_eq!(get_admin, 405);
        assert_eq!(post_config, 405);

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Unloading something that was never loaded is a per-model outcome,
    /// not an error.
    #[test]
    fn admin_unload_reports_not_loaded() {
        let (root, _runtime, server, addr) = spawn_admin_server();

        let (status, body) = post_json(addr, "/admin/unload", json!({ "models": ["ghost"] }));
        assert_eq!(status, 200);
        assert_eq!(body["success"], json!(true));
        let results = results(&body);
        assert_eq!(results[0]["model"], "ghost");
        assert_eq!(results[0]["status"], "not-loaded");

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Within one request a dependent unloads before its dependency —
    /// transitively, with duplicates collapsed; models without ordering
    /// information keep their request order.
    #[test]
    fn unload_order_puts_dependents_before_dependencies() {
        let deps = HashMap::from([
            ("a".to_string(), vec!["b".to_string()]),
            ("b".to_string(), vec!["c".to_string()]),
        ]);
        let models: Vec<String> = ["c", "b", "a", "c", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            reverse_dependency_order(&models, &deps),
            vec!["a", "b", "c", "x"]
        );

        // No descriptors at all: request order, deduplicated.
        assert_eq!(
            reverse_dependency_order(&models, &HashMap::new()),
            vec!["c", "b", "a", "x"]
        );
    }
}
