//! Fallible conversions into `FloatX`.

use super::error::VectorError;
use super::types::FloatX;
use serde_json::Value;

/// A trait for types that can be fallibly converted to a `FloatX`.
pub trait TryIntoFloatX {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError>;
}

impl TryIntoFloatX for Value {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError> {
        self.as_f64().map(|f| f as FloatX).ok_or_else(|| {
            VectorError::ConversionError(format!("Cannot convert JSON value '{self}' to FloatX"))
        })
    }
}

impl TryIntoFloatX for f64 {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError> {
        Ok(*self as FloatX)
    }
}

impl TryIntoFloatX for f32 {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError> {
        Ok(*self)
    }
}

impl TryIntoFloatX for i64 {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError> {
        Ok(*self as FloatX)
    }
}

impl TryIntoFloatX for &str {
    fn try_into_float_x(&self) -> Result<FloatX, VectorError> {
        self.parse::<f64>()
            .map(|f| f as FloatX)
            .map_err(|e| e.into())
    }
}
