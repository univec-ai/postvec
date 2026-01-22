// File: engine/src/models/generic_model.rs
//! ## Generic Model
//!
//! This module provides a `GenericModel`, which serves as a placeholder or
//! dummy model implementation. It is useful for scenarios where a model
//! is required by an interface but no actual inference is needed, or for
//! testing purposes. It is the Rust equivalent of `ModelGeneric` from the Go code.
use super::base_model::BaseModel;
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use super::model::Model;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// A placeholder model that fulfills the `Model` trait but performs no operations.
///
/// It embeds a `BaseModel` to inherit common functionality and sets its own
/// backend type. Its primary purpose is to provide a default or fallback
/// model instance.
pub struct GenericModel {
    pub base: BaseModel,
}

impl GenericModel {
    /// Creates a new `GenericModel` with the given configuration.
    ///
    /// The backend of the configuration is explicitly set to `Generic`.
    pub fn new(mut configuration: ModelConfiguration) -> Self {
        configuration.backend = ModelBackend::Generic;
        Self {
            base: BaseModel { configuration },
        }
    }
}

/// The implementation of the `Model` trait for `GenericModel`.
///
/// It delegates most method calls to the embedded `BaseModel` and provides
/// specific implementations where behavior differs.
impl Model for GenericModel {
    /// A `GenericModel` is always considered valid, as it has no external
    /// dependencies or files to load.
    fn valid(&self) -> bool {
        true
    }

    /// Returns the model's name by delegating to the base model.
    fn name(&self) -> &str {
        self.base.name()
    }

    /// Returns the model's backend by delegating to the base model.
    fn backend(&self) -> &ModelBackend {
        self.base.backend()
    }

    /// Returns a reference to the model's configuration.
    fn configuration(&self) -> &ModelConfiguration {
        self.base.configuration()
    }

    /// The `query` method for a `GenericModel` always returns an error,
    /// indicating that it is a non-functional placeholder.
    fn query(&self, _inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        Err(ModelError::QueryError(format!(
            "Model backend for '{}' is a placeholder with no functionality",
            self.name()
        )))
    }

    /// The `overview` method for a `GenericModel` delegates to the base implementation,
    /// which returns a `NotImplemented` error.
    fn overview(&self) -> Result<ModelOverview, ModelError> {
        self.base.overview()
    }

    // --- Parameter retrieval methods are all delegated to the base model ---
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
