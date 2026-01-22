// File: engine/src/executors/mod.rs
//! ## Executors
//!
//! This module defines the `Executor` trait and its concrete implementations.
//!
//! An Executor is responsible for translating the input data from an API request
//! into the tensor format required by a model, calling the model's `query` method,
//! and then transforming the output tensors back into a user-friendly format (like JSON).
//!
//! This file acts as the public API for the `executor` module, declaring the
//! individual implementation modules and providing the factory function.
// --- Module Imports & Declarations ---
// Declare the modules for each concrete executor implementation.
// Each file corresponds to a specific executor.
mod dummy;
mod embedding_output; // New module for handling embedding outputs.
mod passthru;
// New module for the sentence embedding executor.
mod transformer_sequence_embedding;
// New module for the vector embedding executor.
pub(crate) mod embed_bridge;
mod vector_embedding;
// Shared helper for input_type-keyed Jinja templates (Snowflake / EmbeddingGemma).
mod templates;
// New modules for the univec model pipeline.
pub(crate) mod convert_bridge;
// Bring the concrete executor types into this module's scope so the factory
// function can use them.
use self::{
    convert_bridge::ConvertBridgeExecutor, dummy::DummyExecutor, embed_bridge::EmbedBridgeExecutor,
    passthru::PassThruExecutor, transformer_sequence_embedding::TransformerForSequenceEmbedding,
    vector_embedding::VectorEmbeddingExecutor,
};
// --- Publicly Exported Items ---
use crate::context::Context;
use crate::error::EngineError;
// Import ModelConfiguration from the new internal module path.
use crate::models::ModelConfiguration;
use crate::InferenceEngine;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

// Publicly export the new output handling structs for use in executors.
pub use self::embedding_output::{EmbeddingOutput, SingleBatchOutput};

// Output metadata for tracking usage.
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetadata {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// An enum to represent the output of an executor.
///
/// This allows an executor to produce either a structured JSON `Value` or
/// raw binary data with an associated MIME type (e.g., "image/png").
/// The server will use this to construct the appropriate HTTP response.
pub enum ExecutorOutput {
    /// Represents a complete, custom-built JSON response.
    /// The executor is responsible for creating the entire `serde_json::Value` object.
    Json(Value),
    /// Represents a structured list of JSON values to be automatically mapped
    /// to the output keys defined in the model's configuration file.
    /// The order
    /// of values in the `Vec` must match the order of the `executor.outputs` array.
    Structured(Vec<Value>),
    /// Represents a structured list of JSON values with associated metadata (e.g. token usage).
    StructuredWithUsage {
        outputs: Vec<Value>,
        usage: ExecutionMetadata,
    },
    /// Represents a binary response.
    Binary {
        /// The raw byte data of the response.
        data: Vec<u8>,
        /// The IANA media type (MIME type) of the data, e.g., "image/png",
        /// "application/octet-stream", etc.
        content_type: String,
    },
}
/// A reusable helper function that contains the default logic for building model dependencies.
///
/// Any executor can call this function at the beginning of its own `build` implementation
/// to ensure that all models listed in the `dependencies` array of its configuration
/// are downloaded and loaded into the engine.
pub async fn build_dependencies(
    engine: &InferenceEngine,
    config: &ModelConfiguration,
) -> Result<(), EngineError> {
    // If the model has no dependencies, there's nothing to do.
    if config.dependencies.is_empty() {
        return Ok(());
        //
    }
    log::info!(
        "Model '{}' has dependencies, checking/loading them now: {:?}...",
        config.name,
        config.dependencies
    );
    // Process each dependency declared in the model's configuration.
    for dep_name in &config.dependencies {
        // A pool without its executor is not usable readiness. This can only
        // be observed transiently while another load owns the commit gate;
        // falling through lets `load_model` wait and re-check atomically.
        if engine.is_model_ready(dep_name) {
            log::debug!("  -> Dependency '{}' is already ready.", dep_name);
            //
            continue;
            //
        }
        // Re-entering `load_model` from under the load-commit gate would
        // block this task on the non-reentrant mutex it already holds and
        // permanently wedge all future loads. `load_model` pre-loads the
        // full dependency list before taking the gate, so reaching this
        // branch under the gate means a dependency was unloaded in between —
        // fail typed and let the caller retry the whole load. (Called
        // without the gate — startup's `build_executors` — the JIT path
        // below remains available.)
        if crate::load_commit_held() {
            return Err(EngineError::Configuration(format!(
                "Dependency '{}' of model '{}' became unavailable while its \
                 load held the commit gate (unloaded after dependency \
                 resolution?); retry the load.",
                dep_name, config.name
            )));
        }
        log::info!(
            "  -> Dependency '{}' is not loaded. Loading it from disk.",
            dep_name
        );
        // Dependency files are resolved from local disk; the `load_model`
        // call below fails if they are not present.
        log::debug!(
            "  -> Dependency '{}' is resolved from local disk.",
            dep_name
        );
        // Now, instruct the engine to load the model from disk.
        engine.load_model(dep_name).await.map_err(|e| {
            EngineError::Configuration(format!(
                "Failed to load dependency model '{}': {}",
                dep_name, e
            ))
        })?;
        log::info!(
            "  -> Successfully loaded dependency '{}' into engine.",
            dep_name
        );
    }
    log::info!("All dependencies for model '{}' are loaded.", config.name);
    //
    Ok(()) //
}
/// A trait defining the contract for all executors.
///
/// Each model is associated with an executor that handles the logic of
/// preparing inputs, invoking the model, and processing outputs.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Executes the prediction logic for the model.
    ///
    /// # Arguments
    /// * `ctx` - The execution context, containing accessors for the model, input data, and engine.
    ///
    /// # Returns
    /// A `Result` containing the `ExecutorOutput` or an `EngineError`.
    /// This allows the executor to return either JSON or binary data.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError>;
    /// Asynchronously prepares the executor by ensuring all declared model dependencies are loaded.
    ///
    /// This method is called by the `InferenceEngine` during the startup sequence or during
    /// just-in-time loading, before the server accepts requests for this model.
    /// It reads the
    /// `dependencies` list from the model's configuration.
    /// For each dependency, it checks
    /// if the model is loaded in the engine.
    /// If not, it instructs the engine to
    /// load the model into memory from local disk.
    ///
    /// # Arguments
    /// * `engine` - A reference to the `InferenceEngine` to check for and load models.
    /// * `config` - The configuration of the model associated with this executor.
    ///
    /// # Returns
    /// An `Ok(())` if all dependencies are successfully loaded.
    /// If an `Err` is returned, the
    /// `InferenceEngine` will unload this executor and its associated model.
    async fn build(
        &self,
        engine: &InferenceEngine,
        config: &ModelConfiguration,
    ) -> Result<(), EngineError> {
        // The default implementation now delegates to the shared helper function.
        // This allows custom build implementations to easily reuse the dependency logic.
        build_dependencies(engine, config).await
    }
}
/// A factory function to create a concrete `Executor` instance based on the
/// model's configuration.
///
/// This acts as a router, calling the appropriate constructor for the
/// specified executor type.
pub fn new_executor(
    engine: &InferenceEngine,
    config: &ModelConfiguration,
) -> Result<Arc<dyn Executor>, EngineError> {
    // Determine the executor key from the model configuration, defaulting to
    // "passthru" if it's not explicitly specified.
    let executor_key = if !config.executor.key.is_empty() {
        config.executor.key.as_str()
    } else {
        "passthru"
    };
    log::debug!("  -> Creating executor with key: '{}'", executor_key); //
                                                                        // Match the key and instantiate the corresponding executor.
    match executor_key {
        // Each arm of the match now calls the specific constructor for that executor.
        "passthru" => Ok(Arc::new(PassThruExecutor::new())), //
        "dummy" => Ok(Arc::new(DummyExecutor::new())),
        "vector-embedding" => Ok(Arc::new(VectorEmbeddingExecutor::new())),
        "embed-bridge" => Ok(Arc::new(EmbedBridgeExecutor::new(config)?)),
        "convert-bridge" => Ok(Arc::new(ConvertBridgeExecutor::new(config)?)),
        "transformer-sequence-embedding" => Ok(Arc::new(TransformerForSequenceEmbedding::new(
            engine, config,
        )?)),
        // If the key is unknown, return a configuration error.
        _ => Err(EngineError::Configuration(format!(
            "Unknown executor key: '{}'",
            executor_key
        ))), //
    }
}

/// Shared harness for bridge-executor resolution tests: run a real
/// `execute()` call the way the engine does (inside `spawn_blocking` on a
/// multi-thread runtime, the executor's own generic model leased from a real
/// deadpool pool) against an engine whose resolver indexes only `loaded`.
/// Only the model inventory is synthetic — the failure being tested is
/// exactly the resolution step a production request would hit.
#[cfg(test)]
pub(crate) mod bridge_test_support {
    use crate::config::EngineConfig;
    use crate::context::{Context, InputData};
    use crate::error::EngineError;
    use crate::executors::{Executor, ExecutorOutput};
    use crate::models::pool::ModelPoolManager;
    use crate::models::ModelConfiguration;
    use crate::InferenceEngine;
    use axum::http::HeaderMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// An `enabled` model configuration (the resolver skips disabled ones).
    pub(crate) fn model_cfg(name: &str, params: serde_json::Value) -> ModelConfiguration {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "enabled": true,
            "params": params,
        }))
        .expect("test model configuration parses")
    }

    pub(crate) fn execute_against_resolver(
        executor: impl Executor + 'static,
        executor_cfg: &ModelConfiguration,
        loaded: &[ModelConfiguration],
        inputs: Vec<serde_json::Value>,
    ) -> Result<ExecutorOutput, EngineError> {
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let engine = Arc::new(InferenceEngine::new(Arc::new(EngineConfig {
            root_path: PathBuf::from("."),
            host_policy: Default::default(),
        })));
        engine.resolver.rebuild(loaded.iter());
        let manager = ModelPoolManager {
            config: executor_cfg.clone(),
            config_path: PathBuf::from("."),
            #[cfg(test)]
            test_model: None,
        };
        let pool = deadpool::managed::Pool::builder(manager)
            .max_size(1)
            .build()
            .expect("pool builds");
        rt.block_on(async move {
            let obj = pool.get().await.expect("generic model instantiates");
            let ctx = Context::new(
                engine,
                obj,
                InputData::Structured(inputs),
                HeaderMap::new(),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
                tokio::runtime::Handle::current(),
            );
            tokio::task::spawn_blocking(move || executor.execute(&ctx))
                .await
                .expect("executor task completes")
        })
    }
}
