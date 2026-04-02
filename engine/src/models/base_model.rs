//! Shared `Model` defaults: configuration access and typed parameter lookups.
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use super::model::Model;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// Configuration holder. Concrete backends embed this and override `valid`,
/// `query` and `overview`.
pub struct BaseModel {
    pub configuration: ModelConfiguration,
}

impl Model for BaseModel {
    /// Invalid until a concrete backend says otherwise.
    fn valid(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        &self.configuration.name
    }

    fn backend(&self) -> &ModelBackend {
        &self.configuration.backend
    }

    fn configuration(&self) -> &ModelConfiguration {
        &self.configuration
    }

    fn query(&self, _inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        Err(ModelError::NotImplemented(
            "query".to_string(),
            self.backend().to_string(),
        ))
    }

    fn overview(&self) -> Result<ModelOverview, ModelError> {
        Err(ModelError::NotImplemented(
            "overview".to_string(),
            self.backend().to_string(),
        ))
    }

    fn param_int_or_default(&self, param: &str, default_value: i64) -> i64 {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_i64())
            .unwrap_or(default_value)
    }

    fn param_f32_or_default(&self, param: &str, default_value: f32) -> f32 {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_f64())
            .map(|f| f as f32)
            .unwrap_or(default_value)
    }

    /// Retrieves a boolean parameter.
    fn param_bool_or_default(&self, param: &str, default_value: bool) -> bool {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_bool())
            .unwrap_or(default_value)
    }

    /// Retrieves a string parameter.
    fn param_string_or_default(&self, param: &str, default_value: &str) -> String {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| default_value.to_string())
    }

    fn param_string_list_or_default(&self, param: &str, default_value: &[&str]) -> Vec<String> {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| default_value.iter().map(|&s| s.to_string()).collect())
    }

    fn param_list_or_default(&self, param: &str, default_value: &[Value]) -> Vec<Value> {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| default_value.to_vec())
    }
}
