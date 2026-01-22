// File: engine/src/models/model.rs
//! ## Model Trait
//!
//! This module defines the central `Model` trait, which establishes a universal
//! contract for all machine learning model implementations within the Baikal
//! ecosystem. It is the Rust equivalent of the `ModelInterface` in the Go code.
//!
//! By conforming to this trait, different model backends (like ONNX Runtime,
//! TensorFlow, etc.) can be used interchangeably.
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// A common interface for all machine learning models.
///
/// This trait provides a standardized API for querying models, accessing their
/// configuration, and retrieving metadata, regardless of the underlying backend.
/// The `Send + Sync` bounds are crucial for ensuring models can be used safely
/// across threads.
pub trait Model: Send + Sync {
    /// Checks if the model is valid and ready for inference.
    ///
    /// A model might be invalid if its files are missing, it failed to load,
    /// or its configuration is incomplete.
    fn valid(&self) -> bool;

    /// Returns the name of the model.
    fn name(&self) -> &str;

    /// Returns the backend used by the model.
    fn backend(&self) -> &ModelBackend;

    /// Returns a reference to the model's configuration.
    fn configuration(&self) -> &ModelConfiguration;

    /// Performs inference using the model.
    ///
    /// # Arguments
    ///
    /// * `inputs` - A slice of `GenericTensor`s, one for each input layer.
    ///
    /// # Returns
    ///
    /// A `Result` containing a `Vec` of `GenericTensor`s (the model's outputs)
    /// or a `ModelError` if inference fails.
    fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError>;

    /// Deadline-aware inference. The default forwards to [`Model::query`]
    /// and is therefore **cooperative/best-effort** — backends without a
    /// native cancellation facility (Candle, generic) cannot interrupt work
    /// mid-run. ONNX Runtime overrides this to terminate the native run when
    /// the deadline passes, so a timed-out caller's capacity (model lease,
    /// engine admission permit) is actually recovered instead of remaining
    /// pinned under a hung or long computation.
    ///
    /// The bound carries an explicit tokio runtime handle because model
    /// queries legitimately run on threads with NO ambient tokio context —
    /// the embedding executor's dedicated Rayon pool in particular — where a
    /// `Handle::try_current()`-based watchdog could never be scheduled.
    fn query_with_deadline(
        &self,
        inputs: &[GenericTensor],
        bound: Option<&QueryBound>,
    ) -> Result<Vec<GenericTensor>, ModelError> {
        let _ = bound;
        self.query(inputs)
    }

    /// Provides an overview of the model's architecture, including input and output layers.
    ///
    /// # Returns
    ///
    /// A `Result` containing the `ModelOverview` or a `ModelError`.
    fn overview(&self) -> Result<ModelOverview, ModelError>;

    /// Retrieves an integer parameter from the model's configuration, with a default fallback.
    fn param_int_or_default(&self, param: &str, default_value: i64) -> i64;
    /// Retrieves a 32-bit float parameter from the model's configuration, with a default fallback.
    fn param_f32_or_default(&self, param: &str, default_value: f32) -> f32;
    /// Retrieves a boolean parameter from the model's configuration, with a default fallback.
    fn param_bool_or_default(&self, param: &str, default_value: bool) -> bool;
    /// Retrieves a string parameter from the model's configuration, with a default fallback.
    fn param_string_or_default(&self, param: &str, default_value: &str) -> String;
    /// Retrieves a list of strings from the model's configuration, with a default fallback.
    fn param_string_list_or_default(&self, param: &str, default_value: &[&str]) -> Vec<String>;
    /// Retrieves a generic list from the model's configuration, with a default fallback.
    fn param_list_or_default(&self, param: &str, default_value: &[Value]) -> Vec<Value>;
}

/// The cancellation contract a root prediction hands to every model
/// invocation in its execution graph: the request's absolute deadline plus
/// the runtime the termination watchdog runs on. Cloneable and `Sync`, so it
/// crosses Rayon pool closures by reference.
#[derive(Debug, Clone)]
pub struct QueryBound {
    /// The ROOT request's absolute deadline.
    pub deadline: std::time::Instant,
    /// The runtime that schedules the deadline watchdog. Carried explicitly:
    /// Rayon worker threads have no ambient tokio context.
    pub runtime: tokio::runtime::Handle,
}
