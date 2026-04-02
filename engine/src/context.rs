//! Per-request state handed to an executor: engine, pooled model and input.

use crate::error::EngineError;
use crate::executors::ExecutorOutput;
use crate::input::InputValue;
use crate::models::pool::ModelObject;
use crate::models::{Model, ModelConfiguration, ModelOverview, QueryBound};
use crate::InferenceEngine;

use axum::http::{
    header::{HeaderName, HeaderValue},
    HeaderMap,
};
use serde_json::Value;
use shared::vectors::GenericTensor;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum InputDataType {
    Json,
    Binary,
    /// Chained / internal call: a list of `Value`s.
    Structured,
}

/// Request payload. Structured values map 1:1 onto input indices.
pub enum InputData {
    #[allow(dead_code)]
    Json(Value),
    #[allow(dead_code)]
    Binary(Vec<u8>),
    #[allow(dead_code)]
    Structured(Vec<Value>),
}

/// Handle to another model for a chained `predict` / `query`.
pub struct ModelHandle {
    engine: Arc<InferenceEngine>,
    /// Template instance: configuration and identity for the chained call.
    model: Arc<dyn Model>,
    headers: HeaderMap,
    /// Root request deadline. Nested predictions are clamped to it and do
    /// not re-enter the admission gate (the root already holds the permit;
    /// re-acquiring would deadlock at `admission_limit = 1`).
    deadline: std::time::Instant,
    /// The runtime that schedules termination watchdogs (see [`QueryBound`]).
    runtime: tokio::runtime::Handle,
}

impl ModelHandle {
    #[allow(dead_code)]
    pub fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, EngineError> {
        self.model
            .query_with_deadline(inputs, Some(&self.query_bound()))
            .map_err(EngineError::Model)
    }

    /// Deadline + watchdog runtime. Needed on Rayon threads, which have no
    /// ambient tokio context.
    pub fn query_bound(&self) -> QueryBound {
        QueryBound {
            deadline: self.deadline,
            runtime: self.engine_runtime(),
        }
    }

    fn engine_runtime(&self) -> tokio::runtime::Handle {
        self.runtime.clone()
    }

    #[allow(dead_code)]
    pub fn configuration(&self) -> &ModelConfiguration {
        self.model.configuration()
    }

    /// Set a header for the next chained call. Panics on an invalid HTTP
    /// header name or value.
    #[allow(dead_code)]
    pub fn add_header(mut self, key: &str, value: &str) -> Self {
        let header_name = HeaderName::from_bytes(key.as_bytes())
            .expect("Invalid header name provided to add_header");
        let header_value =
            HeaderValue::from_str(value).expect("Invalid header value provided to add_header");
        self.headers.insert(header_name, header_value);
        self
    }

    /// Drop a header for the next chained call. No-op if it is absent.
    #[allow(dead_code)]
    pub fn remove_header(mut self, key: &str) -> Self {
        self.headers.remove(key);
        self
    }

    /// Nested prediction with prepared `InputData`. Consumes the handle.
    #[allow(dead_code)]
    pub async fn predict_raw(
        self,
        input_data: InputData,
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        // Nested: reuse the root admission permit (taking another would
        // deadlock at `admission_limit = 1`) and stay inside the root deadline.
        let deadline = std::cmp::min(self.deadline, std::time::Instant::now() + timeout);
        self.engine
            .clone()
            .predict_raw_nested(self.model.name(), input_data, self.headers, deadline)
            .await
    }

    /// Nested prediction from positional JSON values. Order matches
    /// `executor.inputs`. Consumes the handle.
    #[allow(dead_code)]
    pub async fn predict(
        self,
        inputs: &[Value],
        timeout: Duration,
    ) -> Result<ExecutorOutput, EngineError> {
        let target_input_mappings = &self.model.configuration().executor.inputs;
        if inputs.len() > target_input_mappings.len() {
            return Err(EngineError::Prediction(format!(
                "Too many inputs for chained prediction on model '{}': Executor accepts at most {} inputs, but {} were provided.",
                self.model.name(),
                target_input_mappings.len(),
                inputs.len()
            )));
        }
        let input_data = InputData::Structured(inputs.to_vec());
        self.predict_raw(input_data, timeout).await
    }
}

/// One prediction: engine, leased model instance and input.
/// Dropping the context returns the model to the pool.
pub struct Context {
    engine: Arc<InferenceEngine>,
    model: ModelObject,
    input_data: InputData,
    headers: HeaderMap,
    /// Root deadline. Nested work gets the remaining budget, not a fresh one.
    deadline: std::time::Instant,
    /// The runtime that schedules termination watchdogs (see [`QueryBound`]).
    runtime: tokio::runtime::Handle,
}

impl Context {
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

    /// Time left on the root deadline (zero once it has passed). Pass this
    /// to chained work, not a fresh timeout.
    pub fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(std::time::Instant::now())
    }

    pub fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, EngineError> {
        // ONNX Runtime registers a watchdog on the bound's runtime that
        // terminates the run at the root deadline.
        self.model
            .query_with_deadline(inputs, Some(&self.query_bound()))
            .map_err(EngineError::Model)
    }

    /// Deadline + watchdog runtime. Needed on Rayon threads, which have no
    /// ambient tokio context.
    pub fn query_bound(&self) -> QueryBound {
        QueryBound {
            deadline: self.deadline,
            runtime: self.runtime.clone(),
        }
    }

    /// Handle to another model, with a copy of this request's headers.
    #[allow(dead_code)]
    pub fn model(&self, model_name: &str) -> Result<ModelHandle, EngineError> {
        let model = self.engine.get_model(model_name)?;
        Ok(ModelHandle {
            engine: self.engine.clone(),
            model,
            headers: self.headers.clone(),
            deadline: self.deadline,
            runtime: self.runtime.clone(),
        })
    }

    #[allow(dead_code)]
    pub fn model_arc(&self) -> Result<&Arc<dyn Model>, EngineError> {
        Ok(self.model.deref())
    }

    /// Check out a different model from the pool. Wait is capped by the root deadline.
    pub async fn lease_model(
        &self,
        model_name: &str,
        timeout: Duration,
    ) -> Result<ModelObject, EngineError> {
        let timeout = timeout.min(self.remaining());
        self.engine.lease_model(model_name, timeout).await
    }

    /// Name of the model this context is executing.
    #[allow(dead_code)]
    pub fn model_name(&self) -> &str {
        self.model.name()
    }

    #[allow(dead_code)]
    pub fn model_configuration(&self) -> &ModelConfiguration {
        self.model.configuration()
    }

    #[allow(dead_code)]
    pub fn model_overview(&self) -> Result<ModelOverview, EngineError> {
        self.model.overview().map_err(EngineError::Model)
    }

    /// Input by mapping index: `Structured` uses the vec index, `Json`
    /// looks up `executor.inputs[index].json_key`. Binary is an error.
    #[allow(dead_code)]
    pub fn input_json(&self, index: usize) -> Result<InputValue<'_>, EngineError> {
        match &self.input_data {
            InputData::Structured(values) => {
                let value = values.get(index).ok_or_else(|| {
                    EngineError::Configuration(format!(
                        "Attempted to access structured input index {}, but only {} inputs were provided.",
                        index,
                        values.len()
                    ))
                })?;
                Ok(InputValue::new(value))
            }
            InputData::Json(json_payload) => {
                let executor_config = &self.model_configuration().executor;
                let input_mapping =
                    executor_config.inputs.get(index).ok_or_else(|| {
                        EngineError::Configuration(format!(
                            "Attempted to access input index {}, but only {} inputs are configured for executor.",
                            index,
                            executor_config.inputs.len()
                        ))
                    })?;

                let value = json_payload.get(&input_mapping.json_key).ok_or_else(|| {
                    EngineError::Prediction(format!(
                        "Missing required key in input JSON: '{}' for input index {}.",
                        input_mapping.json_key, index
                    ))
                })?;
                Ok(InputValue::new(value))
            }
            InputData::Binary(_) => Err(EngineError::InputTypeError(
                "Request payload is binary, but a JSON or structured input was requested."
                    .to_string(),
            )),
        }
    }

    #[allow(dead_code)]
    pub fn input_binary(&self) -> Option<&[u8]> {
        match &self.input_data {
            InputData::Binary(b) => Some(b),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn request_type(&self) -> InputDataType {
        match self.input_data {
            InputData::Json(_) => InputDataType::Json,
            InputData::Binary(_) => InputDataType::Binary,
            InputData::Structured(_) => InputDataType::Structured,
        }
    }

    #[allow(dead_code)]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    #[allow(dead_code)]
    fn json(&self) -> Option<&Value> {
        match &self.input_data {
            InputData::Json(v) => Some(v),
            _ => None,
        }
    }

    pub fn resolver(&self) -> &crate::resolver::ModelResolver {
        &self.engine.resolver
    }
}
