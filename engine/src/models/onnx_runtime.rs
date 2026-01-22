// File: engine/src/models/onnx_runtime.rs
//! ## ONNX Runtime Model
//!
//! This module provides the `OnnxRuntimeModel`, a concrete implementation of the
//! `Model` trait for running inference with ONNX models using the `ort` crate.
//!
//! It supports configurable execution providers for CPU and GPU inference.
use super::base_model::BaseModel;
use super::configuration::{
    ExecutionProvider, LayerOverview, ModelBackend, ModelConfiguration, ModelOverview,
};
use super::error::ModelError;
use super::model::{Model, QueryBound};

// Import components from the `ort` crate with their updated paths for version 2.0.
use ort::{
    execution_providers::CPUExecutionProvider,
    session::{builder::GraphOptimizationLevel, Session},
    tensor::TensorElementType,
    // `Value` is the dynamically-typed tensor value we will work with.
    value::{Value, ValueType},
};

// Conditionally import GPU execution providers only when the corresponding features are enabled.
// This resolves the parsing error by separating the conditional imports.
#[cfg(feature = "ort-cuda")]
use ort::execution_providers::CUDAExecutionProvider;
#[cfg(feature = "ort-tensorrt")]
use ort::execution_providers::TensorRTExecutionProvider;

use shared::vectors::{GenericTensor, TensorDataType, TensorValue};
// We re-introduce the Mutex for interior mutability.
use std::sync::Mutex;

/// A model implementation that uses the ONNX Runtime for inference.
///
/// This struct holds the ONNX session and the model's configuration.
/// It is designed to be thread-safe and can be shared across multiple requests.
pub struct OnnxRuntimeModel {
    /// Provides default implementations for common `Model` trait methods.
    pub base: BaseModel,
    /// The ONNX Runtime session, which contains the loaded model and is used for inference.
    /// `ort::Session::run` requires `&mut self`, so we wrap the `Session` in a `Mutex`
    /// to allow for interior mutability, enabling inference calls from an immutable
    /// `&OnnxRuntimeModel` context as required by the `Model` trait.
    session: Mutex<Session>,
    /// A pre-computed overview of the model's input and output layers.
    overview: ModelOverview,
}

impl OnnxRuntimeModel {
    /// The shared inference body behind `query`/`query_with_deadline`.
    fn run_bounded(
        &self,
        inputs: &[GenericTensor],
        bound: Option<&QueryBound>,
    ) -> Result<Vec<GenericTensor>, ModelError> {
        if inputs.len() != self.overview.inputs.len() {
            return Err(ModelError::QueryError(format!(
                "Input count mismatch: model expects {}, but {} were provided.",
                self.overview.inputs.len(),
                inputs.len()
            )));
        }

        // Convert our `GenericTensor`s into `ort::value::Value`s.
        let ort_values: Vec<Value> = inputs
            .iter()
            .map(|tensor| -> Result<Value, ModelError> {
                match &tensor.value {
                    // Create a statically-typed Value from the ndarray, handle potential errors
                    // with `?`, and then call `.into()` to convert it into a dynamically-typed Value.
                    TensorValue::Float32(arr) => Ok(Value::from_array(arr.clone())?.into()),
                    TensorValue::Int64(arr) => Ok(Value::from_array(arr.clone())?.into()),
                    _ => Err(ModelError::QueryError(
                        "Unsupported GenericTensor value type for ONNX conversion.".into(),
                    )),
                }
            })
            .collect::<Result<Vec<_>, ModelError>>()?;

        // Create named inputs for the `ort` crate's `run` method.
        // The `ort` v2 API
        // prefers named inputs in a `Vec` of (name, value) tuples.
        // We zip the
        // input names from our pre-computed model overview with the converted tensors.
        let named_inputs: Vec<(String, Value)> = self
            .overview
            .inputs
            .iter()
            .map(|layer_overview| layer_overview.name.clone())
            .zip(ort_values)
            .collect();

        // Lock the session to get mutable access for the `run` call.
        // Recover from poisoning rather than panicking: ort's `run` reports
        // native failures as `Result` and does not unwind through the C ABI,
        // so a panic while the lock is held can only originate in Rust-side
        // output conversion *after* the native run returned — the session
        // itself is still consistent. Propagating the poison would instead
        // turn one conversion panic into a permanently dead pooled instance
        // that nothing evicts.
        let mut session_guard = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Execute inference. With a deadline, the run carries `RunOptions`
        // and a watchdog task (on the ambient tokio runtime) calls
        // `terminate()` when the deadline passes — onnxruntime then stops at
        // the next node boundary and the call returns an error, releasing
        // the model lease and any admission permit. The watchdog is aborted
        // on normal completion. `RunOptions<NoSelectedOutputs>` is
        // Send + Sync, so sharing it with the watchdog is sound.
        // The RunOptions must outlive the returned SessionOutputs (the ort
        // API ties their lifetimes), so it is hoisted out of the branch.
        let options = match bound {
            Some(_) => Some(std::sync::Arc::new(
                ort::session::RunOptions::new()
                    .map_err(|e| ModelError::QueryError(format!("RunOptions: {e}")))?,
            )),
            None => None,
        };
        // The watchdog runs on the runtime the bound CARRIES — this thread
        // may be a Rayon worker (the embedding executor's pool) with no
        // ambient tokio context, where `Handle::try_current()` would fail
        // and silently degrade to an unbounded run.
        let watchdog = match (bound, &options) {
            (Some(b), Some(options)) => {
                let opts = options.clone();
                let deadline = b.deadline;
                Some(b.runtime.spawn(async move {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
                    let _ = opts.terminate();
                }))
            }
            _ => None,
        };
        let run_result = match &options {
            Some(options) => session_guard
                .run_with_options(named_inputs, &**options)
                .map_err(|e| {
                    ModelError::QueryError(format!(
                        "inference failed or was terminated at its deadline: {e}"
                    ))
                }),
            None => session_guard.run(named_inputs).map_err(ModelError::from),
        };
        if let Some(w) = watchdog {
            w.abort();
        }
        let ort_outputs = run_result?;

        // Convert the `ort::value::Value`s from `SessionOutputs` back into our `GenericTensor`.
        let generic_outputs = ort_outputs
            .into_iter()
            .map(|(_name, ort_value)| -> Result<GenericTensor, ModelError> {
                // `query()` converts *every* model output, including ones a given
                // executor won't select (e.g. a quantised `pooler_output_int8`
                // sitting next to a float `pooler_output`). `TensorValue` only
                // carries Float32/Int64, so each ONNX element type is widened into
                // one of those: float-like -> Float32, integer/bool -> Int64.
                // Downstream embedding extraction already casts Int64 -> f32
                // (shared::vectors::tensor), so a selected int8 output still
                // yields a correct vector.
                macro_rules! try_extract {
                    ($t:ty => $variant:ident as $cast:ty) => {
                        if let Ok(view) = ort_value.try_extract_array::<$t>() {
                            let shape: Vec<usize> = view.shape().to_vec();
                            return Ok(GenericTensor {
                                value: TensorValue::$variant(view.mapv(|v| v as $cast).into_dyn()),
                                shape,
                                dtype: TensorDataType::Numeric,
                            });
                        }
                    };
                }

                try_extract!(f32 => Float32 as f32);
                try_extract!(f64 => Float32 as f32);
                try_extract!(i64 => Int64 as i64);
                try_extract!(i32 => Int64 as i64);
                try_extract!(i16 => Int64 as i64);
                try_extract!(i8 => Int64 as i64);
                try_extract!(u8 => Int64 as i64);
                try_extract!(u16 => Int64 as i64);
                try_extract!(u32 => Int64 as i64);
                try_extract!(u64 => Int64 as i64);

                Err(ModelError::QueryError(
                    "Unsupported output tensor data type from ONNX Runtime.".into(),
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(generic_outputs)
    }

    /// Creates a new `OnnxRuntimeModel` from a given configuration.
    ///
    /// This function initializes the ONNX Runtime session, configures execution
    /// providers (for CPU/GPU), and inspects the model to build an overview of its
    /// inputs and outputs.
    ///
    /// # Arguments
    /// * `configuration` - The configuration for this model instance.
    ///
    /// # Returns
    /// A `Result` containing the new `OnnxRuntimeModel` or a `ModelError` if initialization fails.
    pub fn new(mut configuration: ModelConfiguration) -> Result<Self, ModelError> {
        // Ensure the backend type is correctly set.
        configuration.backend = ModelBackend::OnnxRuntime;
        // Ensure the model file path is specified in the configuration.
        let model_path = configuration.file_path.as_ref().ok_or_else(|| {
            ModelError::ConfigurationError(
                "Model file_path is required for ONNX Runtime".to_string(),
            )
        })?;

        // Begin building the session using the `ort` 2.0 builder pattern.
        let mut session_builder = Session::builder()?;

        // Configure execution providers based on the model's configuration.
        if !configuration.execution_providers.is_empty() {
            // This vector will hold the providers that are both requested
            // AND supported by the current build configuration.
            let mut available_providers = Vec::new();
            // We'll store the names of the *added* providers for logging.
            let mut available_provider_names = Vec::new();
            log::info!(
                "Configuring execution providers for model '{}': {:?}",
                configuration.name,
                configuration.execution_providers
            );
            for ep in &configuration.execution_providers {
                // We match on the requested provider and conditionally
                // add it to our list only if the corresponding cargo
                // feature is enabled.
                match ep {
                    ExecutionProvider::Cpu => {
                        // CPU is always available.
                        log::debug!("  -> Adding CPU execution provider.");
                        available_providers.push(CPUExecutionProvider::default().build());
                        available_provider_names.push("CPU");
                    }
                    ExecutionProvider::Cuda => {
                        // This arm is only compiled if the `ort-cuda` feature is enabled.
                        #[cfg(feature = "ort-cuda")]
                        {
                            log::debug!("  -> Adding CUDA execution provider.");
                            available_providers.push(CUDAExecutionProvider::default().build());
                            available_provider_names.push("CUDA");
                        }
                        // This arm is compiled if the `ort-cuda` feature is NOT enabled.
                        // Instead of erroring, we log a warning and skip this provider.
                        #[cfg(not(feature = "ort-cuda"))]
                        {
                            log::warn!(
                                "  -> CUDA execution provider was requested, but the application was not compiled with the 'ort-cuda' feature. Skipping."
                            );
                        }
                    }
                    ExecutionProvider::TensorRt => {
                        // This arm is only compiled if the `ort-tensorrt` feature is enabled.
                        #[cfg(feature = "ort-tensorrt")]
                        {
                            log::debug!("  -> Adding TensorRT execution provider.");
                            available_providers.push(TensorRTExecutionProvider::default().build());
                            available_provider_names.push("TensorRT");
                        }
                        // This arm is compiled if the `ort-tensorrt` feature is NOT enabled.
                        // We log a warning and skip this provider.
                        #[cfg(not(feature = "ort-tensorrt"))]
                        {
                            log::warn!(
                                "  -> TensorRT execution
 provider was requested, but this application
                                   was not compiled with the 'ort-tensorrt' feature. Skipping."
                            );
                        }
                    }
                }
            }
            // Only call `with_execution_providers` if we have at least one
            // available provider.
            // If the list is empty (e.g., config
            // only specified "cuda" but feature was off), we do nothing
            // and let `ort` use its default (which is typically CPU).
            if !available_providers.is_empty() {
                log::info!(
                    "  -> Final execution provider list: {:?}",
                    available_provider_names
                );
                session_builder = session_builder.with_execution_providers(available_providers)?;
            } else {
                log::warn!(
                    "  -> No requested execution providers are available in this build.
                       Falling back to ONNX Runtime default (CPU)."
                );
            }
        }

        // Apply a high level of graph optimization for performance.
        session_builder =
            session_builder.with_optimization_level(GraphOptimizationLevel::Level3)?;

        // Host threading policy (opt-in, process-global): a shared host — the
        // postvec embedded launcher living inside a PostgreSQL cluster — sets
        // this before loading models to bound each session's intra-op pool
        // and disable onnxruntime's spin-wait (which otherwise burns CPU
        // after every inference on however many threads × sessions exist).
        // Standalone ninference never sets it, keeping onnxruntime's
        // throughput-oriented defaults.
        if let Some(policy) = crate::config::session_thread_policy() {
            log::info!(
                "Applying host session-thread policy to '{}': intra_op_threads={}, \
                 spinning_disabled={}",
                configuration.name,
                policy.intra_op_threads,
                policy.disable_spinning
            );
            session_builder = session_builder.with_intra_threads(policy.intra_op_threads.max(1))?;
            if policy.disable_spinning {
                session_builder =
                    session_builder.with_config_entry("session.intra_op.allow_spinning", "0")?;
            }
        }

        // Create the session by loading the model from the specified file.
        let session = session_builder.commit_from_file(model_path)?;

        // Inspect the model's inputs and outputs to build an overview.
        let overview = Self::get_model_overview(&session)?;

        Ok(Self {
            base: BaseModel { configuration },
            // Wrap the created session in the Mutex.
            session: Mutex::new(session),
            overview,
        })
    }

    /// Inspects a loaded session to extract details about its input and output layers.
    ///
    /// # Arguments
    /// * `session` - A reference to the `ort::Session`.
    ///
    /// # Returns
    /// A `Result` containing the `ModelOverview` or a `ModelError`.
    fn get_model_overview(session: &Session) -> Result<ModelOverview, ModelError> {
        // Map over the session's inputs to create a `LayerOverview` for each.
        let inputs = session
            .inputs
            .iter()
            .map(|input| {
                // The `ValueType::Tensor` struct has a `dimension_symbols` field which we ignore.
                if let ValueType::Tensor { ty, shape, .. } = &input.input_type {
                    Ok(LayerOverview {
                        name: input.name.clone(),
                        shape: shape.iter().copied().collect(),
                        data_type: ort_type_to_i32(*ty)?,
                    })
                } else {
                    Err(ModelError::ConfigurationError(format!(
                        "Model input '{}' is not a tensor, which is unsupported.",
                        input.name
                    )))
                }
            })
            .collect::<Result<Vec<_>, ModelError>>()?;

        // Do the same for the session's outputs.
        let outputs = session
            .outputs
            .iter()
            .map(|output| {
                if let ValueType::Tensor { ty, shape, .. } = &output.output_type {
                    Ok(LayerOverview {
                        name: output.name.clone(),
                        shape: shape.iter().copied().collect(),
                        data_type: ort_type_to_i32(*ty)?,
                    })
                } else {
                    Err(ModelError::ConfigurationError(format!(
                        "Model output '{}' is not a tensor, which is unsupported.",
                        output.name
                    )))
                }
            })
            .collect::<Result<Vec<_>, ModelError>>()?;

        Ok(ModelOverview { inputs, outputs })
    }
}

impl Model for OnnxRuntimeModel {
    /// An ONNX model is considered valid if its session was successfully created.
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

    /// Performs inference using the loaded ONNX model.
    ///
    /// # Arguments
    /// * `inputs` - A slice of `GenericTensor`s, one for each input layer.
    ///
    /// # Returns
    /// A `Result` containing a `Vec` of `GenericTensor`s (the outputs) or a `ModelError`.
    fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        self.run_bounded(inputs, None)
    }

    /// Deadline-aware inference: the run is executed with an `ort`
    /// `RunOptions` whose `terminate()` a watchdog fires when the deadline
    /// passes, so a hung or overlong native computation returns an error and
    /// releases its model lease / admission permit instead of pinning them
    /// indefinitely. Without a reachable tokio runtime the watchdog cannot be
    /// scheduled and the call degrades to the unbounded run.
    fn query_with_deadline(
        &self,
        inputs: &[GenericTensor],
        bound: Option<&QueryBound>,
    ) -> Result<Vec<GenericTensor>, ModelError> {
        self.run_bounded(inputs, bound)
    }

    /// Returns the pre-computed model overview.
    fn overview(&self) -> Result<ModelOverview, ModelError> {
        Ok(self.overview.clone())
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
    fn param_list_or_default(
        &self,
        param: &str,
        default_value: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        self.base.param_list_or_default(param, default_value)
    }
}

/// Helper function to convert an `ort::tensor::TensorElementType` to the i32 representation
/// used in `LayerOverview`.
fn ort_type_to_i32(dtype: TensorElementType) -> Result<i32, ModelError> {
    // We can convert the `ort` enum to its `ort-sys` counterpart to get the integer value.
    // The returned i32 is the standard ONNX `TensorProto.DataType` code. It is
    // descriptive metadata only — `LayerOverview.data_type` is never branched on
    // anywhere in the workspace — so it is safe (and correct) to map the full set
    // rather than rejecting tensors whose element type we simply weren't listing.
    use ort_sys::ONNXTensorElementDataType as T;
    let sys_type: ort_sys::ONNXTensorElementDataType = dtype.into();
    match sys_type {
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT => Ok(1),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_UINT8 => Ok(2),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_INT8 => Ok(3),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_UINT16 => Ok(4),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_INT16 => Ok(5),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_INT32 => Ok(6),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_INT64 => Ok(7),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_STRING => Ok(8),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_BOOL => Ok(9),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT16 => Ok(10),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_DOUBLE => Ok(11),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_UINT32 => Ok(12),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_UINT64 => Ok(13),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_COMPLEX64 => Ok(14),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_COMPLEX128 => Ok(15),
        T::ONNX_TENSOR_ELEMENT_DATA_TYPE_BFLOAT16 => Ok(16),
        // Exotic / not-yet-standardised codes (UNDEFINED, FP8, INT4, …): keep the
        // explicit error so an unexpected type is surfaced rather than mislabelled.
        _ => Err(ModelError::NotImplemented(
            format!("Conversion for ONNX data type {:?} to i32", dtype),
            "onnx-runtime".to_string(),
        )),
    }
}
