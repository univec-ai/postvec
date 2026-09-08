//! The UniVec `engine` crate hosted in-process inside the postvec
//! launcher.
//!
//! Both constraints are load-bearing:
//!
//! - **Only the launcher process owns an engine.** Everything else —
//!   connection backends *and* the per-database workers — are thin gRPC
//!   clients pointed at the loopback servers this module spawns ([`server`]
//!   for inference, [`http`] for `/config` model discovery and the
//!   `/admin/load`/`/admin/unload` controls), so every
//!   caller is byte-identical to the remote deployment shape. Nothing calls
//!   the engine directly; there is no in-process client.
//! - **The engine runs on its own dedicated multi-thread tokio runtime**,
//!   never on postvec's current-thread runtime: the engine's bridge
//!   executors re-enter async via `tokio::task::block_in_place`, which
//!   panics on a current-thread runtime. Engine threads are pure compute +
//!   file I/O — none of them ever touches Postgres state, so the pgrx
//!   "only the main thread may call into Postgres" rule holds as long as
//!   SPI stays on worker main threads (this module never does SPI).
//!
//! Failure containment (the reason this module is shaped defensively):
//! an engine that fails to build must degrade the launcher, not crash it —
//! a panic here would put it into its 5-second crash-respawn loop, and a
//! crash in a `shared_preload_libraries` bgworker is a postmaster-visible
//! event. So [`maybe_init`] returns errors as values and retries on a fixed
//! backoff; workers keep queueing (gated on listener reachability) until
//! the engine comes up. Panics inside predictions are contained by the
//! engine (`spawn_blocking` join errors surface as `EngineError`). What
//! cannot be contained is a native fault (segfault/OOM-kill) inside ONNX
//! Runtime — that is inherent to hosting inference in-process and is why
//! embedded mode is opt-in.
//!
//! IMPORTANT: code that runs on engine-runtime threads (the servers) must
//! never call pgrx logging (`log!`/`warning!` are `elog` — Postgres state).
//! Use the `log` crate (bridged to stderr by [`init_stderr_logger`]) inside
//! those paths; pgrx logging is fine in the init/shutdown paths, which run
//! on the launcher main thread.

use engine::InferenceEngine;
use once_cell::sync::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod http;
mod server;

/// Worker threads for the engine runtime. Deliberately small: the runtime
/// only shepherds async plumbing (timeouts, the loopback server, pool
/// checkout) — the heavy compute runs on `spawn_blocking` threads and ORT's
/// own intra-op pools, both bounded elsewhere (deadpool `pool_size` per
/// model, ORT session config). Two threads (not one) so `block_in_place` in
/// the bridge executors always has a peer to hand its worker duties to.
const ENGINE_RUNTIME_THREADS: usize = 2;

/// How long to wait between failed engine-init attempts.
const INIT_RETRY: Duration = Duration::from_secs(30);

struct EmbeddedState {
    /// Never read, deliberately owned: dropping the runtime would tear down
    /// the loopback servers and every in-flight prediction.
    _runtime: tokio::runtime::Runtime,
    /// Never read after init (the servers hold their own `Arc` clones); kept
    /// so the engine's lifetime is anchored here rather than implied.
    _engine: Arc<InferenceEngine>,
    server: tokio::task::JoinHandle<()>,
    http_server: tokio::task::JoinHandle<()>,
}

/// The one engine per worker process. Never replaced once set: models,
/// listener, and the ORT global environment cannot be hot-swapped (which is
/// also why the embedded GUCs are POSTMASTER context).
static STATE: OnceCell<EmbeddedState> = OnceCell::new();

/// `initialize_onnx` commits a process-global ORT environment and has no
/// internal guard — this cell makes "exactly once per process" structural.
static ONNX_READY: OnceCell<()> = OnceCell::new();

static LAST_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);

pub fn ready() -> bool {
    STATE.get().is_some()
}

/// Resident-model cap: every loaded model costs session
/// memory plus an intra-op thread pool INSIDE the PostgreSQL process
/// family. Shared by the startup scan and the `/admin/load` route — the
/// only two load paths since inference-request JIT loading was removed —
/// so the ceiling holds for the process lifetime.
pub(super) const MAX_SCAN_LOADED_MODELS: usize = 16;

/// Everything an init attempt needs, resolved from GUCs/env up front so a
/// misconfiguration is one precise error instead of a partial build.
struct EmbeddedConfig {
    root: PathBuf,
    /// Explicit preload list; empty = scan-load everything enabled on disk.
    models: Vec<String>,
    listen: String,
    http_listen: String,
    /// `postvec.providers_path` — the providers.d directory the gateway
    /// loads. A path, never a credential.
    providers_path: PathBuf,
    predict_timeout: Duration,
    /// `postvec.embedded_max_inflight`: sizes both the engine admission
    /// gate and the loopback server's global ingress limit. The ingress
    /// memory envelope scales with this, so it is one knob.
    max_inflight: usize,
}

fn config_from_gucs() -> Result<EmbeddedConfig, String> {
    let root = crate::gucs::ENGINE_PATH
        .get()
        .map(|c| c.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "no engine root: set postvec.path".to_string())?;
    let root = PathBuf::from(root);
    if !root.is_dir() {
        return Err(format!(
            "engine root {} does not exist or is not a directory",
            root.display()
        ));
    }
    Ok(EmbeddedConfig {
        root,
        models: crate::gucs::parse_endpoint_list(crate::gucs::EMBEDDED_MODELS.get()),
        listen: crate::gucs::embedded_listen(),
        http_listen: crate::gucs::embedded_http_listen(),
        providers_path: PathBuf::from(crate::gucs::providers_path()),
        predict_timeout: Duration::from_millis(crate::gucs::EMBED_TIMEOUT_MS.get().max(100) as u64),
        max_inflight: crate::gucs::EMBEDDED_MAX_INFLIGHT.get().clamp(1, 16) as usize,
    })
}

/// Rate-limited engine init. Called from the launcher main thread each
/// wake while not ready. Returns:
/// - `Ok(true)`  — engine is up (was already, or came up in this call);
/// - `Ok(false)` — not ready and the retry backoff has not elapsed;
/// - `Err(msg)`  — an attempt ran and failed (the launcher logs it; the
///   next attempt happens after [`INIT_RETRY`]).
pub fn maybe_init() -> Result<bool, String> {
    if ready() {
        return Ok(true);
    }
    {
        // Recover a poisoned guard instead of panicking every retry forever:
        // the critical section only reads/writes an Instant.
        let mut last = LAST_ATTEMPT.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(at) = *last {
            if at.elapsed() < INIT_RETRY {
                return Ok(false);
            }
        }
        *last = Some(Instant::now());
    }
    try_init()?;
    Ok(true)
}

#[derive(serde::Deserialize)]
struct ResidentDescriptor {
    name: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// Index every addressable descriptor by its model-directory name. This is
/// deliberately stricter than the engine's legacy first-match scan: duplicate
/// names across backends are not a deterministic resource plan.
fn descriptor_index(root: &std::path::Path) -> Result<BTreeMap<String, Vec<PathBuf>>, String> {
    let models_dir = root.join("models");
    let backends = std::fs::read_dir(&models_dir)
        .map_err(|e| format!("cannot scan {}: {e}", models_dir.display()))?;
    let mut index: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for backend in backends {
        let backend = backend.map_err(|e| format!("scanning {}: {e}", models_dir.display()))?;
        let backend_path = backend.path();
        if !backend_path.is_dir()
            || backend
                .file_name()
                .to_str()
                .is_none_or(|name| name.starts_with('.'))
        {
            continue;
        }
        for model in std::fs::read_dir(&backend_path)
            .map_err(|e| format!("cannot scan {}: {e}", backend_path.display()))?
        {
            let model = model.map_err(|e| format!("scanning {}: {e}", backend_path.display()))?;
            let model_path = model.path();
            let Some(name) = model.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !model_path.is_dir() || name.starts_with('.') {
                continue;
            }
            let descriptor = model_path.join("ninference.hub.json");
            if descriptor.is_file() {
                index.entry(name).or_default().push(descriptor);
            }
        }
    }
    Ok(index)
}

fn read_resident_descriptor(path: &std::path::Path) -> Result<ResidentDescriptor, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", path.display()))
}

/// The result of walking one dependency closure.
///
/// `disabled` is recorded rather than raised, because the two callers need
/// opposite answers from the same walk. The **startup** preflight is a
/// resource ceiling: a deactivated dependency is a fact to note, not a reason
/// to abort the whole engine for every other model. The **`/admin/load`**
/// route refuses instead, so the operator gets a precise message before
/// anything is attempted.
///
/// Neither is the actual enforcement any more: `InferenceEngine::load_model`
/// refuses a disabled config itself, which is what makes the switch mean the
/// same thing on every load path (the startup executor build's JIT dependency
/// loads included). This walk is a better error in front of that gate.
#[derive(Debug, Default)]
pub(super) struct ResidentClosure {
    /// Every model the closure would make resident, roots included.
    pub planned: BTreeSet<String>,
    /// Members whose descriptor says `enabled: false`.
    pub disabled: BTreeSet<String>,
}

fn visit_resident_model(
    index: &BTreeMap<String, Vec<PathBuf>>,
    name: &str,
    depth: usize,
    is_root: bool,
    visiting: &mut BTreeSet<String>,
    closure: &mut ResidentClosure,
) -> Result<(), String> {
    if closure.planned.contains(name) {
        return Ok(());
    }
    if depth > 32 {
        return Err(format!("dependency chain under {name:?} exceeds depth 32"));
    }
    if !visiting.insert(name.to_string()) {
        return Err(format!("dependency cycle reaches model {name:?}"));
    }
    if visiting.len() + closure.planned.len() > MAX_SCAN_LOADED_MODELS {
        return Err(format!(
            "embedded model roots plus dependencies exceed the resident ceiling of \
             {MAX_SCAN_LOADED_MODELS} (first overflow at {name:?}); reduce \
             postvec.embedded_models or unload model assets"
        ));
    }
    let paths = index
        .get(name)
        .ok_or_else(|| format!("model {name:?} has no descriptor under models/<backend>/"))?;
    if paths.len() != 1 {
        return Err(format!(
            "model {name:?} exists under multiple backends; remove the duplicate"
        ));
    }
    let descriptor = read_resident_descriptor(&paths[0])?;
    if descriptor.name != name {
        return Err(format!(
            "{} names model {:?}, but its directory is {name:?}",
            paths[0].display(),
            descriptor.name
        ));
    }
    if is_root && !descriptor.enabled {
        return Err(format!(
            "explicit embedded model {name:?} is disabled in {}; run `postvec model activate \
             {name}` (or drop it from postvec.embedded_models)",
            paths[0].display()
        ));
    }
    if !descriptor.enabled {
        closure.disabled.insert(name.to_string());
    }
    for dependency in &descriptor.dependencies {
        visit_resident_model(index, dependency, depth + 1, false, visiting, closure)?;
    }
    visiting.remove(name);
    closure.planned.insert(name.to_string());
    Ok(())
}

/// Exact startup resident-set preflight, including every transitive
/// dependency that `load_model`/executor build can pull in. The previous
/// enabled-descriptor count omitted disabled dependency descriptors, so 16
/// enabled bridge roots could still create far more than 16 pools/sessions.
/// Every model name the *engine* owns: what is loaded now, plus every
/// descriptor on disk.
///
/// Deliberately wider than `get_active_models()`. A model that is configured
/// but currently failing to load is not in the engine's map, so reserving
/// only loaded names would let a provider file declaring that same name take
/// it over — and a column bound to it would start sending its source text to
/// a third party because a local model was broken. The reservation has to
/// cover intent, not just current state.
///
/// A descriptor scan that fails is an **error**, not a smaller reservation.
/// Falling back to the loaded set narrows the very list that decides whether
/// a provider may claim a name, so a transient scan failure during reload
/// could let a provider take over a configured-but-unloaded local model and
/// start sending that column's text to a third party. Fail closed: the
/// caller keeps the previous gateway snapshot (reload) or serves no providers
/// (boot). Local models are unaffected either way.
pub(crate) fn reserved_local_names(
    root: &std::path::Path,
    engine: &InferenceEngine,
) -> Result<BTreeSet<String>, String> {
    let mut names: BTreeSet<String> = engine.get_active_models().into_iter().collect();
    names.extend(descriptor_index(root)?.into_keys());
    Ok(names)
}

fn planned_resident_models(
    root: &std::path::Path,
    explicit: &[String],
) -> Result<BTreeSet<String>, String> {
    let index = descriptor_index(root)?;
    planned_resident_models_indexed(&index, explicit)
}

/// [`planned_resident_models`] over a prebuilt descriptor index. The admin
/// load route builds the index once per request (on a blocking thread) and
/// reuses it per requested name, instead of rescanning the whole model root
/// per name on the 2-thread engine runtime.
fn planned_resident_models_indexed(
    index: &BTreeMap<String, Vec<PathBuf>>,
    explicit: &[String],
) -> Result<BTreeSet<String>, String> {
    resident_closure(index, explicit).map(|closure| closure.planned)
}

/// The closure walk both callers share.
///
/// Disabled dependencies are **counted** here on purpose: they cost nothing at
/// startup (the scan loader skips them) but the ceiling stays conservative, and
/// [`admin_load_closure`] — the path that would really load them — turns the
/// same information into a refusal.
fn resident_closure(
    index: &BTreeMap<String, Vec<PathBuf>>,
    explicit: &[String],
) -> Result<ResidentClosure, String> {
    let mut roots = Vec::new();
    if explicit.is_empty() {
        for (name, paths) in index {
            // Broken descriptors are skipped by the engine's startup scan and
            // consume no resident resources. An enabled duplicate is different:
            // the engine's read_dir-order winner is nondeterministic, so fail.
            let enabled = paths
                .iter()
                .filter_map(|path| read_resident_descriptor(path).ok())
                .any(|descriptor| descriptor.enabled);
            if enabled && paths.len() != 1 {
                return Err(format!(
                    "enabled model {name:?} exists under multiple backends; remove the duplicate"
                ));
            }
            if enabled {
                roots.push(name.clone());
            }
        }
    } else {
        roots.extend(explicit.iter().cloned());
    }

    let mut closure = ResidentClosure::default();
    let mut visiting = BTreeSet::new();
    for root in roots {
        visit_resident_model(index, &root, 0, true, &mut visiting, &mut closure)?;
    }
    Ok(closure)
}

/// The `/admin/load` closure: the resident set this load would create, refused
/// outright when any member is deactivated.
///
/// `InferenceEngine::load_model` refuses a disabled config, so the load would
/// fail on the dependency anyway; catching it here names the deactivated model
/// and `postvec model activate` instead of surfacing a mid-closure engine
/// error. The refusal also matches what a restart produces: the same
/// `load_model` gate applies to the startup executor build's JIT dependency
/// loads, so a deactivated dependency is not resident there either.
pub(super) fn admin_load_closure(
    index: &BTreeMap<String, Vec<PathBuf>>,
    name: &str,
) -> Result<BTreeSet<String>, String> {
    let closure = resident_closure(index, &[name.to_string()])?;
    if let Some(first) = closure.disabled.iter().next() {
        let names: Vec<&str> = closure.disabled.iter().map(String::as_str).collect();
        return Err(format!(
            "model {name:?} depends on deactivated model(s) [{}]; the engine will not load a \
             deactivated descriptor, so this load would not survive a restart — run `postvec \
             model activate {first}` (or activate {name:?}, which enables its dependency \
             closure first)",
            names.join(", ")
        ));
    }
    Ok(closure.planned)
}

fn try_init() -> Result<(), String> {
    let cfg = config_from_gucs()?;
    init_stderr_logger();

    // Resource policy is proved before creating even one native session.
    // This counts transitive dependencies for both scan and explicit modes;
    // checking after load would be too late if the excess itself OOMs the
    // PostgreSQL process family.
    let planned_models = planned_resident_models(&cfg.root, &cfg.models)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(ENGINE_RUNTIME_THREADS)
        // 8 MiB stacks. tokio's 2 MiB default is too small: tokenizer
        // regex recursion runs on these and on the spawn_blocking threads,
        // which inherit this size.
        .thread_stack_size(8 * 1024 * 1024)
        // Bound the blocking pool (tokio's default is 512 threads): heavy
        // work is per-predict spawn_blocking, already gated per model by
        // deadpool — excess demand should queue (and hit the predict
        // timeout), not fan out into hundreds of 8 MiB stacks.
        .max_blocking_threads(32)
        .thread_name("postvec-engine")
        .enable_all()
        .build()
        .map_err(|e| format!("engine runtime: {e}"))?;

    // Process-global ORT environment (dlopen of libonnxruntime from
    // <root>/libs). No engine guard of its own; guarded here. A failed call
    // commits nothing, so retrying on the next attempt is safe.
    if ONNX_READY.get().is_none() {
        engine::initialize_onnx(&cfg.root).map_err(|e| format!("initialize_onnx: {e}"))?;
        let _ = ONNX_READY.set(());
    }

    // Database-host resource policy. Unset, onnxruntime
    // builds one intra-op pool of ~physical-core threads *per session*, with
    // spin-wait enabled, and requests for different models can all enter
    // blocking execution at once — an embedded engine competing with
    // PostgreSQL for every core, and burning a post-inference spin tail that
    // reads as "CPU busy while idle". Embedded mode therefore bounds each
    // session to a small intra-op pool, disables spinning, and admits
    // `postvec.embedded_max_inflight` (default 1) predictions engine-wide;
    // the permit is held until the native computation actually finishes, so
    // a timed-out caller cannot oversubscribe the gate. Standalone
    // Standalone inference nodes are untouched — this policy exists only in this
    // process.
    engine::set_session_thread_policy(engine::SessionThreadPolicy {
        intra_op_threads: (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            / 2)
        .clamp(1, 4),
        disable_spinning: true,
    });
    let engine_cfg = Arc::new(engine::EngineConfig {
        root_path: cfg.root.clone(),
        host_policy: engine::HostPolicy {
            admission_limit: Some(cfg.max_inflight),
            // Serialize native model instantiation behind the load-commit
            // gate: on the database host a cancelled load must not admit a
            // second native instantiation while the first still runs.
            serialized_model_loads: true,
        },
    });

    // Engine build runs `block_on` the engine runtime so `spawn_blocking`
    // inside `load_model` lands on that runtime, exactly as in nin/udb.
    // `hub: None` everywhere — the DB host carries its model assets on disk;
    // there is no S3 JIT download in embedded mode.
    let engine = if cfg.models.is_empty() {
        // Scan mode: mirror the standalone server's boot (load_models_and_executors +
        // build_executors). Per-model failures are logged and skipped by the
        // engine; only structural failures (unreadable models dir) error.
        //
        let engine = InferenceEngine::new(engine_cfg);
        runtime.block_on(async {
            engine
                .load_models_and_executors()
                .map_err(|e| format!("load_models_and_executors: {e}"))?;
            engine
                .build_executors()
                .await
                .map_err(|e| format!("build_executors: {e}"))
        })?;
        Arc::new(engine)
    } else {
        Arc::new(
            runtime
                .block_on(InferenceEngine::default_with_models(
                    engine_cfg,
                    &cfg.models,
                ))
                .map_err(|e| format!("model preload: {e}"))?,
        )
    };

    let active = engine.get_active_models();
    if active.len() > MAX_SCAN_LOADED_MODELS || active.len() > planned_models.len() {
        return Err(format!(
            "embedded engine loaded {} resident models although startup preflight allowed {} \
             (hard ceiling {MAX_SCAN_LOADED_MODELS}); refusing to publish the engine",
            active.len(),
            planned_models.len()
        ));
    }
    if active.is_empty() {
        // Not fatal — but say so loudly, because nothing will embed until
        // models exist under <root>/models/<backend>/<name>/.
        pgrx::warning!(
            "postvec: embedded engine started with zero models (root {}); \
             set postvec.embedded_models or add models under models/",
            cfg.root.display()
        );
    }

    // One gateway, shared by both loopback servers. `load` isolates
    // per-file failures (a broken provider file must never degrade local
    // models). A missing directory is the ordinary zero-config case: an
    // empty gateway changes nothing, including the ingress bound.
    // Names the engine already owns are reserved so a provider file
    // claiming one is refused at load. If that list cannot be built, no
    // provider serves: a partial reservation is how a provider would
    // steal a local name.
    let gateway = Arc::new(match reserved_local_names(&cfg.root, &engine) {
        Ok(local_models) => providers::gateway::Gateway::load(
            &cfg.providers_path,
            &providers::gateway::Gateway::reserve(local_models),
        ),
        Err(e) => {
            pgrx::warning!(
                "postvec: cannot enumerate local models under {} ({e}); serving no external \
                 providers this start, because a partial list could let one claim a local \
                 model's name. Local models are unaffected",
                cfg.root.display()
            );
            providers::gateway::Gateway::empty()
        }
    });
    let server = server::spawn(
        engine.clone(),
        &runtime,
        &cfg.listen,
        cfg.predict_timeout,
        cfg.max_inflight,
        gateway.clone(),
    )
    .map_err(|e| format!("loopback gRPC server: {e}"))?;
    // If the HTTP listener fails, abort the gRPC server before erroring:
    // dropping a JoinHandle does NOT cancel the task, and a still-bound gRPC
    // listener would make every retry fail on that port forever.
    let http_server = match http::spawn(
        engine.clone(),
        &runtime,
        &cfg.http_listen,
        &cfg.root,
        cfg.models.clone(),
        http::ProviderState {
            gateway,
            providers_path: cfg.providers_path.clone(),
        },
    ) {
        Ok((handle, _)) => handle,
        Err(e) => {
            server.abort();
            return Err(format!("loopback /config server: {e}"));
        }
    };

    let n = active.len();
    let listen = cfg.listen.clone();
    let http_listen = cfg.http_listen.clone();
    STATE
        .set(EmbeddedState {
            _runtime: runtime,
            _engine: engine,
            server,
            http_server,
        })
        .map_err(|_| "embedded engine already initialized".to_string())?;

    pgrx::log!(
        "postvec: embedded engine up ({n} models, gRPC on {listen}, /config on {http_listen}). \
         Embedded inference is best-effort, not crash-isolated: it shares this PostgreSQL \
         cluster's process family, and a native onnxruntime fault or OOM kill restarts the \
         cluster. Remote mode (postvec.mode='grpc') is the fail-safe production profile."
    );
    Ok(())
}

/// Stop the loopback servers. Called on launcher exit (best effort —
/// process exit tears the runtime and ORT down regardless).
pub fn shutdown() {
    if let Some(state) = STATE.get() {
        state.server.abort();
        state.http_server.abort();
    }
}

/// Minimal `log`-crate bridge to stderr so engine logs (model loading,
/// prediction failures) land in the Postgres server log instead of being
/// dropped. stderr is safe from any thread; pgrx `elog` is not.
fn init_stderr_logger() {
    struct StderrLogger;
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.level() <= log::Level::Info
        }
        fn log(&self, record: &log::Record) {
            if self.enabled(record.metadata()) {
                eprintln!(
                    "postvec-engine [{}] {}: {}",
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }
        fn flush(&self) {}
    }
    static LOGGER: StderrLogger = StderrLogger;
    // Errors only when a logger is already installed — fine either way.
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{EmbedRoute, ErrorClass, InferenceClient, PvError, RavennaCode};

    // ---- Integration: a real (model-less) engine behind the loopback
    // server, driven through postvec's own gRPC client — validates the whole
    // plumbing (server marshalling, error metadata, client classification)
    // without any model assets.

    pub(super) fn empty_engine_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "postvec-embedded-test-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        // models/<backend>/ exists but holds no models: load_model resolves
        // to a clean NotFound instead of "models directory not found".
        std::fs::create_dir_all(root.join("models").join("onnx-runtime")).unwrap();
        root
    }

    pub(super) fn test_engine(root: &std::path::Path) -> Arc<InferenceEngine> {
        Arc::new(InferenceEngine::new(Arc::new(engine::EngineConfig {
            root_path: root.to_path_buf(),
            host_policy: Default::default(),
        })))
    }

    pub(super) fn engine_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn plant_descriptor(root: &std::path::Path, name: &str, enabled: bool, deps: &[String]) {
        let dir = root.join("models").join("generic").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ninference.hub.json"),
            serde_json::json!({
                "name": name,
                "backend": "generic",
                "enabled": enabled,
                "dependencies": deps,
                "executor": { "key": "dummy" }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn startup_resident_preflight_counts_dependencies_and_cycles() {
        let root = empty_engine_root();
        // One enabled root plus sixteen dependencies. The ceiling counts the
        // whole closure, not the enabled descriptors: the request-visible name
        // alone would see one where the engine would create seventeen pools.
        let dependencies: Vec<String> = (0..16).map(|i| format!("dep-{i}")).collect();
        plant_descriptor(&root, "root", true, &dependencies);
        for dependency in &dependencies {
            plant_descriptor(&root, dependency, true, &[]);
        }
        let err = planned_resident_models(&root, &[]).unwrap_err();
        assert!(err.contains("resident ceiling"), "got: {err}");

        let _ = std::fs::remove_dir_all(&root);
        let root = empty_engine_root();
        plant_descriptor(&root, "a", true, &["b".to_string()]);
        plant_descriptor(&root, "b", false, &["a".to_string()]);
        let err = planned_resident_models(&root, &[]).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A deactivated dependency must not abort startup. `load_model` refuses
    /// it per model — the enabled parent's executor build fails and that
    /// parent is dropped — so one deactivated model is never an engine-wide
    /// outage. The preflight is only a resource ceiling and still plans (and
    /// conservatively counts) the closure.
    #[test]
    fn a_deactivated_dependency_does_not_fail_the_startup_preflight() {
        let root = empty_engine_root();
        plant_descriptor(&root, "converter", true, &["embed-dep".to_string()]);
        plant_descriptor(&root, "embed-dep", false, &[]);

        let index = descriptor_index(&root).unwrap();
        let closure = resident_closure(&index, &[]).unwrap();
        assert!(closure.planned.contains("converter"));
        assert!(
            closure.planned.contains("embed-dep"),
            "the ceiling stays conservative"
        );
        assert_eq!(
            closure
                .disabled
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["embed-dep"]
        );
        // Scan mode never treats a disabled descriptor as a root.
        assert!(!resident_closure(&index, &[]).unwrap().planned.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// …but the admin load path, which is the one that would really make it
    /// resident, refuses and names the fix.
    #[test]
    fn admin_load_refuses_a_closure_containing_a_deactivated_model() {
        let root = empty_engine_root();
        plant_descriptor(&root, "converter", true, &["embed-dep".to_string()]);
        plant_descriptor(&root, "embed-dep", false, &[]);
        let index = descriptor_index(&root).unwrap();

        let err = admin_load_closure(&index, "converter").unwrap_err();
        assert!(err.contains("embed-dep"), "got: {err}");
        assert!(err.contains("postvec model activate"), "got: {err}");

        // Enabled again: the same closure loads.
        plant_descriptor(&root, "embed-dep", true, &[]);
        let index = descriptor_index(&root).unwrap();
        let planned = admin_load_closure(&index, "converter").unwrap();
        assert!(planned.contains("converter") && planned.contains("embed-dep"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An explicitly preloaded root that is deactivated still fails the
    /// startup preflight: `postvec.embedded_models` names it, so silently
    /// skipping it would change availability without saying so.
    #[test]
    fn a_deactivated_explicit_root_still_fails_the_preflight() {
        let root = empty_engine_root();
        plant_descriptor(&root, "m", false, &[]);
        let err = planned_resident_models(&root, &["m".to_string()]).unwrap_err();
        assert!(err.contains("is disabled"), "got: {err}");
        assert!(err.contains("postvec model activate m"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End to end over the loopback wire: postvec's production `GrpcClient`
    /// against the in-worker server. A missing model must cross as
    /// `NotFound` + `x-ravenna-error-code: MODEL_NOT_FOUND`, which the
    /// client classifies exactly like a remote inference-node answer.
    #[test]
    fn loopback_server_speaks_the_inference_wire_contract() {
        use crate::client::grpc::GrpcClient;

        let root = empty_engine_root();
        let runtime = engine_runtime();
        let engine = test_engine(&root);

        // Round 8: inference requests never load models, so an unknown (or
        // not-yet-activated) model refuses as MODEL_NOT_LOADED — which the
        // client classifies as Config, same as a remote node's answer.
        // Port 0: the OS picks a free port; spawn() reports it back.
        let (server, addr) = server::spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            Arc::new(providers::gateway::Gateway::empty()),
        )
        .unwrap();

        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        let err = crate::runtime::block_on(client.embed(
            &["hello".to_string()],
            "no-such-model",
            &EmbedRoute::default(),
        ))
        .unwrap_err();
        match &err {
            PvError::Remote { code, message } => {
                assert_eq!(*code, RavennaCode::ModelNotLoaded);
                assert!(message.contains("no-such-model"), "message: {message}");
            }
            other => panic!("expected Remote(ModelNotLoaded), got {other:?}"),
        }
        assert_eq!(err.class(), ErrorClass::Config);

        let err =
            crate::runtime::block_on(client.convert(&[vec![1.0f32, 2.0]], "no-such-converter"))
                .unwrap_err();
        assert!(
            matches!(&err, PvError::Remote { code, .. } if *code == RavennaCode::ModelNotLoaded),
            "got {err:?}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }
}
