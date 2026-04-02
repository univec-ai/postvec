//! JSON payload helper: convert a request field to the type an executor wants.

#![allow(dead_code)]
use crate::error::EngineError;
use serde::de::DeserializeOwned;
use serde_json::Value;
use shared::vectors::{
    AsFloatCube, AsFloatMatrix, AsFloatPenta, AsFloatQuad, AsFloatVector, FloatCube, FloatMatrix,
    FloatPenta, FloatQuad, FloatVector,
};
use std::collections::HashMap;

/// One JSON field from `Context::input_json`.
#[derive(Clone, Copy)]
pub struct InputValue<'a> {
    /// A reference to the underlying JSON value from the request payload.
    value: &'a Value,
}

impl<'a> InputValue<'a> {
    pub fn new(value: &'a Value) -> Self {
        Self { value }
    }

    pub fn as_value(&self) -> &'a Value {
        self.value
    }

    fn convert_value<T: DeserializeOwned>(&self) -> Result<T, EngineError> {
        serde_json::from_value(self.value.clone()).map_err(|e| {
            EngineError::InputTypeError(format!(
                "Failed to convert JSON value '{}' into the requested type: {}",
                self.value, e
            ))
        })
    }

    pub fn as_string(&self) -> Result<String, EngineError> {
        self.value.as_str().map(ToString::to_string).ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a string, but found '{}'", self.value))
        })
    }

    pub fn as_i64(&self) -> Result<i64, EngineError> {
        self.value.as_i64().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected an integer, but found '{}'", self.value))
        })
    }

    pub fn as_f64(&self) -> Result<f64, EngineError> {
        self.value.as_f64().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a float, but found '{}'", self.value))
        })
    }

    pub fn as_bool(&self) -> Result<bool, EngineError> {
        self.value.as_bool().ok_or_else(|| {
            EngineError::InputTypeError(format!("Expected a boolean, but found '{}'", self.value))
        })
    }

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

    pub fn as_vec<T: DeserializeOwned>(&self) -> Result<Vec<T>, EngineError> {
        self.convert_value()
    }

    pub fn as_string_list(&self) -> Result<Vec<String>, EngineError> {
        self.convert_value()
    }

    /// A string or an array of strings. Integer / nested-integer arrays are
    /// rejected: this engine does not take pre-tokenised input.
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
                // Token-id arrays are not accepted.
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

    pub fn as_string_map<T: DeserializeOwned>(&self) -> Result<HashMap<String, T>, EngineError> {
        self.convert_value()
    }

    pub fn as_float_vector(&self) -> Result<FloatVector, EngineError> {
        self.value
            .as_float_vector()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    pub fn as_float_matrix(&self) -> Result<FloatMatrix, EngineError> {
        let nested_vec: Vec<Vec<f32>> = self.convert_value()?;
        nested_vec
            .as_float_matrix()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    pub fn as_float_cube(&self) -> Result<FloatCube, EngineError> {
        let nested_vec: Vec<Vec<Vec<f32>>> = self.convert_value()?;
        nested_vec
            .as_float_cube()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    pub fn as_float_quad(&self) -> Result<FloatQuad, EngineError> {
        let nested_vec: Vec<Vec<Vec<Vec<f32>>>> = self.convert_value()?;
        nested_vec
            .as_float_quad()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }

    pub fn as_float_penta(&self) -> Result<FloatPenta, EngineError> {
        let nested_vec: Vec<Vec<Vec<Vec<Vec<f32>>>>> = self.convert_value()?;
        nested_vec
            .as_float_penta()
            .map_err(|e| EngineError::InputTypeError(e.to_string()))
    }
}
