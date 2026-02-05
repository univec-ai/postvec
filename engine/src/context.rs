// File: engine/src/context.rs
//! ## Execution Context
//!
//! This module defines the `Context` struct, which encapsulates the state for a
//! single inference request.
//!
//! It provides the `Executor` with a safe and ergonomic
//! API to access the engine, the specific model being executed, and the input data.

// --- Crate-internal Imports ---
use crate::error::EngineError;
use crate::executors::ExecutorOutput;
use crate::input::InputValue;
// Import model-related types from the new internal module.
use crate::models::{Model, ModelConfiguration, ModelOverview, QueryBound};
// Import the pool object type
use crate::models::pool::ModelObject;
// We now import InferenceEngine itself to use it within an Arc.
use crate::InferenceEngine;

// --- External Imports ---
use axum::http::{
    header::{HeaderName, HeaderValue},
    HeaderMap,
};
use serde_json::Value;
// We now use Arc to hold a thread-safe reference to the engine,
// which allows the Context to be 'static.
use shared::vectors::GenericTensor;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

/// Represents the type of input data received in a request.
/// This allows executors to query the context about the nature of the payload.
#[derive(Debug, PartialEq, Eq)]
pub enum InputDataType {
    Json,
    Binary,
    /// Represents a structured list of `Value`s, typically for internal/chained calls.
    Structured,
}

/// A wrapper for the incoming request payload.
///
/// This enum owns the request data, which can be either a deserialized
/// JSON `Value` (as an object), a vector of raw bytes (`Vec<u8>`), or a
/// structured vector of `Value`s.
/// This allows the rest
/// of the engine to be agnostic about the specific format of the data.
pub enum InputData {
    /// A JSON object payload, typically from an external web request.
    #[allow(dead_code)]
    Json(Value),
    /// A raw binary payload (e.g., an image).
    #[allow(dead_code)]
    Binary(Vec<u8>),
    /// A structured list of JSON values, typically from a chained internal call
    /// (e.g., from `ModelHandle::predict`).
    /// The order of values maps directly
    /// to the input indices (e.g., `input_json(0)` maps to `values[0]`).
    #[allow(dead_code)]
    Structured(Vec<Value>),
}

/// A handle to a model retrieved from the engine via the context.
///
/// This provides a safe, scoped way for an executor to interact with other models
/// (e.g., in a pipeline) without needing direct access to the entire engine.
/// It
/// uses a builder pattern for header modification, culminating in a call to
/// `predict` or `predict_raw` that consumes the handle.
// The lifetime 'a has been removed.
pub struct ModelHandle {
    // The engine is now held in an Arc, not a reference.
    // This makes the struct 'static.
    engine: Arc<InferenceEngine>,
    /// This holds the "template" model instance, retrieved from the
    /// engine's `get_model` method. It's used for configuration
    /// and to identify the model to the engine for a chained call.
    model: Arc<dyn Model>,
    /// An owned copy of the headers, inherited from the parent context, which can
    /// be modified before making a chained prediction call.
    headers: HeaderMap,
    /// The ROOT request's absolute deadline, inherited from the parent
    /// context. Chained predictions are clamped to it and never re-enter the
    /// engine's admission gate — the root logical request already holds the
    /// permit (re-acquiring would deadlock at `admission_limit = 1`).
    deadline: std::time::Instant,
    /// The runtime that schedules termination watchdogs (see [`QueryBound`]).
    runtime: tokio::runtime::Handle,
}

// The lifetime 'a has been removed from the impl block.
impl ModelHandle {
    /// Performs inference by calling the `query` method on the model this handle points to.
    /// Note: This method does not consume the handle.
    #[allow(dead_code)]
    pub fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, EngineError> {
        self.model
            .query_with_deadline(inputs, Some(&self.query_bound()))
            .map_err(EngineError::Model)
    }

    /// The cancellation contract executors hand to EVERY model invocation —
    /// including ones made on Rayon pool threads, which have no ambient
    /// tokio context for a watchdog of their own.
    pub fn query_bound(&self) -> QueryBound {
        QueryBound {
            deadline: self.deadline,
            runtime: self.engine_runtime(),
        }
    }

    fn engine_runtime(&self) -> tokio::runtime::Handle {
        self.runtime.clone()
    }

    /// Returns a reference to the configuration of the model this handle points to.
    ///
    /// This is useful when an executor needs to know the output structure of another
    /// model it's calling, for example to extract values by their configured JSON keys.
    #[allow(dead_code)]
    pub fn configuration(&self) -> &ModelConfiguration {
        self.model.configuration()
    }

    /// Adds or updates a header for the subsequent chained prediction call.
    ///
    /// This allows modifying the inherited headers before calling `predict` or `predict_raw`.
    /// Returns `self` to enable a builder-style pattern.
    ///
    /// # Arguments
    /// * `key` - The name of the header (e.g., "Content-Type").
    /// * `value` - The value of the header.
    ///
    /// # Panics
    /// This method will panic if the provided `key` or `value` are not valid HTTP
    /// header strings.
    /// This is a design choice for a simpler builder-pattern API.
    #[allow(dead_code)]
    pub fn add_header(mut self, key: &str, value: &str) -> Self {
        let header_name = HeaderName::from_bytes(key.as_bytes())
            .expect("Invalid header name provided to add_header");
        let header_value =
            HeaderValue::from_str(value).expect("Invalid header value provided to add_header");
        self.headers.insert(header_name, header_value);
        self
    }

    /// Removes a header for the subsequent chained prediction call.
    ///
    /// This allows modifying the inherited headers before calling `predict` or `predict_raw`.
    /// Does nothing if the header does not exist.
    /// Returns `self` to enable a builder-style pattern.
    ///
    /// # Arguments
    /// * `key` - The name of the header to remove.
    #[allow(dead_code)]
    pub fn remove_header(mut self, key: &str) -> Self {
        self.headers.remove(key);
        self
    }

    /// Executes a prediction using raw `InputData`, consuming the handle.
    ///
    /// This method is for advanced use cases where the caller has already constructed
    /// the `InputData` enum.
    /// It uses the headers stored in the handle, which may
    /// have been modified from the original request's headers.
    ///
    /// # Arguments
    /// * `input_data` - The prepared input data, e.g., `InputData::Json`, `InputData::Binary`,
    ///   or `InputData::Structured`.
    /// * `timeout` - The maximum duration to wait for the prediction to complete.
    #[allow(dead_code)]
    pub async fn predict_raw(
        self,
        input_data: InputData,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        // This chained call tells the engine to run the model identified
        // by `self.model.name()`. It is a NESTED prediction: it reuses the
        // root request's admission permit (acquiring another would deadlock
        // at `admission_limit = 1` — the outer bridge holds the only permit
        // while waiting on this call) and is clamped to the root deadline,
        // so a component model cannot outlive the logical request's budget.
        let deadline = std::cmp::min(self.deadline, std::time::Instant::now() + timeout);
        self.engine
            .clone()
            .predict_raw_nested(self.model.name(), input_data, self.headers, deadline)
            .await
    }

    /// Executes a prediction by constructing a structured payload, consuming the handle.
    ///
    /// This convenience method builds an `InputData::Structured` variant from a slice
    /// of `serde_json::Value`s.
    /// This avoids the need for the caller to know the
    /// specific JSON keys defined in the target executor's configuration.
    ///
    /// # Arguments
    /// * `inputs` - A slice of `serde_json::Value`s.
    ///   The order must match the `executor.inputs` order.
    /// * `timeout` - The maximum duration to wait for the prediction to complete.
    ///
    /// # Returns
    /// An `EngineError` if more inputs are provided than the model is configured to accept.
    #[allow(dead_code)]
    pub async fn predict(
        self,
        inputs: &[Value],
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        let target_input_mappings = &self.model.configuration().executor.inputs;
        // Sanity check: ensure the user does not provide more inputs than the model
        // is configured to accept.
        if inputs.len() > target_input_mappings.len() {
            return Err(EngineError::Prediction(format!(
                "Too many inputs for chained prediction on model '{}': Executor accepts at most {} inputs, but {} were provided.",
                self.model.name(),
                target_input_mappings.len(),
                inputs.len()
            )));
        }
        // Create the InputData::Structured variant directly from the input slice.
        // This is much simpler for chained calls, as the caller doesn't need
        // to know the JSON keys.
        let input_data = InputData::Structured(inputs.to_vec());
        // Delegate the actual prediction to `predict_raw`.
        self.predict_raw(input_data, timeout).await
    }
}

/// A context for a single execution, providing access to the engine, model,
/// and input data.
///
/// An instance of `Context` is created for each call to `engine.predict()` and
/// is passed to the `Executor::execute` method.
/// Its methods provide the primary
/// API for an executor to do its work.
// The lifetime 'a has been removed.
pub struct Context {
    /// A thread-safe, reference-counted handle to the parent `InferenceEngine`.
    engine: Arc<InferenceEngine>,
    /// The specific model instance being executed.
    /// This is a `ModelObject` (a "smart pointer" from `deadpool`) that
    /// holds one of the `Arc<dyn Model>` instances from the pool.
    /// When this `Context` is dropped, this object is dropped, and the
    /// model instance is automatically returned to the pool.
    model: ModelObject,
    /// The input data for the current prediction request.
    input_data: InputData,
    /// The HTTP headers from the incoming request.
    headers: HeaderMap,
    /// The ROOT request's absolute deadline. Everything the executor does —
    /// direct queries, chained predictions, sub-model leases — is bounded by
    /// it; nested work receives the REMAINING budget, never a fresh one.
    deadline: std::time::Instant,
    /// The runtime that schedules termination watchdogs (see [`QueryBound`]).
    runtime: tokio::runtime::Handle,
}

// The lifetime 'a has been removed from the impl block.
impl Context {
    /// Creates a new `Context`. This is used internally by the `InferenceEngine`.
    // The signature is updated to take an Arc<InferenceEngine> and a pooled ModelObject.
    pub fn new(
        engine: Arc<InferenceEngine>,
        model: ModelObject,
        input_data: InputData,
        headers: HeaderMap,
        deadline: std::time::Instant,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            engine,
            model,
            input_data,
            headers,
            deadline,
            runtime,
        }
    }

    /// The remaining budget of the ROOT request (zero once the deadline has
    /// passed). Executors pass this — never a fresh constant — to chained
    /// predictions and sub-model work.
    pub fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    // --- Model and Engine Interaction ---
    /// A convenience method to perform inference on the model associated with this context.
    /// This is a shortcut for `ctx.model(ctx.model_name())?.query(inputs)`.
    pub fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, EngineError> {
        // `self.model` is a `ModelObject`, which derefs to `&Arc<dyn Model>`.
        // Deadline-aware: an ONNX Runtime model registers a watchdog (on the
        // bound's runtime) that terminates the run when the root deadline
        // passes.
        self.model
            .query_with_deadline(inputs, Some(&self.query_bound()))
            .map_err(EngineError::Model)
    }

    /// The cancellation contract executors hand to EVERY model invocation —
    /// including ones made on Rayon pool threads, which have no ambient
    /// tokio context for a watchdog of their own.
    pub fn query_bound(&self) -> QueryBound {
        QueryBound {
            deadline: self.deadline,
            runtime: self.runtime.clone(),
        }
    }

    /// Retrieves a handle to another model managed by the engine.
    ///
    /// This allows an executor to chain models together by querying or predicting on
    /// other available models.
    /// The returned handle gets its own copy of the request
    /// headers, which can then be modified.
    #[allow(dead_code)]
    pub fn model(&self, model_name: &str) -> Result<ModelHandle, EngineError> {
        // `self.engine.get_model` now returns the "template" model
        // instance from the pool wrapper, which is what we want here.
        let model = self.engine.get_model(model_name)?;
        Ok(ModelHandle {
            // We clone the Arc, which is cheap (just bumps the ref count).
            engine: self.engine.clone(),
            model, // This is an Arc<dyn Model> (the template)
            // The handle gets its own mutable copy of the headers.
            headers: self.headers.clone(),
            deadline: self.deadline,
            runtime: self.runtime.clone(),
        })
    }

    /// Returns a reference to the `Arc` of the model being executed.
    #[allow(dead_code)]
    pub fn model_arc(&self) -> Result<&Arc<dyn Model>, EngineError> {
        // `self.model` is a `ModelObject`, which derefs to `&Arc<dyn Model>`.
        Ok(self.model.deref())
    }

    /// Leases a different model instance from the engine's pool.
    ///
    /// This is used when an executor needs to run another model as a sub-step
    /// (e.g., an encoder in an encoder-decoder architecture) and needs
    /// exclusive access to it.
    pub async fn lease_model(
        &self,
        model_name: &str,
        timeout: Duration,
    ) -> Result<ModelObject, EngineError> {
        // Never wait past the root deadline for a sub-model lease.
        let timeout = timeout.min(self.remaining());
        self.engine.lease_model(model_name, timeout).await
    }

    // --- Accessors for Model Information ---
    // All these methods work without modification because `self.model`
    // derefs to `&Arc<dyn Model>`, allowing the trait methods to be called.

    /// Returns the name of the model being executed.
    #[allow(dead_code)]
    pub fn model_name(&self) -> &str {
        self.model.name()
    }
    /// Returns a reference to the configuration of the model being executed.
    #[allow(dead_code)]
    pub fn model_configuration(&self) -> &ModelConfiguration {
        self.model.configuration()
    }
    /// Returns the architectural overview (inputs/outputs) of the model being executed.
    #[allow(dead_code)]
    pub fn model_overview(&self) -> Result<ModelOverview, EngineError> {
        self.model.overview().map_err(EngineError::Model)
    }

    // --- Accessors for Input Data ---
    /// Retrieves an input value from the payload by its configured mapping index.
    ///
    /// This is the primary way for an executor to access structured input.
    /// It intelligently handles the `InputData` type:
    ///
    /// - **`InputData::Structured(values)`**:
    ///   It retrieves `values[index]`.
    ///   This is used for chained calls.
    /// - **`InputData::Json(payload)`**:
    ///   It finds the `json_key` from `executor.inputs[index]` and retrieves
    ///   `payload[json_key]`.
    ///   This is used for external API calls.
    /// - **`InputData::Binary`**:
    ///   It returns an error, as this method is for structured data.
    ///
    /// # Arguments
    /// * `index` - The zero-based index of the input mapping in the model's configuration file.
    ///
    /// # Returns
    /// A `Result` containing an `InputValue` helper, which can then be used to
    /// convert the data to the required type (e.g., `ctx.input_json(0)?.as_string()?`).
    #[allow(dead_code)]
    pub fn input_json(&self, index: usize) -> Result<InputValue<'_>, EngineError> {
        match &self.input_data {
            // Handle structured input (e.g., from a chained call)
            InputData::Structured(values) => {
                // For structured input, the index maps directly to the Vec index.
                let value = values.get(index).ok_or_else(|| {
                    EngineError::Configuration(format!(
                        "Attempted to access structured input index {}, but only {} inputs were provided.",
                        index,
                        values.len()
                    ))
                })?;
                Ok(InputValue::new(value))
            }
            // Handle standard JSON object input (e.g., from a web request)
            InputData::Json(json_payload) => {
                // For JSON object input, we use the executor config to map the
                // index to a specific JSON key.
                // `self.model.configuration()` works because `self.model` derefs.
                let executor_config = &self.model_configuration().executor;
                let input_mapping =
                    executor_config.inputs.get(index).ok_or_else(|| {
                        EngineError::Configuration(format!(
                            "Attempted to access input index {}, but only {} inputs are configured for executor.",
                            index,
                            executor_config.inputs.len()
                        ))
                    })?;
                // Find the value in the JSON object using the configured key.
                let value = json_payload.get(&input_mapping.json_key).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Missing required key in input JSON: '{}' for input index {}.",
                        input_mapping.json_key, index
                    ))
                })?;
                Ok(InputValue::new(value))
            }
            // Handle binary input
            InputData::Binary(_) => Err(EngineError::InputTypeError(
                "Request payload is binary, but a JSON or structured input was requested."
                    .to_string(),
            )),
        }
    }

    /// Returns a reference to the raw binary payload, if the request contained binary data.
    #[allow(dead_code)]
    pub fn input_binary(&self) -> Option<&[u8]> {
        match &self.input_data {
            InputData::Binary(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the type of the input data (JSON, Binary, or Structured).
    /// An executor can use this to decide how to process the input payload.
    #[allow(dead_code)]
    pub fn request_type(&self) -> InputDataType {
        match self.input_data {
            InputData::Json(_) => InputDataType::Json,
            InputData::Binary(_) => InputDataType::Binary,
            InputData::Structured(_) => InputDataType::Structured,
        }
    }

    /// Returns a reference to the HTTP headers of the incoming request.
    /// This allows executors to inspect headers like `Content-Type` or `Accept`.
    #[allow(dead_code)]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns a reference to the JSON value if the input is `InputData::Json`.
    /// This returns `None` for `Binary` and `Structured` input types.
    #[allow(dead_code)]
    fn json(&self) -> Option<&Value> {
        match &self.input_data {
            InputData::Json(v) => Some(v),
            _ => None,
        }
    }

    /// Returns a reference to the engine's model resolver.
    pub fn resolver(&self) -> &crate::resolver::ModelResolver {
        &self.engine.resolver
    }
}
