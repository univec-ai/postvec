// File: engine/src/lib.rs
//!
//! ## ninference Engine
//!
//! This crate contains the `InferenceEngine`, the central component responsible for
//! managing the lifecycle of machine learning models and their corresponding executors.
//!
//! It is designed to be used by a server or other application that needs to run inference.
// --- Module Declarations ---
// These modules contain the core components of the inference engine.
// They are made public to allow the binary crate to use their types.
pub mod config;
pub mod context;
pub mod error;
pub mod executors;
pub mod input;
// The `models` module contains the model trait and all its implementations.
pub mod models;
// The newly integrated tokenizers module.
pub mod tokenizers;
// New module for pooling strategies.
pub mod pooling;
pub mod resolver;
pub mod utils;
// --- Public API Re-exports ---
// Re-export key types to make them easily accessible to consumers of this crate.
pub use config::{set_session_thread_policy, EngineConfig, HostPolicy, SessionThreadPolicy};
pub use context::InputData;
pub use error::EngineError;
pub use executors::{new_executor, Executor, ExecutorOutput};
// Re-export essential components from the new internal `models` module.
pub use models::{
    ExecutorConfiguration, GenericModel, InputMapping, LayerOverview, Model, ModelBackend,
    ModelConfiguration, ModelError, ModelOverview, OutputMapping,
};
// Conditionally re-export the OnnxRuntimeModel.
#[cfg(feature = "onnx")]
pub use models::OnnxRuntimeModel;
// Re-export essential tokenizer components for convenient access.
pub use tokenizers::{
    config as TokenizerConfig, error::TokenizerError, new_tokenizer, Tokenizer,
    TransformerEncodingsWithPosition,
};
// --- Crate-internal Imports ---
use crate::context::Context;
use crate::models::pool::{ModelObject, ModelPool, ModelPoolManager};
use crate::models::ExecutionProvider;
#[cfg(feature = "onnx")]
use anyhow::anyhow;
use axum::http::HeaderMap;
// use num_cpus;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
// We import tokio::task to get access to `spawn_blocking`.
use tokio::task;
// --- Constants ---
const MODELS_DIR_NAME: &str = "models";
const MODEL_CONFIG_FILENAME: &str = "ninference.hub.json";
/// Holds the pool of active models and a template instance
/// for non-inference tasks like reading metadata.
///
/// This wrapper solves the concurrency bottleneck by providing a
/// pool of `Model` instances for concurrent inference, while still
/// offering a single, safe "template" instance for metadata calls
/// (e.g., `overview()`, `configuration()`).
pub struct ModelPoolWrapper {
    /// The pool of `Model` instances for concurrent inference, managed by `deadpool`.
    pub pool: ModelPool,
    /// A single "template" instance used for metadata calls.
    /// This instance is created once during loading and is not
    /// used for actual inference calls via `predict_raw`.
    pub template_model: Arc<dyn Model>,
}

// --- ONNX Runtime Initializer ---
// The ONNX Runtime initialization logic is now part of the `engine` crate.
// This entire block is compiled only when the "onnx" feature is enabled.
#[cfg(feature = "onnx")]
mod onnx_initializer {
    use super::*;
    /// A helper function to find a library file recursively within a directory.
    ///
    /// This function is used at runtime to locate the ONNX Runtime dynamic library
    /// (`.so`, `.dll`, or `.dylib`).
    fn find_library_recursively(dir: &Path, lib_name: &str) -> Option<PathBuf> {
        if !dir.is_dir() {
            return None;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Some(found_path) = find_library_recursively(&path, lib_name) {
                        return Some(found_path);
                    }
                } else if path.is_file() {
                    if let Some(file_name) = path.file_name().and_then(|s| s.to_str()) {
                        if file_name == lib_name {
                            return Some(path);
                        }
                    }
                }
            }
        }
        None
    }
    /// Initializes the ONNX Runtime dynamic library at runtime.
    ///
    /// This function must be called once at application startup before any
    /// `ort` functionality is used.
    /// It finds the appropriate shared library
    /// within the provided root path and loads it.
    ///
    /// # Arguments
    /// * `root_path` - The root directory of the application installation (e.g., from `NINFERENCE_PATH`).
    pub fn initialize_onnx(root_path: &Path) -> anyhow::Result<()> {
        log::info!("Attempting to dynamically load ONNX Runtime library...");
        let lib_name = if cfg!(target_os = "windows") {
            "onnxruntime.dll"
        } else if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else {
            "libonnxruntime.so"
        };
        let libs_search_path = root_path.join("libs");
        log::info!(
            "Searching for '{}' in '{}'...",
            lib_name,
            libs_search_path.display()
        );
        let lib_path = find_library_recursively(&libs_search_path, lib_name).ok_or_else(|| {
            anyhow!(
                "Could not find the ONNX Runtime library ('{}') within '{}'. \
                 Please ensure `NINFERENCE_PATH` is set correctly and the library exists.",
                lib_name,
                libs_search_path.display()
            )
        })?;
        log::info!("Found ONNX Runtime library at: {}", lib_path.display());
        // This is the corrected initialization sequence that addresses the compiler errors.
        // 1. `ort::init_from` expects an argument that implements `ToString`. A `PathBuf` does not,
        //    so we convert it to a string representation using `to_string_lossy()`.
        //    This safely handles potentially non-UTF8 characters in the path.
        // 2. `ort::init_from` returns an `EnvironmentBuilder`, not a `Result`. We must call the
        //    `.commit()` method on this builder to finalize the initialization.
        // 3. It is the `.commit()` method that returns the `Result` we need to check for errors.
        ort::init_from(lib_path.to_string_lossy())
            .commit()
            .map_err(|e| anyhow!("Failed to initialize ONNX Runtime: {}", e))?;
        log::info!("ONNX Runtime library loaded and initialized successfully.");
        Ok(())
    }
}
// Re-export the initializer function to be accessible from outside the crate.
#[cfg(feature = "onnx")]
pub use onnx_initializer::initialize_onnx;
// Provide a stub function for when the "onnx" feature is NOT enabled.
// This ensures that calling code will compile successfully
// even if it's not using the ONNX backend.
#[cfg(not(feature = "onnx"))]
pub fn initialize_onnx(_root_path: &Path) -> anyhow::Result<()> {
    log::warn!("ONNX feature is not enabled. Skipping ONNX Runtime initialization.");
    Ok(())
}
/// The `InferenceEngine` struct holds the state of the inference system.
///
/// It manages a collection of loaded models and their associated executors.
pub struct InferenceEngine {
    /// A thread-safe map of model names to their *model pool wrappers*.
    /// Each wrapper contains a pool of `Model` instances for concurrent
    /// inference and a single "template" instance for metadata.
    models: RwLock<HashMap<String, Arc<ModelPoolWrapper>>>,
    /// A thread-safe map of model names to their corresponding executors.
    /// Executors are stateless and can be shared, so they do not need a pool.
    executors: RwLock<HashMap<String, Arc<dyn Executor>>>,
    /// A thread-safe map of model names to their directory paths on disk.
    /// This is populated at startup to allow for correct path resolution.
    model_paths: RwLock<HashMap<String, PathBuf>>,
    /// The configuration for the engine, containing paths and host policy.
    config: Arc<EngineConfig>,
    /// The model resolver for dynamic model lookup.
    pub resolver: Arc<crate::resolver::ModelResolver>,
    /// Global admission gate over concurrently executing predictions, from
    /// `EngineConfig::host_policy.admission_limit`. `None` = unlimited.
    admission: Option<Arc<tokio::sync::Semaphore>>,
    /// Serializes the mutation-bearing half of dynamic model loading.
    ///
    /// Discovery and dependency loading happen before this gate. The gate is
    /// held through publication (path + pool insert), executor build, and
    /// resolver rebuild, so two callers can never commit the same model
    /// concurrently. Where the native instantiation sits relative to the
    /// gate depends on `HostPolicy::serialized_model_loads`: a shared host
    /// takes the gate first and moves the permit into the blocking
    /// instantiation task (cancelling the async caller cannot admit a second
    /// native load while the first still runs); the standalone default
    /// instantiates before the gate so concurrent cold-starts of different
    /// models parallelize their native work.
    ///
    /// The mutex is NOT re-entrant: code running under it must never call
    /// `load_model` again on the same task. `LOAD_COMMIT_HELD` marks the
    /// gated region so `executors::build_dependencies` fails typed instead
    /// of self-deadlocking.
    model_load_commit: Arc<tokio::sync::Mutex<()>>,
}

tokio::task_local! {
    /// Set for the duration of `load_model`'s gated publish/build phase.
    /// `build_dependencies` consults it: re-entering `load_model` from under
    /// the commit gate would block the same task on the non-reentrant mutex
    /// it already holds and permanently wedge all future loads.
    static LOAD_COMMIT_HELD: ();
}

/// True when the current task is inside `load_model`'s gated phase.
pub(crate) fn load_commit_held() -> bool {
    LOAD_COMMIT_HELD.try_with(|_| ()).is_ok()
}

/// Test-only synchronization for the load lifecycle's cancellation window.
#[cfg(test)]
pub(crate) mod load_test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// While set, `load_model` parks (cooperatively, cancellable) right
    /// after its optimistic publication, so a test can deterministically
    /// observe or cancel the published-but-not-ready window.
    pub static HOLD_AT_PUBLICATION: AtomicBool = AtomicBool::new(false);

    pub async fn pause_at_publication() {
        while HOLD_AT_PUBLICATION.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }
}

/// Roll back the mutation-bearing half of `load_model` even when its future
/// is *dropped*. Ordinary `Result` cleanup is insufficient for async code:
/// timeout/client cancellation destroys the future without executing the
/// statements after its current `.await`.
struct ModelLoadRollback<'a> {
    engine: &'a InferenceEngine,
    model_name: &'a str,
    armed: bool,
}

impl ModelLoadRollback<'_> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ModelLoadRollback<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.engine.models.write().unwrap().remove(self.model_name);
        self.engine
            .executors
            .write()
            .unwrap()
            .remove(self.model_name);
        self.engine
            .model_paths
            .write()
            .unwrap()
            .remove(self.model_name);
        let models = self.engine.models.read().unwrap();
        self.engine.resolver.rebuild(
            models
                .values()
                .map(|wrapper| wrapper.template_model.configuration()),
        );
    }
}
/// Instantiates a single model object from its configuration.
///
/// This is a helper function that can be called from a blocking thread.
/// It is made public so the `ModelPoolManager` can use it as its factory.
// This attribute tells the compiler to allow unused variables in this function
// when certain features are NOT enabled.
#[cfg_attr(not(feature = "onnx"), allow(unused_variables))]
pub fn instantiate_model_from_config(
    model_config: &mut ModelConfiguration,
    config_path: &Path,
) -> Result<Arc<dyn Model>, EngineError> {
    let model_name = &model_config.name;
    let model: Arc<dyn Model> = match model_config.backend {
        #[cfg(feature = "onnx")]
        ModelBackend::OnnxRuntime => {
            if let Some(file_path) = &model_config.file_path {
                let mut absolute_model_path = PathBuf::from(file_path);
                if !absolute_model_path.is_absolute() {
                    if let Some(parent_dir) = config_path.parent() {
                        absolute_model_path = parent_dir.join(file_path);
                    }
                }
                model_config.file_path = Some(absolute_model_path.to_string_lossy().to_string());
            }
            Arc::new(OnnxRuntimeModel::new(model_config.clone())?)
        }
        ModelBackend::Generic => Arc::new(GenericModel::new(model_config.clone())),
        _ => {
            return Err(EngineError::Configuration(format!(
                "Unsupported backend '{:?}' for model '{}' or its feature flag is not enabled.",
                model_config.backend, model_name
            )));
        }
    };
    Ok(model)
}
/// Whether a directory encountered by the model scan is a real backend or
/// model directory.
///
/// A **dot-prefixed name is private state, never a model**. `postvec-cli`
/// keeps its transient trees under `models/`, and two of them sit at exactly
/// the depth this scan walks (`models/<backend>/<model>/ninference.hub.json`):
/// `models/.staging/<name>.<uniq>/` holds a fully extracted but *uncommitted*
/// model, and `models/.trash/<name>.<uniq>/` holds one whose removal has not
/// finished. Without this filter, a restart would make an uncommitted model
/// resident or resurrect a removed one.
///
/// `models/.swap/<backend>/<name>/` — the predecessor of an in-flight
/// replacement — is one level deeper than the scan reaches, so it was never
/// directly loadable. It is filtered anyway: the rule is "dot means private",
/// stated once and applied uniformly, rather than a list of the layouts that
/// happen to collide today.
///
/// No hub-produced backend or model directory ever starts with a dot.
///
/// A non-UTF-8 name is skipped for the same reason it can never be addressed:
/// nothing can name it in a load request.
fn is_scannable_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.starts_with('.'))
}

/// Discover every model configuration under `models_dir`, two levels deep.
///
/// Shared by the startup scan; kept separate so the directory rules are
/// testable without a backend feature or a real model.
fn discover_model_configs(
    models_dir: &Path,
) -> Result<Vec<(ModelConfiguration, PathBuf)>, EngineError> {
    let mut discovered = Vec::new();
    for backend_entry in fs::read_dir(models_dir)? {
        let backend_path = backend_entry?.path();
        if !backend_path.is_dir() || !is_scannable_dir(&backend_path) {
            continue;
        }
        log::debug!("Scanning backend directory: {}", backend_path.display());
        for model_entry in fs::read_dir(&backend_path)? {
            let model_path = model_entry?.path();
            if !model_path.is_dir() || !is_scannable_dir(&model_path) {
                continue;
            }
            let config_path = model_path.join(MODEL_CONFIG_FILENAME);
            if !config_path.is_file() {
                continue;
            }
            match ModelConfiguration::from_file(&config_path) {
                Ok(config) => {
                    if config.enabled {
                        discovered.push((config, config_path));
                    } else {
                        log::info!("-> Skipping disabled model '{}'", config.name);
                    }
                }
                Err(e) => log::error!(
                    "  => Failed to parse config at {}: {}",
                    config_path.display(),
                    e
                ),
            }
        }
    }
    Ok(discovered)
}

/// A standalone helper function to find a model's configuration file by scanning directories.
///
/// This function is no longer an instance method. It takes the `root_path` directly,
/// making it easy to use from within a `spawn_blocking` task without needing access to `self`.
fn find_model_config_path(root_path: &Path, model_name: &str) -> Result<PathBuf, EngineError> {
    let models_dir = root_path.join(MODELS_DIR_NAME);
    if !models_dir.is_dir() {
        return Err(EngineError::Configuration(
            "Models directory not found.".to_string(),
        ));
    }
    // Iterate through each backend directory (e.g., "onnxruntime")
    for backend_entry in fs::read_dir(models_dir)? {
        let backend_path = backend_entry?.path();
        if backend_path.is_dir() && is_scannable_dir(&backend_path) {
            // Iterate through each model directory within the backend
            for model_entry in fs::read_dir(backend_path)? {
                let model_path = model_entry?.path();
                // Check if the directory name matches the model we're looking for
                if model_path.is_dir()
                    && is_scannable_dir(&model_path)
                    && model_path.file_name().and_then(|s| s.to_str()) == Some(model_name)
                {
                    let config_path = model_path.join(MODEL_CONFIG_FILENAME);
                    if config_path.is_file() {
                        return Ok(config_path);
                    }
                }
            }
        }
    }
    Err(EngineError::NotFound(format!(
        "Configuration file for model '{}' not found in models directory.",
        model_name
    )))
}
impl InferenceEngine {
    /// Creates a new, empty `InferenceEngine`.
    ///
    /// Model loading and executor building are performed in subsequent, separate steps.
    ///
    /// # Arguments
    /// * `config` - An `Arc`-wrapped `EngineConfig` with necessary settings.
    pub fn new(config: Arc<EngineConfig>) -> Self {
        let admission = config
            .host_policy
            .admission_limit
            .filter(|limit| *limit > 0)
            .map(|limit| Arc::new(tokio::sync::Semaphore::new(limit)));
        Self {
            models: RwLock::new(HashMap::new()),
            executors: RwLock::new(HashMap::new()),
            model_paths: RwLock::new(HashMap::new()),
            config,
            resolver: Arc::new(crate::resolver::ModelResolver::new()),
            admission,
            model_load_commit: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    /// Creates a new
    /// `InferenceEngine` and loads a specific list of models.
    ///
    /// This constructor is a convenient way to initialize an engine and have it
    /// ready with a predefined set of models, including all their dependencies.
    /// It performs all necessary steps: instantiation, dependency resolution,
    /// executor creation, and building.
    /// This is an alternative to calling `new()`,
    /// `load_models_and_executors()`, and `build_executors()` separately.
    ///
    /// # Arguments
    /// * `config` - An `Arc`-wrapped `EngineConfig` with necessary settings.
    /// * `model_names` - A slice of model names to load and build.
    ///
    /// # Returns
    /// A `Result` containing the fully initialized `InferenceEngine` or an `EngineError`.
    pub async fn default_with_models(
        config: Arc<EngineConfig>,
        model_names: &[String],
    ) -> Result<Self, EngineError> {
        // First, create a new, empty engine instance using the standard constructor.
        let engine = Self::new(config);
        log::info!(
            "Initializing engine with a specific set of models: {:?}",
            model_names
        );
        // Iterate through the provided list of model names and load each one.
        // The `load_model` method is asynchronous and handles the entire lifecycle
        // for a single model: dependency resolution, downloading, instantiation,
        // executor creation, and building.
        for model_name in model_names {
            log::info!("=> Attempting to load and build '{}'...", model_name);
            engine.load_model(model_name).await?;
        }
        log::info!("Successfully loaded and built all specified models and their dependencies.");
        // Return the fully configured and ready-to-use engine.
        Ok(engine)
    }
    /// Determines the pool size for a given model configuration.
    ///
    /// The logic is as follows:
    /// 1. Check for `pool_size` in the executor's `params`.
    /// 2. If not found, default based on:
    ///    a) If GPU is requested AND the app is compiled with that GPU feature, use GPU default.
    ///    b) Otherwise, use the CPU default (cores * factor).
    fn get_pool_size(&self, config: &ModelConfiguration) -> usize {
        const DEFAULT_GPU_POOL_SIZE: usize = 4;
        const DEFAULT_CPU_POOL_SIZE_FACTOR: usize = 1; // 1 per core
                                                       // 1. Check for a `pool_size` parameter in `executor.params`
                                                       //    This always takes top priority.
        if let Some(val) = config.executor.params.get("pool_size") {
            if let Some(size) = val.as_u64() {
                if size > 0 {
                    log::info!("Found 'pool_size: {}' in model configuration.", size);
                    return size as usize;
                }
            }
        }
        // 2. If not found, calculate a sensible default
        log::warn!(
            "No 'pool_size' found in config for {}. Calculating default...",
            config.name
        );
        // Check if the *config* is asking for any known GPU provider
        let is_gpu_requested = config
            .execution_providers
            .iter()
            .any(|ep| matches!(ep, ExecutionProvider::Cuda | ExecutionProvider::TensorRt));
        // Check if the required compile-time features are *also* enabled
        let is_gpu_available = is_gpu_requested && {
            // This `cfg` block will evaluate to `true` or `false` at compile time
            #[cfg(all(feature = "onnx", any(feature = "ort-cuda", feature = "ort-tensorrt")))]
            let onnx_gpu_enabled = config.backend == ModelBackend::OnnxRuntime;
            #[cfg(not(all(
                feature = "onnx",
                any(feature = "ort-cuda", feature = "ort-tensorrt")
            )))]
            let onnx_gpu_enabled = false;
            // The app has GPU support *for this specific model backend*
            onnx_gpu_enabled
        };
        // --- Final Decision ---
        if is_gpu_available {
            // Case 1: Config asked for GPU, and we have it.
            log::info!("Using default GPU pool size: {}", DEFAULT_GPU_POOL_SIZE);
            DEFAULT_GPU_POOL_SIZE
        } else {
            // Case 2 (Fallback):
            // - Config asked for CPU.
            // - OR Config asked for GPU, but app was compiled without GPU support.
            // let cpu_size = (num_cpus::get() * DEFAULT_CPU_POOL_SIZE_FACTOR).max(1);
            let cpu_size = DEFAULT_CPU_POOL_SIZE_FACTOR;
            if is_gpu_requested {
                // This is the specific case you asked about
                log::warn!(
                    "GPU was requested for model '{}', but app was not compiled with GPU support. Falling back to default CPU pool size: {}",
                    config.name,
                    cpu_size
                );
            } else {
                log::info!("Using default CPU pool size: {}", cpu_size);
            }
            cpu_size
        }
    }
    /// Scans the model directory and loads all models and their executors in phases.
    ///
    /// This function orchestrates the startup sequence:
    /// 1.  It discovers and parses all model configuration files from a nested structure.
    /// 2.  It instantiates one "template" `Model` object for each model.
    /// 3.  It creates a `ModelPool` for each model, using the config as a factory.
    /// 4.  It stores the pool and template in a `ModelPoolWrapper`.
    /// 5.  It then instantiates an `Executor` for each successfully loaded model.
    pub fn load_models_and_executors(&self) -> Result<(), EngineError> {
        let models_dir = self.config.root_path.join(MODELS_DIR_NAME);
        log::info!("Scanning for models in: {}", models_dir.display());
        if !models_dir.exists() || !models_dir.is_dir() {
            log::warn!("Models directory not found. No models will be loaded.");
            return Ok(());
        }
        // --- Phase 1: Discover and parse all model configurations ---
        let configs_to_process = discover_model_configs(&models_dir)?;
        log::info!(
            "Found {} model configuration file(s) to process.",
            configs_to_process.len()
        );
        // --- Phase 2: Instantiate all models and add them to the engine ---
        let mut loaded_model_configs = Vec::new();
        {
            // This map now stores <String, Arc<ModelPoolWrapper>>
            let mut models_map = self.models.write().unwrap();
            let mut paths_map = self.model_paths.write().unwrap();
            for (mut config, config_path) in configs_to_process {
                let model_name = config.name.clone();
                // This IIFE (Immediately Invoked Function Expression) wraps the
                // fallible loading logic in a closure, allowing us to use `?`
                // and handle the `Result` neatly.
                match (|| -> Result<(), EngineError> {
                    // 1. Create ONE "template" instance first. This validates the
                    // config and model files.
                    let template_model = self.instantiate_model(&mut config, &config_path)?;
                    if !template_model.valid() {
                        log::warn!("  Model '{}' is invalid and will be skipped.", model_name);
                        return Ok(());
                    }
                    // 2. Get pool size from config or defaults.
                    let pool_size = self.get_pool_size(&config);
                    log::info!(
                        "-> Instantiated template model '{}' (pool size: {})",
                        model_name,
                        pool_size
                    );
                    // 3. Create the pool factory (Manager)
                    let manager = ModelPoolManager {
                        config: config.clone(), // Clone config for the factory
                        config_path: config_path.clone(),
                        #[cfg(test)]
                        test_model: None,
                    };
                    // 4. Create the pool
                    let pool = deadpool::managed::Pool::builder(manager)
                        .max_size(pool_size)
                        .build()
                        .map_err(|e| {
                            EngineError::Configuration(format!(
                                "Failed to create model pool: {}",
                                e
                            ))
                        })?;
                    // 5. Create the wrapper
                    let pool_wrapper = Arc::new(ModelPoolWrapper {
                        pool,
                        template_model,
                    });
                    // 6. Store the pool wrapper in the map
                    models_map.insert(model_name.clone(), pool_wrapper);
                    // 7. Store path (unchanged)
                    if let Some(model_dir) = config_path.parent() {
                        paths_map.insert(model_name.clone(), model_dir.to_path_buf());
                    }
                    loaded_model_configs.push(config);
                    Ok(())
                })() {
                    Err(e) => {
                        log::error!("  Failed to instantiate model '{}': {}", model_name, e)
                    }
                    Ok(_) => {
                        // Success, do nothing
                    }
                }
            }
        }
        log::info!(
            "Model instantiation complete. {} valid model(s) loaded.",
            loaded_model_configs.len()
        );

        // Rebuild resolver with all loaded models
        {
            let models_map = self.models.read().unwrap();
            let configs = models_map
                .values()
                .map(|wrapper| wrapper.template_model.configuration());
            self.resolver.rebuild(configs);
        }

        // --- Phase 3: Instantiate executors for each successfully loaded model ---
        let mut successful_executors = 0;
        for config in loaded_model_configs {
            let model_name = config.name.clone();
            match new_executor(self, &config) {
                Ok(executor) => {
                    log::info!("-> Created executor for model '{}'", model_name);
                    self.executors.write().unwrap().insert(model_name, executor);
                    successful_executors += 1;
                }
                Err(e) => {
                    log::error!(
                        "  Failed to create executor for model '{}': {}",
                        model_name,
                        e
                    );
                    log::warn!(
                        "  Unloading model '{}' due to executor creation failure.",
                        model_name
                    );
                    // Unload the model and its path for consistency.
                    self.models.write().unwrap().remove(&model_name);
                    self.model_paths.write().unwrap().remove(&model_name);
                }
            }
        }
        log::info!(
            "Executor creation complete. {} executor(s) created.",
            successful_executors
        );
        Ok(())
    }
    /// Asynchronously loads a single model by its name, recursively loading its dependencies first.
    ///
    /// This is the core method for Just-In-Time (JIT) model loading.
    /// It orchestrates a multi-step, transactional process:
    /// 1.  Finds and parses the model's configuration file to identify dependencies.
    /// 2.  Recursively calls itself to ensure all dependency models are fully loaded and built.
    /// 3.  Once all dependencies are met, it instantiates the "template" model and the model pool.
    /// 4.  It then calls the executor's `build()` method.
    /// 5.  If any step fails, all changes for the current model are rolled back to prevent a
    ///     partially loaded state.
    pub async fn load_model(&self, model_name: &str) -> Result<(), EngineError> {
        // First, check if the model is already loaded AND ready (pool +
        // executor): a partially loaded model — a cancelled earlier load —
        // must fall through so this attempt heals it.
        if self.is_model_ready(model_name) {
            log::debug!(
                "Attempted to load model '{}', but it is already loaded.",
                model_name
            );
            return Ok(());
        }
        // --- Phase 1: Discover and parse configuration ---
        // This is now much cleaner.
        // We clone the necessary data and move it into the
        // blocking task, which can now call the standalone `find_model_config_path` helper.
        let model_name_clone = model_name.to_string();
        let root_path_clone = self.config.root_path.clone();
        let (config_path, config) = match task::spawn_blocking(move || {
            let config_path = find_model_config_path(&root_path_clone, &model_name_clone)?;
            let config = ModelConfiguration::from_file(&config_path)?;
            Ok::<(PathBuf, ModelConfiguration), EngineError>((config_path, config))
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => return Err(e),
            Err(join_error) => return Err(EngineError::Anyhow(join_error.into())),
        };
        // `enabled: false` is the operator's persistent "do not serve this"
        // switch, and it has to mean the same thing on **every** load path or
        // it means nothing.
        //
        // The startup scan (`discover_model_configs`) already skips disabled
        // descriptors, but `build_executors` runs immediately afterwards and
        // JIT-loads each executor's `dependencies` through this function — so
        // without this check a disabled dependency of an enabled parent came
        // straight back on the next restart, and disabling it was a lie that
        // only survived until the process bounced. `resolver.rebuild` skips
        // disabled configurations too, so such a model also sat in the model
        // and executor maps while being absent from the resolver indices.
        //
        // Refusing here is a per-model failure, not a fatal one: the startup
        // builder logs it and unloads that parent, and an explicit load route
        // reports it to its caller.
        if !config.enabled {
            return Err(EngineError::Configuration(format!(
                "Model '{}' is disabled in {} and will not be loaded. Enable it (postvec: \
                 `postvec model activate {}`) and retry.",
                model_name,
                config_path.display(),
                model_name
            )));
        }
        log::info!(
            "Dynamically loading model '{}' from {}",
            model_name,
            config_path.display()
        );
        // --- Phase 2: Resolve dependencies without mutating this model's state ---
        let model_dir = config_path.parent().ok_or_else(|| {
            EngineError::Configuration(format!(
                "Could not determine parent directory for config path '{}'",
                config_path.display()
            ))
        })?;
        let model_dir = model_dir.to_path_buf();
        // No state for `model_name` is published before all dependencies are
        // ready. Cancellation in this phase therefore leaves nothing to heal.
        let dependency_result: Result<(), EngineError> = (async {
            if !config.dependencies.is_empty() {
                log::info!(
                    "Model '{}' has dependencies: {:?}. Ensuring they are loaded.",
                    model_name,
                    config.dependencies
                );
                for dep_name in &config.dependencies {
                    log::info!(
                        "Loading dependency '{}' for model '{}'...",
                        dep_name,
                        model_name
                    );
                    // Dependency files are resolved from local disk; the
                    // recursive `load_model` call below fails if they are
                    // not present.
                    // To satisfy the borrow checker for the recursive async call, we box the future.
                    // This places the future on the heap, giving it a known size at compile time
                    // and breaking the infinite-size recursive type definition.
                    Box::pin(self.load_model(dep_name)).await?;
                    log::info!("Successfully loaded dependency '{}'.", dep_name);
                }
            }
            // Validate that the model name in the config matches the directory name.
            if config.name != model_name {
                return Err(EngineError::Configuration(format!(
                    "Model name mismatch: directory is '{}' but config name is '{}'.",
                    model_name, config.name
                )));
            }
            Ok(())
        })
        .await;
        dependency_result?;

        // --- Phase 3: one cancellation-safe commit/build transaction ---
        // Instantiate before publishing any map entry. Where the native
        // instantiation sits relative to the commit gate is a host-policy
        // decision (see `HostPolicy::serialized_model_loads`); in both
        // shapes the readiness re-check runs under the gate, because another
        // caller may have completed this model in the meantime.
        let mut config_clone = config.clone();
        let config_path_clone = config_path.clone();
        let (_load_permit, template_model) = if self.config.host_policy.serialized_model_loads {
            // Shared-host shape (postvec embedded): gate first, then move
            // the permit into the blocking task so cancellation cannot admit
            // a second native load while this one is still running.
            let load_permit = self.model_load_commit.clone().lock_owned().await;
            if self.is_model_ready(model_name) {
                return Ok(());
            }
            let (permit, template_result) = task::spawn_blocking(move || {
                let result = instantiate_model_from_config(&mut config_clone, &config_path_clone);
                (load_permit, result)
            })
            .await
            .map_err(|e| EngineError::Anyhow(e.into()))?;
            (permit, template_result?)
        } else {
            // Standalone shape: instantiate outside the gate so concurrent
            // JIT cold-starts of different models run their native work in
            // parallel; only publication + executor build serialize. A load
            // cancelled here has published nothing, so there is nothing to
            // roll back — the discarded instantiation is the whole cost.
            let template_result = task::spawn_blocking(move || {
                instantiate_model_from_config(&mut config_clone, &config_path_clone)
            })
            .await
            .map_err(|e| EngineError::Anyhow(e.into()))?;
            let template = template_result?;
            let load_permit = self.model_load_commit.clone().lock_owned().await;
            if self.is_model_ready(model_name) {
                return Ok(());
            }
            (load_permit, template)
        };
        if !template_model.valid() {
            return Err(EngineError::Configuration(format!(
                "Model '{}' is invalid.",
                model_name
            )));
        }

        let mut rollback = ModelLoadRollback {
            engine: self,
            model_name,
            armed: false,
        };
        // The scope marks this task as holding the commit gate so
        // `build_dependencies` fails typed instead of re-entering
        // `load_model` and self-deadlocking on the non-reentrant mutex.
        let result: Result<(), EngineError> = LOAD_COMMIT_HELD
            .scope((), async {
                // Create the pool.
                let pool_size = self.get_pool_size(&config);
                log::info!(
                    "  -> JIT Model pool size for '{}' set to {}",
                    model_name,
                    pool_size
                );
                let manager = ModelPoolManager {
                    config: config.clone(),
                    config_path: config_path.clone(),
                    #[cfg(test)]
                    test_model: None,
                };
                let pool = deadpool::managed::Pool::builder(manager)
                    .max_size(pool_size)
                    .build()
                    .map_err(|e| {
                        EngineError::Configuration(format!("Failed to create model pool: {}", e))
                    })?;
                let pool_wrapper = Arc::new(ModelPoolWrapper {
                    pool,
                    template_model,
                });
                // Optimistically insert path + pool. Executors may resolve the
                // current model's assets/template during construction. The armed
                // guard removes both on error, panic unwind, or future drop.
                self.model_paths
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), model_dir);
                // This makes it available for the executor's constructor, which may need to
                // query the engine for the template model via `get_model`.
                // If any subsequent step fails, this will be rolled back.
                self.models
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), pool_wrapper);
                rollback.armed = true;

                // Test-only cancellation point that discriminates the drop-time
                // rollback contract without needing a real slow native model:
                // with the hold flag set, the load parks here cooperatively
                // until the test has observed the published-but-not-ready
                // window (a single yield was a race on fast backends).
                #[cfg(test)]
                load_test_hooks::pause_at_publication().await;

                // Instantiate the executor. This should now succeed because all
                // dependencies were loaded and the model (wrapper) is now in the engine's map.
                let executor = new_executor(self, &config)?;
                // Build the executor, which may perform additional setup.
                executor.build(self, &config).await?;
                // On final success, insert the executor into its map.
                // The model pool is already present from the optimistic insertion.
                self.executors
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), executor);
                // `unload_model` is intentionally synchronous and may be called
                // by a host while this async build is in flight. If it removed the
                // optimistic pool/path, do not publish an executor-only success;
                // route through the same rollback guard instead.
                if !self.is_model_ready(model_name) {
                    return Err(EngineError::Configuration(format!(
                        "Model '{}' was unloaded while its executor was being built.",
                        model_name
                    )));
                }
                Ok(())
            })
            .await;
        // `_load_permit` was declared before `rollback`, so Rust's reverse
        // drop order runs rollback first and only then admits the next load.
        // Do not rebind it below `rollback`: that would publish the partial
        // state to a waiter during normal error cleanup.
        if let Err(e) = &result {
            log::error!(
                "Failed during dynamic load of model '{}': {}. Reverting changes.",
                model_name,
                e
            );
        } else {
            log::info!("Successfully loaded and built model '{}'.", model_name);
            // Rebuild resolver
            let models_map = self.models.read().unwrap();
            let configs = models_map
                .values()
                .map(|wrapper| wrapper.template_model.configuration());
            self.resolver.rebuild(configs);
            rollback.disarm();
        }
        result
    }
    /// Unloads a model and its executor, freeing up resources.
    /// Returns true if a model was unloaded, false otherwise.
    pub fn unload_model(&self, model_name: &str) -> bool {
        log::info!("Unloading model '{}'...", model_name);
        // Acquire write locks on all maps to ensure atomicity.
        let mut models = self.models.write().unwrap();
        let mut executors = self.executors.write().unwrap();
        let mut paths = self.model_paths.write().unwrap();
        // The removal from models is the primary operation.
        // If it succeeds, we also remove the associated executor and path for consistency.
        // Dropping the `Arc<ModelPoolWrapper>` will cause `deadpool` to
        // shut down and drop all pooled `Model` instances.
        if models.remove(model_name).is_some() {
            executors.remove(model_name);
            paths.remove(model_name);
            log::info!("Successfully unloaded model '{}'.", model_name);
            // Rebuild resolver
            let models_map = models; // models is already a write guard, we can iterate it or clone values
            let configs = models_map
                .values()
                .map(|wrapper| wrapper.template_model.configuration());
            self.resolver.rebuild(configs);

            true
        } else {
            log::warn!(
                "Attempted to unload model '{}', but it was not loaded.",
                model_name
            );
            false
        }
    }
    /// A synchronous wrapper around the public `instantiate_model_from_config` helper.
    fn instantiate_model(
        &self,
        model_config: &mut ModelConfiguration,
        config_path: &Path,
    ) -> Result<Arc<dyn Model>, EngineError> {
        instantiate_model_from_config(model_config, config_path)
    }
    /// Asynchronously calls the `build()` method on all loaded executors.
    pub async fn build_executors(&self) -> Result<(), EngineError> {
        log::info!("Building loaded executors...");
        let model_names: Vec<String> = self.executors.read().unwrap().keys().cloned().collect();
        let mut successful_builds = 0;
        let mut failed_builds = 0;
        for model_name in model_names {
            // We need to clone the Arc to work with it outside the read lock.
            let executor = self.executors.read().unwrap().get(&model_name).cloned();
            // Get the template model for its configuration
            let model = self.get_model(&model_name)?;
            if let Some(executor) = executor {
                log::info!("Building executor for model '{}'...", model_name);
                // The build method is now async and requires the config.
                match executor.build(self, model.configuration()).await {
                    Ok(_) => {
                        log::info!("=> Successfully built executor for '{}'", model_name);
                        successful_builds += 1;
                    }
                    Err(e) => {
                        log::error!(
                            "  Failed to build executor for model '{}': {}",
                            model_name,
                            e
                        );
                        log::warn!("  Unloading model '{}' due to build failure.", model_name);
                        // If build fails, remove the model, executor, and path.
                        self.models.write().unwrap().remove(&model_name);
                        self.executors.write().unwrap().remove(&model_name);
                        self.model_paths.write().unwrap().remove(&model_name);
                        failed_builds += 1;
                    }
                }
            }
        }
        log::info!(
            "Executor building complete. Success: {}, Failed/Unloaded: {}.",
            successful_builds,
            failed_builds
        );
        Ok(())
    }
    /// This convenience method builds a structured payload from a slice of `serde_json::Value`s.
    /// It allows providing fewer inputs than the model has configured, mapping them to the
    /// first N JSON keys in the executor configuration.
    // This method's signature is updated to take `self: Arc<Self>`.
    pub async fn predict(
        self: Arc<Self>,
        model_name: &str,
        inputs: &[Value],
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        log::debug!("Executing prediction for model '{}'", model_name);
        // We can still use `self.get_model` because `Arc` smart pointers
        // allow calling methods of the inner type.
        // This now gets the
        // template model, which is all we need for the config.
        let model = self.get_model(model_name)?;
        let target_input_mappings = model.configuration().executor.inputs.clone();
        // Sanity check: ensure the user does not provide more inputs than the model
        // is configured to accept.
        if inputs.len() > target_input_mappings.len() {
            return Err(EngineError::Prediction(format!(
                "Too many inputs for chained prediction on model '{}': Executor accepts at most {} inputs, but {} were provided.",
                model_name,
                target_input_mappings.len(),
                inputs.len()
            )));
        }
        // Create the InputData::Structured variant.
        // This simplifies the
        // calling logic, as the caller no longer needs to build a JSON object.
        let input_data = InputData::Structured(inputs.to_vec());
        // This call is now valid because `self` is an `Arc`.
        // The `predict_raw` method will consume this `Arc`.
        self.predict_raw(model_name, input_data, headers, timeout)
            .await
    }
    /// Performs prediction for a given model using the model pool.
    // This method's signature is updated to take `self: Arc<Self>`.
    // This is necessary so it can be cloned into the 'static Context.
    pub async fn predict_raw(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        // ONE absolute deadline for the whole logical request (admission wait
        // + pool wait + execution, INCLUDING every nested prediction a bridge
        // executor makes). The previous shape granted the pool wait a full
        // `timeout` and the execution another one, so total engine wait could
        // approach twice the configured value — and nested calls invented
        // fresh budgets of their own.
        let deadline = std::time::Instant::now() + timeout;
        self.predict_raw_at(model_name, input_data, headers, deadline)
            .await
    }

    /// [`Self::predict_raw`] against a caller-supplied ABSOLUTE deadline.
    ///
    /// For hosts that spend part of the caller's budget before reaching the
    /// engine — model loading, request validation, payload construction —
    /// rebasing a `Duration` at this call would silently hand that
    /// preprocessing time back to the request. Passing the instant the
    /// caller committed to keeps admission wait + pool wait + execution +
    /// every nested bridge prediction under ONE deadline that preprocessing
    /// already spent from. An already-elapsed deadline fails admission
    /// immediately and leaks no capacity.
    pub async fn predict_raw_at(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        deadline: std::time::Instant,
    ) -> Result<ExecutorOutput, EngineError> {
        // An already-elapsed deadline is refused up front, deterministically:
        // `timeout_at` polls its inner future before the timer, so a
        // ready-on-first-poll stage (a free admission permit, a warm pool)
        // could otherwise win the race against an expired clock and execute
        // work whose caller has already given up.
        if deadline <= std::time::Instant::now() {
            return Err(EngineError::Timeout(format!(
                "deadline elapsed before engine admission for '{model_name}'"
            )));
        }
        // Host-policy admission — ROOT requests only: the per-model pools
        // bound same-model concurrency, but requests for *different* models
        // could all enter blocking execution at once — on a shared host
        // (postvec embedded mode) that oversubscribes the machine. The permit
        // is moved into the blocking closure below, so a timed-out caller
        // does NOT release capacity while its native computation is still
        // running. Nested predictions (bridge components) ride the root's
        // permit via [`Self::predict_raw_nested`] — re-acquiring here would
        // deadlock at `admission_limit = 1`.
        let admission_permit = match &self.admission {
            Some(sem) => {
                let acquire = sem.clone().acquire_owned();
                match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), acquire)
                    .await
                {
                    Ok(Ok(permit)) => Some(permit),
                    Ok(Err(_)) => {
                        return Err(EngineError::Prediction(
                            "engine admission gate is closed".to_string(),
                        ))
                    }
                    Err(_) => {
                        return Err(EngineError::Timeout(format!(
                            "Timed out waiting for engine admission for '{}'",
                            model_name
                        )))
                    }
                }
            }
            None => None,
        };
        self.predict_raw_inner(model_name, input_data, headers, deadline, admission_permit)
            .await
    }

    /// A NESTED prediction made from inside an executor (bridge components).
    /// It reuses the root request's admission permit — the root holds the
    /// engine-wide permit for the whole logical request, so acquiring another
    /// here would deadlock at `admission_limit = 1` — and is bounded by the
    /// root's absolute deadline, never a fresh budget.
    pub(crate) async fn predict_raw_nested(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        deadline: std::time::Instant,
    ) -> Result<ExecutorOutput, EngineError> {
        self.predict_raw_inner(model_name, input_data, headers, deadline, None)
            .await
    }

    async fn predict_raw_inner(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        deadline_std: std::time::Instant,
        admission_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<ExecutorOutput, EngineError> {
        log::debug!("Executing prediction for model '{}'", model_name);
        let deadline = tokio::time::Instant::from_std(deadline_std);
        // --- Model Pooling Logic START ---
        // 1. Get the MODEL POOL WRAPPER
        let model_pool_wrapper = self
            .models
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Model pool for '{}' not found", model_name))
            })?;
        // 2. Get the (unchanged) EXECUTOR
        let executor = self.get_executor_for_model(model_name)?;
        // 3. Check out a MODEL INSTANCE from the pool
        // This will wait asynchronously if the pool is empty (up to the deadline).
        let model_instance =
            match tokio::time::timeout_at(deadline, model_pool_wrapper.pool.get()).await {
                Ok(Ok(model)) => model, // `model` is the RAII guard from deadpool
                Ok(Err(e)) => {
                    return Err(EngineError::Prediction(format!(
                        "Failed to get model from pool: {}",
                        e
                    )))
                }
                Err(_) => {
                    return Err(EngineError::Timeout(format!(
                        "Timed out waiting for an available model instance for '{}'",
                        model_name
                    )))
                }
            };
        // 4. Create the 'static Context, passing the checked-out model instance
        // `model_instance` is an `Object` from deadpool that derefs to `Arc<dyn Model>`.
        // We pass it directly into `Context::new`, together with the root
        // deadline every nested query/prediction is clamped to.
        let context = Context::new(
            self.clone(),
            model_instance,
            input_data,
            headers,
            deadline_std,
            // The runtime that hosts termination watchdogs: captured HERE,
            // in async context, because executors fan work out to Rayon
            // threads that have no ambient tokio context of their own.
            tokio::runtime::Handle::current(),
        );
        // 5. The rest of the logic is identical to before,
        //    but with a critical difference in behavior.
        let blocking_task = task::spawn_blocking(move || {
            // This closure now owns `context` AND the admission permit.
            // `context` owns the `model_instance` RAII guard. Dropping a
            // timed-out JoinHandle does not stop this closure — the model
            // lease and the admission permit stay held until the native
            // execution actually returns, which is exactly the accounting
            // the admission gate needs.
            let _admission_permit = admission_permit;
            let result = executor.execute(&context);
            // When `context` is dropped at the end of this closure,
            // the `model_instance` guard is also dropped,
            // which *automatically returns the model to the pool*.
            result
        });
        // --- Model Pooling Logic END ---
        // The remaining budget bounds the *blocking task*.
        match tokio::time::timeout_at(deadline, blocking_task).await {
            // Timeout occurred.
            Err(_) => Err(EngineError::Timeout(format!(
                "Prediction for model '{}' passed its deadline",
                model_name,
            ))),
            // Timeout did not occur, we got a Result from the `spawn_blocking` task.
            // This outer `Ok` means the `JoinHandle` (from `spawn_blocking`) resolved.
            Ok(join_result) => {
                match join_result {
                    // The `spawn_blocking` task panicked or was cancelled.
                    Err(join_error) => Err(EngineError::Anyhow(join_error.into())),
                    // The `spawn_blocking` task completed successfully.
                    // The `inner_result` is the `Result<ExecutorOutput, EngineError>`
                    // returned by `executor.execute(&context)`.
                    Ok(inner_result) => {
                        match inner_result {
                            // The executor's `execute` method ran and returned Ok.
                            Ok(result) => {
                                log::debug!(
                                    "Prediction for model '{}' completed successfully.",
                                    model_name
                                );
                                Ok(result)
                            }
                            // The executor's `execute` method ran and returned an Err.
                            Err(e) => Err(e),
                        }
                    }
                }
            }
        }
    }
    /// Retrieves the "template" model instance for metadata/overview.
    ///
    /// This method is safe to call for non-inference tasks, as it
    /// does not interact with the pool.
    ///
    /// DO NOT USE THE RETURNED `Arc<dyn Model>` FOR `query()` IN A CONCURRENT
    /// CONTEXT.
    /// Use `predict_raw` instead.
    pub fn get_model(&self, model_name: &str) -> Result<Arc<dyn Model>, EngineError> {
        self.models
            .read()
            .unwrap()
            .get(model_name)
            .map(|pool_wrapper| pool_wrapper.template_model.clone()) // Get the template
            .ok_or_else(|| EngineError::NotFound(format!("Model '{}' not found", model_name)))
    }
    /// Checks
    /// if a model is currently loaded in memory.
    pub fn is_model_loaded(&self, model_name: &str) -> bool {
        self.models.read().unwrap().contains_key(model_name)
    }
    /// TRUE only when the model can actually serve a prediction: its pool
    /// AND its executor are both registered. `load_model` now rolls partial
    /// registration back on future drop as well as ordinary errors, but this
    /// stronger predicate also prevents observers from treating the brief
    /// optimistic pool-registration window as serve-ready.
    pub fn is_model_ready(&self, model_name: &str) -> bool {
        self.models.read().unwrap().contains_key(model_name)
            && self.executors.read().unwrap().contains_key(model_name)
    }
    /// Retrieves the executor associated with a given model name.
    pub fn get_executor_for_model(
        &self,
        model_name: &str,
    ) -> Result<Arc<dyn Executor>, EngineError> {
        self.executors
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Executor for model '{}' not found", model_name))
            })
    }
    /// Returns a list of all currently loaded and active models.
    pub fn get_active_models(&self) -> Vec<String> {
        self.models.read().unwrap().keys().cloned().collect()
    }
    /// Retrieves the directory where the model's assets are stored.
    /// This now uses the internal `model_paths` map for an efficient and correct lookup.
    pub fn get_model_version_directory(&self, model_name: &str) -> Result<PathBuf, EngineError> {
        self.model_paths
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Model directory for '{}' not found", model_name))
            })
    }

    /// Leases a model instance from the pool.
    ///
    /// This allows advanced executors to manually check out a model instance
    /// for operations that require exclusive access (like `model.query()`).
    /// The returned `ModelObject` relies on RAII to automatically return the
    /// model to the pool when dropped.
    pub async fn lease_model(
        &self,
        model_name: &str,
        timeout: Duration,
    ) -> Result<ModelObject, EngineError> {
        // 1. Get the MODEL POOL WRAPPER
        let model_pool_wrapper = self
            .models
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Model pool for '{}' not found", model_name))
            })?;

        // 2. Check out a MODEL INSTANCE from the pool
        // This will wait asynchronously if the pool is empty (up to the timeout).
        let model_instance =
            match tokio::time::timeout(timeout, model_pool_wrapper.pool.get()).await {
                Ok(Ok(model)) => model, // `model` is the RAII guard from deadpool
                Ok(Err(e)) => {
                    return Err(EngineError::Prediction(format!(
                        "Failed to get model from pool: {}",
                        e
                    )))
                }
                Err(_) => {
                    return Err(EngineError::Timeout(format!(
                        "Timed out waiting for an available model instance for '{}'",
                        model_name
                    )))
                }
            };
        Ok(model_instance)
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    /// A minimal enabled descriptor: enough to parse and be discovered, with
    /// no backend feature required.
    fn plant(models_dir: &Path, backend: &str, name: &str) {
        let dir = models_dir.join(backend).join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MODEL_CONFIG_FILENAME),
            format!(
                r#"{{"name":"{name}","backend":"generic","enabled":true,
                     "executor":{{"key":"embed-bridge"}},
                     "params":{{"model_type":"embed-bridge"}}}}"#
            ),
        )
        .unwrap();
    }

    fn models_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join(MODELS_DIR_NAME);
        fs::create_dir_all(&models).unwrap();
        (dir, models)
    }

    /// The startup scan must not load a model out of the CLI's private
    /// transaction state: `.staging` holds uncommitted extractions, `.trash`
    /// holds models whose removal is unfinished, `.swap` holds superseded
    /// predecessors. Loading any of them would resurrect or pre-commit a
    /// model the operator never activated.
    #[test]
    fn the_startup_scan_ignores_dot_directories() {
        let (_guard, models) = models_root();
        plant(&models, "onnx-runtime", "real-model");
        plant(&models, ".staging", "uncommitted-model");
        plant(&models, ".trash", "removed-model");
        plant(&models, ".swap/onnx-runtime", "superseded-model");

        let found: Vec<String> = discover_model_configs(&models)
            .unwrap()
            .into_iter()
            .map(|(config, _)| config.name)
            .collect();
        assert_eq!(found, ["real-model"]);
    }

    /// The same rule one level down: a hidden *model* directory under a real
    /// backend is private state too.
    #[test]
    fn the_startup_scan_ignores_hidden_model_directories() {
        let (_guard, models) = models_root();
        plant(&models, "onnx-runtime", "real-model");
        plant(&models, "onnx-runtime", ".partial-model");

        let found: Vec<String> = discover_model_configs(&models)
            .unwrap()
            .into_iter()
            .map(|(config, _)| config.name)
            .collect();
        assert_eq!(found, ["real-model"]);
    }

    /// JIT / admin lookup follows the same rule, or `/admin/load` would be a
    /// way to activate a staged or trashed tree by name.
    #[test]
    fn jit_lookup_does_not_resolve_through_hidden_directories() {
        let (guard, models) = models_root();
        let root = guard.path();
        plant(&models, "onnx-runtime", "real-model");
        plant(&models, ".staging", "uncommitted-model");
        plant(&models, "onnx-runtime", ".partial-model");

        // The ordinary layout still resolves.
        let found = find_model_config_path(root, "real-model").unwrap();
        assert!(found.ends_with("models/onnx-runtime/real-model/ninference.hub.json"));

        for hidden in ["uncommitted-model", ".partial-model"] {
            let err = find_model_config_path(root, hidden).unwrap_err();
            assert!(
                matches!(err, EngineError::NotFound(_)),
                "{hidden} resolved through a hidden directory: {err:?}"
            );
        }
    }

    /// A disabled descriptor is skipped, and a directory without one is not
    /// an error — both pre-existing behaviours the refactor must preserve.
    #[test]
    fn disabled_and_descriptorless_directories_are_skipped_quietly() {
        let (_guard, models) = models_root();
        plant(&models, "onnx-runtime", "real-model");
        fs::create_dir_all(models.join("onnx-runtime/weights-only")).unwrap();
        let disabled = models.join("onnx-runtime/disabled-model");
        fs::create_dir_all(&disabled).unwrap();
        fs::write(
            disabled.join(MODEL_CONFIG_FILENAME),
            r#"{"name":"disabled-model","backend":"generic","enabled":false}"#,
        )
        .unwrap();

        let found: Vec<String> = discover_model_configs(&models)
            .unwrap()
            .into_iter()
            .map(|(config, _)| config.name)
            .collect();
        assert_eq!(found, ["real-model"]);
    }
}

#[cfg(test)]
mod load_lifecycle_tests {
    use super::*;

    fn generic_model(root: &Path, name: &str) {
        generic_model_with(root, name, true, &[]);
    }

    fn generic_model_with(root: &Path, name: &str, enabled: bool, dependencies: &[&str]) {
        let dir = root.join(MODELS_DIR_NAME).join("generic").join(name);
        fs::create_dir_all(&dir).unwrap();
        let dependencies = serde_json::to_string(dependencies).unwrap();
        fs::write(
            dir.join(MODEL_CONFIG_FILENAME),
            format!(
                r#"{{"name":"{name}","backend":"generic","enabled":{enabled},
                     "dependencies":{dependencies},
                     "executor":{{"key":"dummy"}}}}"#
            ),
        )
        .unwrap();
    }

    /// `enabled: false` must mean "do not serve this" on **every** load path,
    /// not only on the startup scan.
    ///
    /// The regression this pins: `discover_model_configs` skips disabled
    /// descriptors, but `build_executors` runs straight afterwards and
    /// JIT-loads each executor's `dependencies` through `load_model`. While
    /// that path ignored `enabled`, deactivating a *dependency* of an enabled
    /// parent survived only until the next restart brought it back — and
    /// `resolver.rebuild` skips disabled configurations, so the model was
    /// resident yet absent from the resolver indices.
    #[test]
    fn a_disabled_model_is_refused_by_every_load_path() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let root = tempfile::tempdir().unwrap();
            generic_model_with(root.path(), "embed-dep", false, &[]);
            generic_model_with(root.path(), "converter", true, &["embed-dep"]);
            let engine = Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
                root_path: root.path().to_path_buf(),
                host_policy: HostPolicy::default(),
            })));

            // Directly, as an explicit load request would.
            let err = engine.load_model("embed-dep").await.unwrap_err();
            assert!(
                format!("{err}").contains("is disabled"),
                "expected a disabled refusal, got: {err:?}"
            );
            assert!(!engine.is_model_loaded("embed-dep"));

            // …and transitively, which is the path a restart actually takes:
            // the scan loads the enabled parent, then the executor build pulls
            // its dependency closure in.
            engine.load_models_and_executors().unwrap();
            assert!(
                engine.is_model_loaded("converter"),
                "the enabled parent is instantiated by the scan"
            );
            engine.build_executors().await.unwrap();
            assert!(
                !engine.is_model_loaded("embed-dep"),
                "a deactivated dependency came back through the executor build"
            );
            // The parent could not be built, so it is dropped rather than left
            // resident and unusable. That is the honest outcome of `deactivate
            // --force`: the dependant fails until the dependency is activated.
            assert!(
                !engine.is_model_ready("converter"),
                "a parent whose dependency is deactivated must not read as ready"
            );
        });
    }

    /// Cancelling after the optimistic pool/path publication must execute
    /// drop-time rollback, not strand a pool that `is_model_loaded()` would
    /// mistake for a usable model. A subsequent load must then succeed.
    #[test]
    fn cancelled_dynamic_load_rolls_back_and_recovers() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let root = tempfile::tempdir().unwrap();
            generic_model(root.path(), "cancel-me");
            let engine = Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
                root_path: root.path().to_path_buf(),
                host_policy: HostPolicy::default(),
            })));

            // Park the load at the publication window so observing it is
            // deterministic — a fast backend otherwise races straight
            // through to readiness between two polls.
            load_test_hooks::HOLD_AT_PUBLICATION.store(true, std::sync::atomic::Ordering::SeqCst);
            let task_engine = engine.clone();
            let load = tokio::spawn(async move { task_engine.load_model("cancel-me").await });
            let observe_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !(engine.is_model_loaded("cancel-me") && !engine.is_model_ready("cancel-me")) {
                assert!(
                    std::time::Instant::now() < observe_deadline,
                    "load never reached the publication window"
                );
                tokio::task::yield_now().await;
            }
            assert!(
                engine.is_model_loaded("cancel-me") && !engine.is_model_ready("cancel-me"),
                "test must reach the mutation-before-executor cancellation window"
            );
            load.abort();
            let _ = load.await;
            load_test_hooks::HOLD_AT_PUBLICATION.store(false, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;

            assert!(!engine.is_model_loaded("cancel-me"));
            assert!(!engine.is_model_ready("cancel-me"));
            assert!(!engine.model_paths.read().unwrap().contains_key("cancel-me"));

            engine
                .load_model("cancel-me")
                .await
                .expect("a clean retry must load the model");
            assert!(engine.is_model_ready("cancel-me"));
        });
    }

    /// A dependency that becomes unready while a load holds the commit gate
    /// must fail typed and fast: re-entering `load_model` from under the
    /// non-reentrant gate would block the task on the mutex it already holds
    /// and permanently wedge every future load in the process.
    #[test]
    fn dependency_gap_under_commit_gate_fails_fast_instead_of_deadlocking() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let engine = Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
                root_path: root.path().to_path_buf(),
                host_policy: HostPolicy::default(),
            })));
            // Reproduce the in-load state: the gate is held and this task is
            // inside the gated phase (scope set). On the pre-fix code the
            // dependency helper re-enters `load_model`, which then awaits
            // the very gate held here — a deadlock this timeout would trip.
            let _gate = engine.model_load_commit.clone().lock_owned().await;
            let cfg = ModelConfiguration {
                name: "parent".into(),
                dependencies: vec!["vanished-dep".into()],
                ..Default::default()
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                LOAD_COMMIT_HELD.scope((), crate::executors::build_dependencies(&engine, &cfg)),
            )
            .await
            .expect("must fail fast, not deadlock on the held commit gate");
            let err = result.expect_err("an unready dependency under the gate must error");
            assert!(
                err.to_string().contains("vanished-dep"),
                "error must name the missing dependency: {err}"
            );
        });
    }
}

#[cfg(test)]
mod admission_tests {
    //! Release-blocker regressions (2026-08 audit): with `admission_limit = 1`
    //! a bridge request's nested component predictions must REUSE the root
    //! logical request's admission permit. The old shape re-entered the gate
    //! per nested call, so the outer bridge held the only permit while its
    //! inner call waited for it — default embedded bridge requests could
    //! never complete.

    use super::*;
    use crate::context::{Context, InputData};
    use crate::executors::bridge_test_support::model_cfg;
    use crate::executors::convert_bridge::ConvertBridgeExecutor;
    use crate::executors::embed_bridge::EmbedBridgeExecutor;
    use crate::executors::{Executor, ExecutorOutput};
    use crate::models::pool::ModelPoolManager;
    use axum::http::HeaderMap;
    use serde_json::json;

    /// Stands in for a native leaf model: returns one canned structured
    /// output, exactly like an embedding/converter executor would.
    struct CannedExecutor(serde_json::Value);
    impl Executor for CannedExecutor {
        fn execute(&self, _ctx: &Context) -> Result<ExecutorOutput, EngineError> {
            Ok(ExecutorOutput::Structured(vec![self.0.clone()]))
        }
    }

    fn engine_with_admission(limit: usize) -> Arc<InferenceEngine> {
        Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
            root_path: PathBuf::from("."),
            host_policy: HostPolicy {
                admission_limit: Some(limit),
                serialized_model_loads: true,
            },
        })))
    }

    /// A leaf model configuration accepting one structured input (the
    /// chained-predict sanity check counts `executor.inputs`).
    fn leaf_cfg(name: &str, params: serde_json::Value) -> ModelConfiguration {
        let mut cfg = model_cfg(name, params);
        cfg.executor.inputs = vec![Default::default()];
        cfg
    }

    /// Register a model pool + executor pair exactly as the load path would,
    /// so `predict_raw` (the REAL entry, admission gate included) can run it.
    fn register(engine: &InferenceEngine, cfg: &ModelConfiguration, exec: Arc<dyn Executor>) {
        let manager = ModelPoolManager {
            config: cfg.clone(),
            config_path: PathBuf::from("."),
            #[cfg(test)]
            test_model: None,
        };
        let pool = deadpool::managed::Pool::builder(manager)
            .max_size(1)
            .build()
            .expect("pool builds");
        let mut template_cfg = cfg.clone();
        let template = instantiate_model_from_config(&mut template_cfg, Path::new("."))
            .expect("generic template instantiates");
        engine.models.write().unwrap().insert(
            cfg.name.clone(),
            Arc::new(ModelPoolWrapper {
                pool,
                template_model: template,
            }),
        );
        engine
            .executors
            .write()
            .unwrap()
            .insert(cfg.name.clone(), exec);
        engine
            .model_paths
            .write()
            .unwrap()
            .insert(cfg.name.clone(), PathBuf::from("."));
    }

    /// Round-7 audit: a host (postvec's embedded loopback server) spends
    /// part of the caller's budget on model loading and payload construction
    /// before reaching the engine. `predict_raw_at` must charge that
    /// preprocessing to the caller's ABSOLUTE deadline — an already-elapsed
    /// deadline fails fast instead of granting a stale rebased budget — and
    /// the refusal must leak no admission capacity.
    #[test]
    fn elapsed_caller_deadline_fails_fast_and_recovers_capacity() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        rt.block_on(async {
            let engine = engine_with_admission(1);
            let leaf = leaf_cfg("leaf", json!({"model_type": "embed", "target_model": "t"}));
            register(&engine, &leaf, Arc::new(CannedExecutor(json!([[1.0]]))));

            let started = std::time::Instant::now();
            let err = engine
                .clone()
                .predict_raw_at(
                    "leaf",
                    InputData::Structured(vec![json!(["x"])]),
                    HeaderMap::new(),
                    std::time::Instant::now(),
                )
                .await
                .err()
                .expect("an elapsed deadline must not execute");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the refusal must be immediate, not a fresh budget"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("Timed out") || msg.contains("deadline"),
                "expected a timeout-shaped refusal, got: {msg}"
            );

            engine
                .clone()
                .predict_raw(
                    "leaf",
                    InputData::Structured(vec![json!(["x"])]),
                    HeaderMap::new(),
                    Duration::from_secs(10),
                )
                .await
                .expect("capacity must recover after the elapsed-deadline refusal");
        });
    }

    #[test]
    fn embed_bridge_completes_with_admission_limit_one() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        rt.block_on(async {
            let engine = engine_with_admission(1);
            let src = leaf_cfg(
                "src-embed",
                json!({"model_type": "embed", "target_model": "mid"}),
            );
            let conv = leaf_cfg(
                "conv-mid-tgt",
                json!({"model_type": "convert", "source_model": "mid", "target_model": "tgt"}),
            );
            register(
                &engine,
                &src,
                Arc::new(CannedExecutor(json!([[0.1, 0.2, 0.3]]))),
            );
            register(
                &engine,
                &conv,
                Arc::new(CannedExecutor(json!([[1.0, 2.0]]))),
            );
            let bridge_cfg = model_cfg("embed-bridge", json!({"model_type": "embed-bridge"}));
            let bridge = EmbedBridgeExecutor::new(&bridge_cfg).expect("bridge builds");
            register(&engine, &bridge_cfg, Arc::new(bridge));
            engine.resolver.rebuild([src, conv].iter());

            let out = engine
                .clone()
                .predict_raw(
                    "embed-bridge",
                    InputData::Structured(vec![json!(["hello"]), json!("mid"), json!("tgt")]),
                    HeaderMap::new(),
                    Duration::from_secs(10),
                )
                .await
                .expect(
                    "embed-bridge must complete under admission_limit = 1 \
                     (nested predictions reuse the root permit)",
                );
            match out {
                ExecutorOutput::StructuredWithUsage { outputs, .. } => {
                    assert_eq!(outputs[0], json!([[1.0, 2.0]]))
                }
                ExecutorOutput::Structured(outputs) => assert_eq!(outputs[0], json!([[1.0, 2.0]])),
                _ => panic!("unexpected output shape"),
            }
        });
    }

    #[test]
    fn convert_bridge_completes_with_admission_limit_one() {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        rt.block_on(async {
            let engine = engine_with_admission(1);
            let c1 = leaf_cfg(
                "conv-src-mid",
                json!({"model_type": "convert", "source_model": "src", "target_model": "mid"}),
            );
            let c2 = leaf_cfg(
                "conv-mid-tgt",
                json!({"model_type": "convert", "source_model": "mid", "target_model": "tgt"}),
            );
            register(&engine, &c1, Arc::new(CannedExecutor(json!([[0.5, 0.5]]))));
            register(&engine, &c2, Arc::new(CannedExecutor(json!([[9.0]]))));
            let bridge_cfg = model_cfg("convert-bridge", json!({"model_type": "convert-bridge"}));
            let bridge = ConvertBridgeExecutor::new(&bridge_cfg).expect("bridge builds");
            register(&engine, &bridge_cfg, Arc::new(bridge));
            engine.resolver.rebuild([c1, c2].iter());

            let out = engine
                .clone()
                .predict_raw(
                    "convert-bridge",
                    InputData::Structured(vec![
                        json!([[1.0, 2.0, 3.0]]),
                        json!("src"),
                        json!("mid"),
                        json!("tgt"),
                    ]),
                    HeaderMap::new(),
                    Duration::from_secs(10),
                )
                .await
                .expect(
                    "convert-bridge must complete under admission_limit = 1 \
                     (nested predictions reuse the root permit)",
                );
            match out {
                ExecutorOutput::Structured(outputs) => assert_eq!(outputs[0], json!([[9.0]])),
                _ => panic!("unexpected output shape"),
            }
        });
    }

    /// Round-3 release-blocker regression: the cancellation contract must
    /// reach a model query made on a RAYON pool thread — the transformer
    /// embedding executor's exact execution shape, where
    /// `Handle::try_current()` fails because Rayon threads carry no tokio
    /// context — terminate the (mock-native) run at the deadline, and release
    /// admission capacity promptly. The mock model mirrors ONNX semantics:
    /// its plain `query()` is a 10 s un-cancellable hang, while
    /// `query_with_deadline` honours a termination signal scheduled on the
    /// bound's runtime, exactly like `run_bounded`'s watchdog.
    #[test]
    fn rayon_query_terminates_at_deadline_and_capacity_recovers() {
        use crate::models::{ModelBackend, ModelOverview, QueryBound};
        use shared::vectors::GenericTensor;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct TerminableModel {
            cfg: ModelConfiguration,
            terminated: Arc<AtomicBool>,
        }
        impl crate::models::Model for TerminableModel {
            fn valid(&self) -> bool {
                true
            }
            fn name(&self) -> &str {
                &self.cfg.name
            }
            fn backend(&self) -> &ModelBackend {
                &self.cfg.backend
            }
            fn configuration(&self) -> &ModelConfiguration {
                &self.cfg
            }
            fn query(
                &self,
                _inputs: &[GenericTensor],
            ) -> Result<Vec<GenericTensor>, crate::models::ModelError> {
                // The unbounded path a missing cancellation contract would
                // take: a long, un-cancellable native computation.
                std::thread::sleep(Duration::from_secs(10));
                Ok(Vec::new())
            }
            fn query_with_deadline(
                &self,
                _inputs: &[GenericTensor],
                bound: Option<&QueryBound>,
            ) -> Result<Vec<GenericTensor>, crate::models::ModelError> {
                match bound {
                    Some(b) => {
                        // Mirror run_bounded: wait for the watchdog moment on
                        // the runtime the bound carries (this thread has no
                        // tokio context of its own), then fail like a
                        // terminated ORT run.
                        b.runtime.block_on(async {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(b.deadline))
                                .await;
                        });
                        self.terminated.store(true, Ordering::SeqCst);
                        Err(crate::models::ModelError::QueryError(
                            "terminated at deadline".into(),
                        ))
                    }
                    None => {
                        std::thread::sleep(Duration::from_secs(10));
                        Ok(Vec::new())
                    }
                }
            }
            fn overview(&self) -> Result<ModelOverview, crate::models::ModelError> {
                Ok(ModelOverview::default())
            }
            fn param_int_or_default(&self, _p: &str, d: i64) -> i64 {
                d
            }
            fn param_f32_or_default(&self, _p: &str, d: f32) -> f32 {
                d
            }
            fn param_bool_or_default(&self, _p: &str, d: bool) -> bool {
                d
            }
            fn param_string_or_default(&self, _p: &str, d: &str) -> String {
                d.to_string()
            }
            fn param_string_list_or_default(&self, _p: &str, d: &[&str]) -> Vec<String> {
                d.iter().map(|s| s.to_string()).collect()
            }
            fn param_list_or_default(
                &self,
                _p: &str,
                d: &[serde_json::Value],
            ) -> Vec<serde_json::Value> {
                d.to_vec()
            }
        }

        /// The transformer-embedding execution shape: fan the query out to a
        /// Rayon pool and call the deadline-aware model API from there.
        struct RayonQueryExecutor;
        impl Executor for RayonQueryExecutor {
            fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
                let model = ctx.model_arc()?.clone();
                let bound = ctx.query_bound();
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .expect("test rayon pool");
                pool.install(|| model.query_with_deadline(&[], Some(&bound)))
                    .map_err(EngineError::Model)?;
                Ok(ExecutorOutput::Structured(vec![json!([[1.0]])]))
            }
        }

        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        rt.block_on(async {
            let engine = engine_with_admission(1);
            let cfg = model_cfg("terminable", json!({}));
            let terminated = Arc::new(AtomicBool::new(false));
            let model = Arc::new(TerminableModel {
                cfg: cfg.clone(),
                terminated: terminated.clone(),
            });
            let manager = ModelPoolManager {
                config: cfg.clone(),
                config_path: PathBuf::from("."),
                test_model: Some(model.clone()),
            };
            let pool = deadpool::managed::Pool::builder(manager)
                .max_size(1)
                .build()
                .expect("pool builds");
            engine.models.write().unwrap().insert(
                cfg.name.clone(),
                Arc::new(ModelPoolWrapper {
                    pool,
                    template_model: model,
                }),
            );
            engine
                .executors
                .write()
                .unwrap()
                .insert(cfg.name.clone(), Arc::new(RayonQueryExecutor));

            let started = std::time::Instant::now();
            let res = engine
                .clone()
                .predict_raw(
                    "terminable",
                    InputData::Structured(vec![]),
                    HeaderMap::new(),
                    Duration::from_millis(400),
                )
                .await;
            let elapsed = started.elapsed();
            assert!(res.is_err(), "the terminated run must surface an error");
            // The caller's outer deadline and the watchdog fire at the SAME
            // instant, so the caller can observe its timeout a moment before
            // the (mock-)native run finishes terminating — exactly like real
            // ORT. Wait briefly for the termination to land.
            let flag_deadline = std::time::Instant::now() + Duration::from_secs(3);
            while !terminated.load(Ordering::SeqCst) && std::time::Instant::now() < flag_deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                terminated.load(Ordering::SeqCst),
                "the cancellation contract crossed the Rayon boundary and \
                 termination fired"
            );
            assert!(
                elapsed < Duration::from_secs(3),
                "the call returned at the deadline, not after the 10 s \
                 unbounded hang (took {elapsed:?})"
            );

            // Capacity recovered promptly: the sole permit admits a fresh
            // request without waiting out any residual native work.
            let res2 = engine
                .clone()
                .predict_raw(
                    "terminable",
                    InputData::Structured(vec![]),
                    HeaderMap::new(),
                    Duration::from_millis(400),
                )
                .await;
            assert!(res2.is_err(), "same terminable model, same outcome");
        });
    }

    /// Capacity recovery: after a root request whose deadline expires while a
    /// (mock) execution is still running, the admission permit is released
    /// when the blocking closure exits — the NEXT request must be admitted.
    #[test]
    fn admission_capacity_recovers_after_a_timed_out_call() {
        struct SlowExecutor;
        impl Executor for SlowExecutor {
            fn execute(&self, _ctx: &Context) -> Result<ExecutorOutput, EngineError> {
                std::thread::sleep(Duration::from_millis(300));
                Ok(ExecutorOutput::Structured(vec![json!([[1.0]])]))
            }
        }
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        rt.block_on(async {
            let engine = engine_with_admission(1);
            let slow = model_cfg("slow", json!({}));
            register(&engine, &slow, Arc::new(SlowExecutor));

            let res = engine
                .clone()
                .predict_raw(
                    "slow",
                    InputData::Structured(vec![]),
                    HeaderMap::new(),
                    Duration::from_millis(50),
                )
                .await;
            let err = match res {
                Ok(_) => panic!("50ms deadline must expire on a 300ms execution"),
                Err(e) => e,
            };
            assert!(format!("{err}").contains("deadline"), "{err}");

            // The permit is held until the native (mock) work exits — wait it
            // out, then the gate must admit a fresh request.
            let out = engine
                .clone()
                .predict_raw(
                    "slow",
                    InputData::Structured(vec![]),
                    HeaderMap::new(),
                    Duration::from_secs(5),
                )
                .await
                .expect("capacity recovers after the timed-out work exits");
            assert!(matches!(out, ExecutorOutput::Structured(_)));
        });
    }
}
