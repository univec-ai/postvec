use crate::models::{configuration::ModelConfiguration, error::ModelError, model::Model};
use crate::{instantiate_model_from_config, EngineError};

use std::future::Future;

use deadpool::managed::{Manager, Metrics, Object, RecycleResult};
use std::path::PathBuf;
use std::sync::Arc;

/// Factory for pooled `Model` instances.
pub struct ModelPoolManager {
    pub config: ModelConfiguration,
    pub config_path: PathBuf,
    /// Test seam: when set, the pool hands out clones of this model instead
    /// of instantiating from configuration — the only way unit tests can
    /// drive the REAL predict path (admission, deadline, watchdog) against a
    /// scripted model. Compiled out of release builds.
    #[cfg(test)]
    pub test_model: Option<Arc<dyn Model>>,
}

impl Manager for ModelPoolManager {
    type Type = Arc<dyn Model>;
    type Error = EngineError;

    /// Creates a new `Model` instance for the pool.
    fn create(&self) -> impl Future<Output = Result<Self::Type, Self::Error>> + Send {
        log::debug!("Creating new model instance for pool: {}", self.config.name);

        #[cfg(test)]
        let test_model = self.test_model.clone();
        #[cfg(not(test))]
        let test_model: Option<Arc<dyn Model>> = None;

        let mut config_clone = self.config.clone();
        let path_clone = self.config_path.clone();

        async move {
            if let Some(model) = test_model {
                return Ok(model);
            }
            tokio::task::spawn_blocking(move || {
                instantiate_model_from_config(&mut config_clone, &path_clone)
            })
            .await
            .map_err(|e| EngineError::Anyhow(e.into()))?
        }
    }

    fn recycle(
        &self,
        obj: &mut Self::Type,
        _metrics: &Metrics,
    ) -> impl Future<Output = RecycleResult<Self::Error>> + Send {
        let is_valid = obj.valid();

        async move {
            if !is_valid {
                return Err(EngineError::Model(ModelError::InvalidModel(
                    "Model in pool reported as invalid".to_string(),
                ))
                .into());
            }
            Ok(())
        }
    }
}

pub type ModelObject = Object<ModelPoolManager>;
pub type ModelPool = deadpool::managed::Pool<ModelPoolManager>;
