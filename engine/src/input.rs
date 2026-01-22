// File: engine/src/input.rs

//! ## Executor Input Handling
//!
//! This module provides a helper struct, `InputValue`, for safely accessing and converting
//! parts of a JSON request payload into the various data types required by an executor.
//!
//! This centralizes conversion logic and provides clear, specific error messages.

#![allow(dead_code)]
use crate::error::EngineError;
use serde::de::DeserializeOwned;
use serde_json::Value;
use shared::vectors::{
    AsFloatCube, AsFloatMatrix, AsFloatPenta, AsFloatQuad, AsFloatVector, FloatCube, FloatMatrix,
    FloatPenta, FloatQuad, FloatVector,
};
use std::collections::HashMap;

/// A wrapper around a `serde_json::Value` reference that provides convenient
/// and fallible conversion methods. An instance of this is returned by `Context::input_json`.
#[derive(Clone, Copy)]
pub struct InputValue<'a> {
    /// A reference to the underlying JSON value from the request payload.
    value: &'a Value,
}

impl<'a> InputValue<'a> {
    /// Creates a new `InputValue` from a `serde_json::Value` reference.
    pub fn new(value: &'a Value) -> Self {
        Self { value }
    }

    /// Returns a reference to the underlying `serde_json::Value`.
    pub fn as_value(&self) -> &'a Value {
        self.value
    }

    /// A generic private helper that uses `serde_json` to deserialize the value.
    /// This is useful for complex types like lists and maps.
    fn convert_value<T: DeserializeOwned>(&self) -> Result<T, EngineError> {
        serde_json::from_value(self.value.clone()).map_err(|e| {
            EngineError::InputTypeError(format!(
                "Failed to convert JSON value '{}' into the requested type: {}",
                self.value, e
            ))
        })
    }

    /// Attempts to convert the value into a `String`.
    pub fn as_string(&self) -> Result<String, EngineError> {
        self.value.as_str().map(ToString::to_string).ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a string, but found '{}'", self.value))
        })
    }

    /// Attempts to convert the value into an `i64`.
    pub fn as_i64(&self) -> Result<i64, EngineError> {
        self.value.as_i64().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected an integer, but found '{}'", self.value))
        })
    }

    /// Attempts to convert the value into an `f64`.
    pub fn as_f64(&self) -> Result<f64, EngineError> {
        self.value.as_f64().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a float, but found '{}'", self.value))
        })
    }

    /// Attempts to convert the value into a `bool`.
    pub fn as_bool(&self) -> Result<bool, EngineError> {
        self.value.as_bool().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a boolean, but found '{}'", self.value))
        })
    }

    /// Attempts to convert the value into a `u8`.
    pub fn as_u8(&self) -> Result<u8, EngineError> {
        self.value
            .as_u64()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| {
                EngineError::InputTypeError(format!(
                    "Expected a u8 (0-255), but found '{}'",
                    self.value
                ))
            })
    }

    /// Attempts to deserialize the value into a `Vec<T>`.
    pub fn as_vec<T: DeserializeOwned>(&self) -> Result<Vec<T>, EngineError> {
        self.convert_value()
    }

    /// Attempts to deserialize the value into a `Vec<String>`.
    pub fn as_string_list(&self) -> Result<Vec<String>, EngineError> {
        self.convert_value()
    }

    /// Accepts either a single JSON string or a JSON array of strings, returning
    /// `Vec<String>` in both cases. Rejects integer / nested-integer arrays with a
    /// dedicated "token IDs are not supported" message — the engine deliberately
    /// does not consume pre-tokenised input (see docs/openrouter/openrouter-integration.md §1.3).
    pub fn as_string_or_string_list(&self) -> Result<Vec<String>, EngineError> {
        match self.value {
            Value::String(s) => Ok(vec![s.clone()]),
            Value::Array(items) => {
                if items.is_empty() {
                    return Err(EngineError::InputTypeError(
                        "`input` must be a non-empty string or array of strings".into(),
                    ));
                }
                if items.iter().all(Value::is_string) {
                    return Ok(items
                        .iter()
                        .map(|v| v.as_str().unwrap().to_string())
                        .collect());
                }
                // Pre-tokenised int / nested-int arrays are explicitly unsupported.
                if items.iter().all(|v| v.is_number())
                    || items.iter().all(|v| {
                        v.as_array()
                            .map(|inner| inner.iter().all(Value::is_number))
                            .unwrap_or(false)
                    })
                {
                    return Err(EngineError::InputTypeError(
                        "`input` contains token IDs, which are not supported. Provide a string or an array of strings instead.".into(),
                    ));
                }
                Err(EngineError::InputTypeError(
                    "`input` must be a string or an array of strings; mixed or nested types are not supported".into(),
                ))
            }
            Value::Number(_) => Err(EngineError::InputTypeError(
                "`input` is a number; token IDs are not supported. Provide a string or an array of strings.".into(),
            )),
            Value::Null => Err(EngineError::InputTypeError(
                "`input` is null; expected a string or array of strings".into(),
            )),
            Value::Bool(_) => Err(EngineError::InputTypeError(
                "`input` is a boolean; expected a string or array of strings".into(),
            )),
            Value::Object(_) => Err(EngineError::InputTypeError(
                "`input` is an object; expected a string or array of strings".into(),
            )),
        }
    }

    /// Attempts to deserialize the value into a `HashMap<String, T>`.
    pub fn as_string_map<T: DeserializeOwned>(&self) -> Result<HashMap<String, T>, EngineError> {
        self.convert_value()
    }

    /// Attempts to convert a JSON array into a `FloatVector`.
    pub fn as_float_vector(&self) -> Result<FloatVector, EngineError> {
        self.value
            .as_float_vector()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    /// Attempts to convert a JSON array of arrays into a `FloatMatrix`.
    pub fn as_float_matrix(&self) -> Result<FloatMatrix, EngineError> {
        // First, deserialize the JSON value into a nested Vec.
        let nested_vec: Vec<Vec<f32>> = self.convert_value()?;
        // Then, use the trait from the `vectors` crate to convert the Vec into a FloatMatrix.
        nested_vec
            .as_float_matrix()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    /// Attempts to convert a 3D JSON array into a `FloatCube`.
    pub fn as_float_cube(&self) -> Result<FloatCube, EngineError> {
        let nested_vec: Vec<Vec<Vec<f32>>> = self.convert_value()?;
        nested_vec
            .as_float_cube()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    /// Attempts to convert a 4D JSON array into a `FloatQuad`.
    pub fn as_float_quad(&self) -> Result<FloatQuad, EngineError> {
        let nested_vec: Vec<Vec<Vec<Vec<f32>>>> = self.convert_value()?;
        nested_vec
            .as_float_quad()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    /// Attempts to convert a 5D JSON array into a `FloatPenta`.
    pub fn as_float_penta(&self) -> Result<FloatPenta, EngineError> {
        let nested_vec: Vec<Vec<Vec<Vec<Vec<f32>>>>> = self.convert_value()?;
        nested_vec
            .as_float_penta()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }
}
