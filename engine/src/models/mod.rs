//! Model trait, configuration and backends (ONNX Runtime when the `onnx` feature is on).

pub mod base_model;
pub mod configuration;
pub mod error;
pub mod generic_model;
pub mod model;
pub mod pool;

#[cfg(feature = "onnx")]
pub mod onnx_runtime;

pub use self::base_model::BaseModel;
pub use self::configuration::{
    ExecutionProvider, ExecutorConfiguration, InputLayoutItem, InputLayoutItemType, InputMapping,
    LayerOverview, ModelBackend, ModelConfiguration, ModelOverview, OutputMapping,
    QuantizationMode,
};
pub use self::error::ModelError;
pub use self::generic_model::GenericModel;
pub use self::model::{Model, QueryBound};

// Conditionally re-export the OnnxRuntimeModel from its new module.
#[cfg(feature = "onnx")]
pub use self::onnx_runtime::OnnxRuntimeModel;
