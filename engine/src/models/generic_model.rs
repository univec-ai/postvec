//! Placeholder model: satisfies the `Model` trait, runs no inference.
use super::base_model::BaseModel;
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use super::model::Model;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// Placeholder used by bridge executors and tests. Always valid; `query` errors.
pub struct GenericModel {
    pub base: BaseModel,
}

impl GenericModel {
    /// Forces `backend` to `Generic` regardless of the incoming config.
    pub fn new(mut configuration: ModelConfiguration) -> Self {
        configuration.backend = ModelBackend::Generic;
        Self {
            base: BaseModel { configuration },
        }
    }
}

impl Model for GenericModel {
    fn valid(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        self.base.name()
    }

    fn backend(&self) -> &ModelBackend {
        self.base.backend()
    }

    fn configuration(&self) -> &ModelConfiguration {
        self.base.configuration()
    }

    /// Placeholder: no inference.
    fn query(&self, _inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        Err(ModelError::QueryError(format!(
            "Model backend for '{}' is a placeholder with no functionality",
            self.name()
        )))
    }

    fn overview(&self) -> Result<ModelOverview, ModelError> {
        self.base.overview()
    }

    fn param_int_or_default(&self, param: &str, default_value: i64) -> i64 {
        self.base.param_int_or_default(param, default_value)
    }
    fn param_f32_or_default(&self, param: &str, default_value: f32) -> f32 {
        self.base.param_f32_or_default(param, default_value)
    }
    fn param_bool_or_default(&self, param: &str, default_value: bool) -> bool {
        self.base.param_bool_or_default(param, default_value)
    }
    fn param_string_or_default(&self, param: &str, default_value: &str) -> String {
        self.base.param_string_or_default(param, default_value)
    }
    fn param_string_list_or_default(&self, param: &str, default_value: &[&str]) -> Vec<String> {
        self.base.param_string_list_or_default(param, default_value)
    }
    fn param_list_or_default(&self, param: &str, default_value: &[Value]) -> Vec<Value> {
        self.base.param_list_or_default(param, default_value)
    }
}
