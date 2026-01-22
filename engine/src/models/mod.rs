// File: engine/src/models/mod.rs
//!
//! # Models Module
//!
//! This module defines the abstractions and implementations for interacting with
//! various machine learning models, such as those in ONNX format. It is a
//! core component of the Baikal ecosystem, refactored from Go to Rust.
//!
//! ## Core Concepts
//!
//! - **`Model` Trait**: A common interface for all model types.
//! - **`ModelConfiguration`**: A struct for defining model properties.
//! - **`GenericModel`**: A placeholder model implementation.
//! - **`OnnxRuntimeModel`**: An implementation for running ONNX models, enabled via the `onnx` feature flag.

// Declare the modules that make up this crate. Each module is in its own file.
pub mod base_model;
pub mod configuration;
pub mod error;
pub mod generic_model;
pub mod model;

/// This module defines the `deadpool` manager for creating pools of `Model` instances.
pub mod pool;

// Conditionally compile the onnx_runtime module only when the `onnx` feature is enabled.
// This module contains the implementation for `OnnxRuntimeModel`.
#[cfg(feature = "onnx")]
pub mod onnx_runtime;

// Re-export the key public-facing types and traits for a clean public API.
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
