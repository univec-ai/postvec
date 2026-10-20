//! ONNX Runtime backend: session, execution providers and deadline-aware query.
use super::base_model::BaseModel;
use super::configuration::{
    ExecutionProvider, LayerOverview, ModelBackend, ModelConfiguration, ModelOverview,
};
use super::error::ModelError;
use super::model::{Model, QueryBound};

use ort::{
    execution_providers::CPUExecutionProvider,
    session::{builder::GraphOptimizationLevel, Session},
    tensor::TensorElementType,
    value::{Value, ValueType},
};

#[cfg(feature = "ort-cuda")]
use ort::execution_providers::CUDAExecutionProvider;
#[cfg(feature = "ort-tensorrt")]
use ort::execution_providers::TensorRTExecutionProvider;

use shared::vectors::{GenericTensor, TensorDataType, TensorValue};
use std::sync::Mutex;

/// Loaded ONNX model. The session sits behind a mutex because `Session::run`
/// needs `&mut self` while `Model::query` is called on `&self`.
pub struct OnnxRuntimeModel {
    pub base: BaseModel,
    session: Mutex<Session>,
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

        let ort_values: Vec<Value> = inputs
            .iter()
            .map(|tensor| -> Result<Value, ModelError> {
                match &tensor.value {
                    TensorValue::Float32(arr) => Ok(Value::from_array(arr.clone())?.into()),
                    TensorValue::Int64(arr) => Ok(Value::from_array(arr.clone())?.into()),
                    _ => Err(ModelError::QueryError(
                        "Unsupported GenericTensor value type for ONNX conversion.".into(),
                    )),
                }
            })
            .collect::<Result<Vec<_>, ModelError>>()?;

        // Named (name, value) pairs, as the ort v2 `run` API expects.
        let named_inputs: Vec<(String, Value)> = self
            .overview
            .inputs
            .iter()
            .map(|layer_overview| layer_overview.name.clone())
            .zip(ort_values)
            .collect();

        // Recover from a poisoned mutex. `run` returns native failures as
        // `Result` and does not unwind through the C ABI, so a panic under
        // the lock is Rust-side conversion after the session is consistent.
        // Leaving the instance poisoned would kill a pooled model nothing evicts.
        let mut session_guard = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // With a deadline, a watchdog on the bound's runtime calls `terminate()`
        // so onnxruntime stops at the next node and the lease comes back.
        // Abort it if the run finishes first. `RunOptions` must outlive
        // `SessionOutputs` (ort ties their lifetimes), so it is hoisted.
        let options = match bound {
            Some(_) => Some(std::sync::Arc::new(
                ort::session::RunOptions::new()
                    .map_err(|e| ModelError::QueryError(format!("RunOptions: {e}")))?,
            )),
            None => None,
        };
        // Schedule the watchdog on the runtime the bound carries. This
        // thread may be a Rayon worker with no ambient tokio context, and
        // `Handle::try_current()` would leave the run unbounded.
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

        let generic_outputs = ort_outputs
            .into_iter()
            .map(|(_name, ort_value)| -> Result<GenericTensor, ModelError> {
                // Convert every output, including ones this executor will not
                // select. `TensorValue` only holds Float32 and Int64, so
                // float-like types widen to Float32 and integer or bool types
                // widen to Int64. Embedding extraction casts Int64 to f32.
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

    /// Load the ONNX file, set execution providers and cache the layer overview.
    pub fn new(mut configuration: ModelConfiguration) -> Result<Self, ModelError> {
        configuration.backend = ModelBackend::OnnxRuntime;

        let model_path = configuration.file_path.as_ref().ok_or_else(|| {
            ModelError::ConfigurationError(
                "Model file_path is required for ONNX Runtime".to_string(),
            )
        })?;

        let mut session_builder = Session::builder()?;

        // Keep only providers this build actually compiled in.
        if !configuration.execution_providers.is_empty() {
            let mut available_providers = Vec::new();

            let mut available_provider_names = Vec::new();
            log::info!(
                "Configuring execution providers for model '{}': {:?}",
                configuration.name,
                configuration.execution_providers
            );
            for ep in &configuration.execution_providers {
                match ep {
                    ExecutionProvider::Cpu => {
                        log::debug!("  -> Adding CPU execution provider.");
                        available_providers.push(CPUExecutionProvider::default().build());
                        available_provider_names.push("CPU");
                    }
                    ExecutionProvider::Cuda => {
                        #[cfg(feature = "ort-cuda")]
                        {
                            log::debug!("  -> Adding CUDA execution provider.");
                            available_providers.push(CUDAExecutionProvider::default().build());
                            available_provider_names.push("CUDA");
                        }
                        // Feature off: skip rather than fail the whole load.
                        #[cfg(not(feature = "ort-cuda"))]
                        {
                            log::warn!(
                                "  -> CUDA execution provider was requested, but the application was not compiled with the 'ort-cuda' feature. Skipping."
                            );
                        }
                    }
                    ExecutionProvider::TensorRt => {
                        #[cfg(feature = "ort-tensorrt")]
                        {
                            log::debug!("  -> Adding TensorRT execution provider.");
                            available_providers.push(TensorRTExecutionProvider::default().build());
                            available_provider_names.push("TensorRT");
                        }
                        // Feature off: skip rather than fail the whole load.
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
            // Empty list (e.g. config asked for CUDA, feature off): leave
            // ort at its default, typically CPU.
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

        session_builder =
            session_builder.with_optimization_level(GraphOptimizationLevel::Level3)?;

        // A shared host sets this before the first load to bound each
        // session's intra-op pool and turn off spin-wait. Standalone nodes
        // leave the defaults.
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

        let session = session_builder.commit_from_file(model_path)?;

        let overview = Self::get_model_overview(&session)?;

        Ok(Self {
            base: BaseModel { configuration },

            session: Mutex::new(session),
            overview,
        })
    }

    /// Layer names, shapes and dtypes from a loaded session.
    fn get_model_overview(session: &Session) -> Result<ModelOverview, ModelError> {
        let inputs = session
            .inputs
            .iter()
            .map(|input| {
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

    fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError> {
        self.run_bounded(inputs, None)
    }

    /// Stop the native run at the deadline so the model lease and admission
    /// permit come back. `bound: None` is an ordinary unbounded query.
    fn query_with_deadline(
        &self,
        inputs: &[GenericTensor],
        bound: Option<&QueryBound>,
    ) -> Result<Vec<GenericTensor>, ModelError> {
        self.run_bounded(inputs, bound)
    }

    fn overview(&self) -> Result<ModelOverview, ModelError> {
        Ok(self.overview.clone())
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
    fn param_list_or_default(
        &self,
        param: &str,
        default_value: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        self.base.param_list_or_default(param, default_value)
    }
}

/// ONNX `TensorProto.DataType` code for `LayerOverview.data_type`.
/// Metadata only: nothing here branches on it.
fn ort_type_to_i32(dtype: TensorElementType) -> Result<i32, ModelError> {
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
        // Unknown codes (UNDEFINED, FP8, INT4): error instead of mislabelling them.
        _ => Err(ModelError::NotImplemented(
            format!("Conversion for ONNX data type {:?} to i32", dtype),
            "onnx-runtime".to_string(),
        )),
    }
}
