// File: engine/src/models/base_model.rs
//! ## Base Model
//!
//! Provides a `BaseModel` struct that offers a default implementation for
//! common functionalities defined in the `Model` trait. It helps reduce
//! boilerplate code in concrete model implementations by handling configuration
//! access and parameter retrieval.
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use super::model::Model;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// A foundational model struct that holds the configuration and provides
/// default implementations for many `Model` trait methods.
///
/// Concrete model implementations can embed `BaseModel` to inherit this
/// common functionality.
pub struct BaseModel {
    pub configuration: ModelConfiguration,
}

/// Implements the `Model` trait for `BaseModel`. This allows any struct that
/// contains a `BaseModel` to delegate its `Model` trait implementation to it.
impl Model for BaseModel {
    /// The base model is considered invalid by default. Concrete implementations
    /// must provide their own logic to determine validity.
    fn valid(&self) -> bool {
        false
    }

    /// Returns the name of the model from its configuration.
    fn name(&self) -> &str {
        &self.configuration.name
    }

    /// Returns the backend of the model from its configuration.
    fn backend(&self) -> &ModelBackend {
        &self.configuration.backend
    }

    /// Returns a reference to the model's configuration.
    fn configuration(&self) -> &ModelConfiguration {
        &self.configuration
    }

    /// The base query method returns an error, as it has no concrete
    /// inference logic. Implementations must override this.
    fn query(&self, _inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        Err(ModelError::NotImplemented(
            "query".to_string(),
            self.backend().to_string(),
        ))
    }

    /// The base overview method returns an error. Implementations must override this.
    fn overview(&self) -> Result<ModelOverview, ModelError> {
        Err(ModelError::NotImplemented(
            "overview".to_string(),
            self.backend().to_string(),
        ))
    }

    /// Retrieves an integer parameter, falling back to a default if not found or if the
    /// type is incorrect.
    fn param_int_or_default(&self, param: &str, default_value: i64) -> i64 {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_i64())
            .unwrap_or(default_value)
    }

    /// Retrieves a 32-bit float parameter.
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

    /// Retrieves a list of strings.
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

    /// Retrieves a generic list of JSON values.
    fn param_list_or_default(&self, param: &str, default_value: &[Value]) -> Vec<Value> {
        self.configuration
            .params
            .get(param)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_else(|| default_value.to_vec())
    }
}
