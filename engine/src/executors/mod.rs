//! Executors: request JSON in, tensors through a model, JSON or binary out.

pub(crate) mod convert_bridge;
mod dummy;
pub(crate) mod embed_bridge;
mod embedding_output;
mod passthru;
mod templates;
mod transformer_sequence_embedding;
mod vector_embedding;
use self::{
    convert_bridge::ConvertBridgeExecutor, dummy::DummyExecutor, embed_bridge::EmbedBridgeExecutor,
    passthru::PassThruExecutor, transformer_sequence_embedding::TransformerForSequenceEmbedding,
    vector_embedding::VectorEmbeddingExecutor,
};
use crate::context::Context;
use crate::error::EngineError;
use crate::models::ModelConfiguration;
use crate::InferenceEngine;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub use self::embedding_output::{EmbeddingOutput, SingleBatchOutput};

/// Token counts returned alongside structured output.
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetadata {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// What an executor hands back. Values in `Structured` follow `executor.outputs` order.
pub enum ExecutorOutput {
    Json(Value),
    Structured(Vec<Value>),
    StructuredWithUsage {
        outputs: Vec<Value>,
        usage: ExecutionMetadata,
    },
    Binary {
        data: Vec<u8>,
        content_type: String,
    },
}

/// Load every name in `config.dependencies` from local disk.
pub async fn build_dependencies(
    engine: &InferenceEngine,
    config: &ModelConfiguration,
) -> Result<(), EngineError> {
    if config.dependencies.is_empty() {
        return Ok(());
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
            continue;
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
    Ok(())
}

/// Prepare inputs, run the model, pack the output.
#[async_trait]
pub trait Executor: Send + Sync {
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError>;
    /// Load declared dependencies. A failure unloads this executor and its model.
    async fn build(
        &self,
        engine: &InferenceEngine,
        config: &ModelConfiguration,
    ) -> Result<(), EngineError> {
        build_dependencies(engine, config).await
    }
}

/// Construct the executor named by `config.executor.key` (`passthru` if empty).
pub fn new_executor(
    engine: &InferenceEngine,
    config: &ModelConfiguration,
) -> Result<Arc<dyn Executor>, EngineError> {
    let executor_key = if !config.executor.key.is_empty() {
        config.executor.key.as_str()
    } else {
        "passthru"
    };
    log::debug!("  -> Creating executor with key: '{}'", executor_key);
    match executor_key {
        "passthru" => Ok(Arc::new(PassThruExecutor::new())),
        "dummy" => Ok(Arc::new(DummyExecutor::new())),
        "vector-embedding" => Ok(Arc::new(VectorEmbeddingExecutor::new())),
        "embed-bridge" => Ok(Arc::new(EmbedBridgeExecutor::new(config)?)),
        "convert-bridge" => Ok(Arc::new(ConvertBridgeExecutor::new(config)?)),
        "transformer-sequence-embedding" => Ok(Arc::new(TransformerForSequenceEmbedding::new(
            engine, config,
        )?)),
        _ => Err(EngineError::Configuration(format!(
            "Unknown executor key: '{}'",
            executor_key
        ))),
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
