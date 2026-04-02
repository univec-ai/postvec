//! Model lifecycle, executors and prediction for the inference engine.

pub mod config;
pub mod context;
pub mod error;
pub mod executors;
pub mod input;
pub mod models;
pub mod pooling;
pub mod resolver;
pub mod tokenizers;
pub mod utils;
use crate::context::Context;
use crate::models::pool::{ModelObject, ModelPool, ModelPoolManager};
use crate::models::ExecutionProvider;
#[cfg(feature = "onnx")]
use anyhow::anyhow;
use axum::http::HeaderMap;
pub use config::{set_session_thread_policy, EngineConfig, HostPolicy, SessionThreadPolicy};
pub use context::InputData;
pub use error::EngineError;
pub use executors::{new_executor, Executor, ExecutorOutput};
#[cfg(feature = "onnx")]
pub use models::OnnxRuntimeModel;
pub use models::{
    ExecutorConfiguration, GenericModel, InputMapping, LayerOverview, Model, ModelBackend,
    ModelConfiguration, ModelError, ModelOverview, OutputMapping,
};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;
pub use tokenizers::{
    config as TokenizerConfig, error::TokenizerError, new_tokenizer, Tokenizer,
    TransformerEncodingsWithPosition,
};
use tokio::task;
const MODELS_DIR_NAME: &str = "models";
const MODEL_CONFIG_FILENAME: &str = "ninference.hub.json";
/// Pool of model instances for concurrent inference, plus one template
/// used only for metadata (`overview`, `configuration`).
pub struct ModelPoolWrapper {
    pub pool: ModelPool,
    pub template_model: Arc<dyn Model>,
}

#[cfg(feature = "onnx")]
mod onnx_initializer {
    use super::*;

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
    /// Load ONNX Runtime from `root_path/libs`. Call once before any `ort` use.
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
        ort::init_from(lib_path.to_string_lossy())
            .commit()
            .map_err(|e| anyhow!("Failed to initialize ONNX Runtime: {}", e))?;
        log::info!("ONNX Runtime library loaded and initialized successfully.");
        Ok(())
    }
}
#[cfg(feature = "onnx")]
pub use onnx_initializer::initialize_onnx;

#[cfg(not(feature = "onnx"))]
pub fn initialize_onnx(_root_path: &Path) -> anyhow::Result<()> {
    log::warn!("ONNX feature is not enabled. Skipping ONNX Runtime initialization.");
    Ok(())
}
/// Loaded models, executors and the prediction path.
pub struct InferenceEngine {
    models: RwLock<HashMap<String, Arc<ModelPoolWrapper>>>,
    executors: RwLock<HashMap<String, Arc<dyn Executor>>>,
    model_paths: RwLock<HashMap<String, PathBuf>>,
    config: Arc<EngineConfig>,
    pub resolver: Arc<crate::resolver::ModelResolver>,
    /// Cap on concurrent native execution. `None` = unlimited.
    admission: Option<Arc<tokio::sync::Semaphore>>,
    /// Serializes publication (path + pool insert), executor build and
    /// resolver rebuild. Discovery and dependency loads happen first.
    /// `HostPolicy::serialized_model_loads` also holds this across native
    /// instantiation so a cancelled caller cannot start a second native load.
    /// Not re-entrant: `LOAD_COMMIT_HELD` makes `build_dependencies` fail
    /// typed instead of deadlocking on the same task.
    model_load_commit: Arc<tokio::sync::Mutex<()>>,
}

tokio::task_local! {
    /// Set while `load_model` holds the commit gate. Re-entering `load_model`
    /// here would wait on the mutex this task already holds.
    static LOAD_COMMIT_HELD: ();
}

/// True while this task holds the load-commit gate.
pub(crate) fn load_commit_held() -> bool {
    LOAD_COMMIT_HELD.try_with(|_| ()).is_ok()
}

/// Test hook: park a load after pool publication, before the executor is ready.
#[cfg(test)]
pub(crate) mod load_test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// When set, `load_model` yields after optimistic publication so a test
    /// can cancel the published-but-not-ready window.
    pub static HOLD_AT_PUBLICATION: AtomicBool = AtomicBool::new(false);

    pub async fn pause_at_publication() {
        while HOLD_AT_PUBLICATION.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    }
}

/// Undo path/pool/executor inserts if the `load_model` future is dropped
/// (timeout or cancel never run the statements after the current `.await`).
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
/// Build one model from config. Called from blocking threads and the pool factory.
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
/// Directories whose names start with `.` are private (CLI staging, trash,
/// swap) and must not be loaded. A restart would otherwise serve an
/// uncommitted or half-removed model. Non-UTF-8 names are skipped because
/// nothing can address them.
fn is_scannable_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| !name.starts_with('.'))
}

/// Two-level scan of `models/<backend>/<model>/ninference.hub.json`.
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

/// Locate `ninference.hub.json` for `model_name` under `root_path/models`.
fn find_model_config_path(root_path: &Path, model_name: &str) -> Result<PathBuf, EngineError> {
    let models_dir = root_path.join(MODELS_DIR_NAME);
    if !models_dir.is_dir() {
        return Err(EngineError::Configuration(
            "Models directory not found.".to_string(),
        ));
    }

    for backend_entry in fs::read_dir(models_dir)? {
        let backend_path = backend_entry?.path();
        if backend_path.is_dir() && is_scannable_dir(&backend_path) {
            for model_entry in fs::read_dir(backend_path)? {
                let model_path = model_entry?.path();

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
    /// Empty engine. Load models in a later step.
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
    /// Empty engine plus JIT load of each named model (and its dependencies).
    pub async fn default_with_models(
        config: Arc<EngineConfig>,
        model_names: &[String],
    ) -> Result<Self, EngineError> {
        let engine = Self::new(config);
        log::info!(
            "Initializing engine with a specific set of models: {:?}",
            model_names
        );

        for model_name in model_names {
            log::info!("=> Attempting to load and build '{}'...", model_name);
            engine.load_model(model_name).await?;
        }
        log::info!("Successfully loaded and built all specified models and their dependencies.");
        Ok(engine)
    }
    /// Pool size from `executor.params.pool_size`, else GPU default 4 or CPU 1.
    fn get_pool_size(&self, config: &ModelConfiguration) -> usize {
        const DEFAULT_GPU_POOL_SIZE: usize = 4;
        const DEFAULT_CPU_POOL_SIZE_FACTOR: usize = 1;
        if let Some(val) = config.executor.params.get("pool_size") {
            if let Some(size) = val.as_u64() {
                if size > 0 {
                    log::info!("Found 'pool_size: {}' in model configuration.", size);
                    return size as usize;
                }
            }
        }

        log::warn!(
            "No 'pool_size' found in config for {}. Calculating default...",
            config.name
        );

        let is_gpu_requested = config
            .execution_providers
            .iter()
            .any(|ep| matches!(ep, ExecutionProvider::Cuda | ExecutionProvider::TensorRt));
        // GPU default only if the matching compile-time feature is on.
        let is_gpu_available = is_gpu_requested && {
            #[cfg(all(feature = "onnx", any(feature = "ort-cuda", feature = "ort-tensorrt")))]
            let onnx_gpu_enabled = config.backend == ModelBackend::OnnxRuntime;
            #[cfg(not(all(
                feature = "onnx",
                any(feature = "ort-cuda", feature = "ort-tensorrt")
            )))]
            let onnx_gpu_enabled = false;
            onnx_gpu_enabled
        };
        if is_gpu_available {
            log::info!("Using default GPU pool size: {}", DEFAULT_GPU_POOL_SIZE);
            DEFAULT_GPU_POOL_SIZE
        } else {
            let cpu_size = DEFAULT_CPU_POOL_SIZE_FACTOR;
            if is_gpu_requested {
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
    /// Startup scan: discover configs, build pools, then executors.
    pub fn load_models_and_executors(&self) -> Result<(), EngineError> {
        let models_dir = self.config.root_path.join(MODELS_DIR_NAME);
        log::info!("Scanning for models in: {}", models_dir.display());
        if !models_dir.exists() || !models_dir.is_dir() {
            log::warn!("Models directory not found. No models will be loaded.");
            return Ok(());
        }

        let configs_to_process = discover_model_configs(&models_dir)?;
        log::info!(
            "Found {} model configuration file(s) to process.",
            configs_to_process.len()
        );

        let mut loaded_model_configs = Vec::new();
        {
            let mut models_map = self.models.write().unwrap();
            let mut paths_map = self.model_paths.write().unwrap();
            for (mut config, config_path) in configs_to_process {
                let model_name = config.name.clone();
                match (|| -> Result<(), EngineError> {
                    let template_model = self.instantiate_model(&mut config, &config_path)?;
                    if !template_model.valid() {
                        log::warn!("  Model '{}' is invalid and will be skipped.", model_name);
                        return Ok(());
                    }

                    let pool_size = self.get_pool_size(&config);
                    log::info!(
                        "-> Instantiated template model '{}' (pool size: {})",
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
                            EngineError::Configuration(format!(
                                "Failed to create model pool: {}",
                                e
                            ))
                        })?;

                    let pool_wrapper = Arc::new(ModelPoolWrapper {
                        pool,
                        template_model,
                    });

                    models_map.insert(model_name.clone(), pool_wrapper);

                    if let Some(model_dir) = config_path.parent() {
                        paths_map.insert(model_name.clone(), model_dir.to_path_buf());
                    }
                    loaded_model_configs.push(config);
                    Ok(())
                })() {
                    Err(e) => {
                        log::error!("  Failed to instantiate model '{}': {}", model_name, e)
                    }
                    Ok(_) => {}
                }
            }
        }
        log::info!(
            "Model instantiation complete. {} valid model(s) loaded.",
            loaded_model_configs.len()
        );

        {
            let models_map = self.models.read().unwrap();
            let configs = models_map
                .values()
                .map(|wrapper| wrapper.template_model.configuration());
            self.resolver.rebuild(configs);
        }

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
    /// Load one model and its dependencies. Failures (and dropped futures)
    /// roll back any partial publication.
    pub async fn load_model(&self, model_name: &str) -> Result<(), EngineError> {
        // A cancelled earlier load may have left a pool without an executor;
        // fall through so this attempt can heal it.
        if self.is_model_ready(model_name) {
            log::debug!(
                "Attempted to load model '{}', but it is already loaded.",
                model_name
            );
            return Ok(());
        }

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
        // `enabled: false` is "do not serve this" on every load path,
        // including JIT dependency loads from `build_executors`. The scan
        // already skips disabled descriptors; without this check a disabled
        // dependency of an enabled parent would come back on restart.
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

        let model_dir = config_path.parent().ok_or_else(|| {
            EngineError::Configuration(format!(
                "Could not determine parent directory for config path '{}'",
                config_path.display()
            ))
        })?;
        let model_dir = model_dir.to_path_buf();
        // Nothing is published until dependencies are ready.
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
                    // Box the recursive future so the type is finite.
                    Box::pin(self.load_model(dep_name)).await?;
                    log::info!("Successfully loaded dependency '{}'.", dep_name);
                }
            }

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

        // Instantiate before publishing. Gate vs native work is
        // `serialized_model_loads`; the ready re-check always runs under the
        // gate in case another caller finished first.
        let mut config_clone = config.clone();
        let config_path_clone = config_path.clone();
        let (_load_permit, template_model) = if self.config.host_policy.serialized_model_loads {
            // Shared host: gate first, then move the permit into the
            // blocking task so cancel cannot start a second native load.
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
            // Standalone: instantiate outside the gate so different models
            // can cold-start in parallel. Cancel here has published nothing.
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
        // Mark the gated region so `build_dependencies` fails typed instead
        // of re-entering `load_model` and deadlocking.
        let result: Result<(), EngineError> = LOAD_COMMIT_HELD
            .scope((), async {
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
                // Publish path + pool so the executor constructor can see this
                // model. The armed guard removes both on error, panic or drop.
                self.model_paths
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), model_dir);

                self.models
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), pool_wrapper);
                rollback.armed = true;

                // Test-only park: published but not yet ready.
                #[cfg(test)]
                load_test_hooks::pause_at_publication().await;

                let executor = new_executor(self, &config)?;

                executor.build(self, &config).await?;

                self.executors
                    .write()
                    .unwrap()
                    .insert(model_name.to_string(), executor);
                // `unload_model` is sync and can run while this build is in
                // flight. If it removed the pool, fail so rollback runs.
                if !self.is_model_ready(model_name) {
                    return Err(EngineError::Configuration(format!(
                        "Model '{}' was unloaded while its executor was being built.",
                        model_name
                    )));
                }
                Ok(())
            })
            .await;
        // `_load_permit` is declared before `rollback`, so drop order is
        // rollback first, then the next load is admitted. Do not rebind it
        // below `rollback`.
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
    /// Structured prediction from positional JSON values (`self: Arc<Self>`
    /// so the context can hold the engine).
    pub async fn predict(
        self: Arc<Self>,
        model_name: &str,
        inputs: &[Value],
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        log::debug!("Executing prediction for model '{}'", model_name);
        let model = self.get_model(model_name)?;
        let target_input_mappings = model.configuration().executor.inputs.clone();
        if inputs.len() > target_input_mappings.len() {
            return Err(EngineError::Prediction(format!(
                "Too many inputs for chained prediction on model '{}': Executor accepts at most {} inputs, but {} were provided.",
                model_name,
                target_input_mappings.len(),
                inputs.len()
            )));
        }
        let input_data = InputData::Structured(inputs.to_vec());
        self.predict_raw(model_name, input_data, headers, timeout)
            .await
    }
    pub async fn predict_raw(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        // One deadline for admission wait, pool wait, execution and nested
        // bridge predictions.
        let deadline = std::time::Instant::now() + timeout;
        self.predict_raw_at(model_name, input_data, headers, deadline)
            .await
    }

    /// Same as [`Self::predict_raw`] but the caller supplies the absolute
    /// deadline. Hosts that already spent part of the budget (load, validate)
    /// must pass that instant; rebasing a `Duration` here would give the
    /// preprocessing time back. An elapsed deadline fails immediately and
    /// leaks no admission capacity.
    pub async fn predict_raw_at(
        self: Arc<Self>,
        model_name: &str,
        input_data: InputData,
        headers: HeaderMap,
        deadline: std::time::Instant,
    ) -> Result<ExecutorOutput, EngineError> {
        // Refuse up front: `timeout_at` polls the inner future before the
        // timer, so a free permit or warm pool could otherwise run work
        // the caller has already given up on.
        if deadline <= std::time::Instant::now() {
            return Err(EngineError::Timeout(format!(
                "deadline elapsed before engine admission for '{model_name}'"
            )));
        }
        // Root requests only. Pools bound same-model concurrency; different
        // models could still all enter blocking execution and oversubscribe a
        // shared host. The permit moves into the blocking closure so a timed
        // out caller does not free capacity while native work is still
        // running. Nested predictions use [`Self::predict_raw_nested`].
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

    /// Nested prediction from a bridge component. Reuses the root admission
    /// permit (taking another would deadlock at `admission_limit = 1`) and
    /// the root deadline.
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
        let model_pool_wrapper = self
            .models
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Model pool for '{}' not found", model_name))
            })?;
        let executor = self.get_executor_for_model(model_name)?;
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
        let context = Context::new(
            self.clone(),
            model_instance,
            input_data,
            headers,
            deadline_std,
            // Watchdog runtime: captured here; Rayon threads have none.
            tokio::runtime::Handle::current(),
        );
        let blocking_task = task::spawn_blocking(move || {
            // Dropping a timed-out JoinHandle does not stop this closure.
            // The lease and admission permit stay held until native work
            // actually returns.
            let _admission_permit = admission_permit;
            executor.execute(&context)
        });
        // Remaining budget bounds the blocking task.
        match tokio::time::timeout_at(deadline, blocking_task).await {
            Err(_) => Err(EngineError::Timeout(format!(
                "Prediction for model '{}' passed its deadline",
                model_name,
            ))),
            Ok(join_result) => match join_result {
                Err(join_error) => Err(EngineError::Anyhow(join_error.into())),
                Ok(inner_result) => match inner_result {
                    Ok(result) => {
                        log::debug!(
                            "Prediction for model '{}' completed successfully.",
                            model_name
                        );
                        Ok(result)
                    }
                    Err(e) => Err(e),
                },
            },
        }
    }
    /// Template instance for metadata. Do not `query()` this under load;
    /// use `predict_raw` so the pool is used.
    pub fn get_model(&self, model_name: &str) -> Result<Arc<dyn Model>, EngineError> {
        self.models
            .read()
            .unwrap()
            .get(model_name)
            .map(|pool_wrapper| pool_wrapper.template_model.clone())
            .ok_or_else(|| EngineError::NotFound(format!("Model '{}' not found", model_name)))
    }
    /// Pool is registered. Does not mean the model can serve yet.
    pub fn is_model_loaded(&self, model_name: &str) -> bool {
        self.models.read().unwrap().contains_key(model_name)
    }
    /// Pool and executor are both registered. The optimistic publication
    /// window is not serve-ready.
    pub fn is_model_ready(&self, model_name: &str) -> bool {
        self.models.read().unwrap().contains_key(model_name)
            && self.executors.read().unwrap().contains_key(model_name)
    }

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

    pub fn get_active_models(&self) -> Vec<String> {
        self.models.read().unwrap().keys().cloned().collect()
    }
    /// On-disk directory for this model's assets.
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

    /// Check out a model instance. Dropping the guard returns it to the pool.
    pub async fn lease_model(
        &self,
        model_name: &str,
        timeout: Duration,
    ) -> Result<ModelObject, EngineError> {
        let model_pool_wrapper = self
            .models
            .read()
            .unwrap()
            .get(model_name)
            .cloned()
            .ok_or_else(|| {
                EngineError::NotFound(format!("Model pool for '{}' not found", model_name))
            })?;

        let model_instance =
            match tokio::time::timeout(timeout, model_pool_wrapper.pool.get()).await {
                Ok(Ok(model)) => model,
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

    /// Minimal enabled descriptor, no backend feature required.
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

    /// Dot-directories are private CLI state (staging, trash, swap) and
    /// must not be loaded.
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

    /// Hidden model directories under a real backend are private too.
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

    /// JIT lookup follows the same rule, or a staged/trashed tree could be
    /// activated by name.
    #[test]
    fn jit_lookup_does_not_resolve_through_hidden_directories() {
        let (guard, models) = models_root();
        let root = guard.path();
        plant(&models, "onnx-runtime", "real-model");
        plant(&models, ".staging", "uncommitted-model");
        plant(&models, "onnx-runtime", ".partial-model");

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

    /// Disabled descriptors and descriptor-less directories are skipped.
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

    /// `enabled: false` applies to JIT dependency loads, not only the scan.
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

            let err = engine.load_model("embed-dep").await.unwrap_err();
            assert!(
                format!("{err}").contains("is disabled"),
                "expected a disabled refusal, got: {err:?}"
            );
            assert!(!engine.is_model_loaded("embed-dep"));

            // Restart path: scan loads the parent, then build pulls deps.
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
            // Parent cannot be built, so it is dropped rather than left
            // resident and unusable.
            assert!(
                !engine.is_model_ready("converter"),
                "a parent whose dependency is deactivated must not read as ready"
            );
        });
    }

    /// Cancel after optimistic publication must roll back; a retry must load.
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

            // Park at publication so a fast backend cannot skip the window.
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

    /// An unready dependency under the commit gate must fail fast, not
    /// re-enter `load_model` and deadlock.
    #[test]
    fn dependency_gap_under_commit_gate_fails_fast_instead_of_deadlocking() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let engine = Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
                root_path: root.path().to_path_buf(),
                host_policy: HostPolicy::default(),
            })));
            // Gate held, this task inside the gated phase.
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
    //! With `admission_limit = 1`, nested bridge predictions reuse the
    //! root admission permit. Taking another would deadlock.

    use super::*;
    use crate::context::{Context, InputData};
    use crate::executors::bridge_test_support::model_cfg;
    use crate::executors::convert_bridge::ConvertBridgeExecutor;
    use crate::executors::embed_bridge::EmbedBridgeExecutor;
    use crate::executors::{Executor, ExecutorOutput};
    use crate::models::pool::ModelPoolManager;
    use axum::http::HeaderMap;
    use serde_json::json;

    /// Leaf model stand-in: one canned structured output.
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

    /// Leaf config with one structured input (chained-predict counts them).
    fn leaf_cfg(name: &str, params: serde_json::Value) -> ModelConfiguration {
        let mut cfg = model_cfg(name, params);
        cfg.executor.inputs = vec![Default::default()];
        cfg
    }

    /// Register pool + executor as the load path would, so `predict_raw` runs.
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

    /// An already-elapsed deadline fails fast and leaks no admission capacity.
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

    /// A query on a Rayon thread (no tokio context) still terminates at the
    /// deadline and recovers admission capacity. The mock hangs for 10s on
    /// plain `query`; `query_with_deadline` honours the bound's watchdog.
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
                // Unbounded path: long un-cancellable native work.
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
                        // Wait on the bound's runtime, then fail like a
                        // terminated ORT run. This thread has no tokio context.
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

        /// Fan the query onto a Rayon pool, as the embedding executor does.
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
