//! ndarray type aliases used throughout the crate.

use super::error::VectorError;
use super::tensor::{GenericTensor, TensorDataType, TensorValue};
use ndarray::{Array, Ix1, Ix2, Ix3, Ix4, Ix5};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Element type for the default float arrays. Changing this alias is
/// load-bearing: serialize/deserialize assume it is `f32`.
pub type FloatX = f32;

pub type FloatVector = Array<FloatX, Ix1>;
pub type FloatMatrix = Array<FloatX, Ix2>;
pub type FloatCube = Array<FloatX, Ix3>;
pub type FloatQuad = Array<FloatX, Ix4>;
pub type FloatPenta = Array<FloatX, Ix5>;
pub type Float64Vector = Array<f64, Ix1>;
pub type Float64Matrix = Array<f64, Ix2>;
pub type Float32Vector = Array<f32, Ix1>;
pub type Float32Matrix = Array<f32, Ix2>;
pub type Float32Cube = Array<f32, Ix3>;
pub type Float32Quad = Array<f32, Ix4>;
pub type Float32Penta = Array<f32, Ix5>;

pub const NA: f64 = f64::MIN_POSITIVE;
pub const INF: f64 = f64::INFINITY;
pub const NEG_INF: f64 = f64::NEG_INFINITY;

/// Word plus similarity score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pair {
    pub key: String,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PairList(pub Vec<Pair>);
impl PairList {
    /// Returns the number of pairs in the list.
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Checks if the list is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Sorts the list by value in ascending order.
    pub fn sort_by_value(&mut self) {
        self.0
            .sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap_or(Ordering::Equal));
    }
    /// Sorts the list first by value (ascending), then by key (lexicographically) as a tie-breaker.
    pub fn sort_by_value_and_key(&mut self) {
        self.0.sort_by(|a, b| {
            let val_ord = a.value.partial_cmp(&b.value).unwrap_or(Ordering::Equal);
            if val_ord == Ordering::Equal {
                a.key.cmp(&b.key)
            } else {
                val_ord
            }
        });
    }
    /// Finds the maximum value in the pair list.
    ///
    /// # Returns
    /// The maximum value, or 0.0 if the list is empty.
    pub fn max_value(&self) -> f64 {
        // Guard the empty case explicitly: folding with a `NEG_INFINITY` seed
        // would otherwise return `-inf` for an empty list (contradicting the
        // documented `0.0`), which then trips the `is_infinite()` check in
        // `to_unit` and surfaces as `InvalidArgument` instead of the correct
        // `DivisionByZero`.
        if self.0.is_empty() {
            return 0.0;
        }
        self.0
            .iter()
            .map(|p| p.value)
            .fold(f64::NEG_INFINITY, f64::max)
    }
    /// Scales all values in the list to be unit values (i.e., divides by the max value).
    pub fn to_unit(&mut self) -> Result<(), VectorError> {
        let max_val = self.max_value();
        if max_val == 0.0 {
            return Err(VectorError::DivisionByZero);
        }
        if max_val.is_infinite() || max_val.is_nan() {
            return Err(VectorError::InvalidArgument(
                "Invalid max value for scaling".into(),
            ));
        }
        for pair in &mut self.0 {
            pair.value /= max_val;
        }
        Ok(())
    }
    /// Truncates the list to a maximum length.
    pub fn cut_to_max_len(&mut self, max_len: usize) {
        self.0.truncate(max_len);
    }
}

// --- to_tensor() implementations ---
/// Trait for converting N-dimensional arrays to a `GenericTensor`.
pub trait IntoGenericTensorExt {
    /// Converts the array into a `GenericTensor` with the correct shape and data type.
    fn to_tensor(self) -> GenericTensor;
}

impl IntoGenericTensorExt for FloatVector {
    fn to_tensor(self) -> GenericTensor {
        let shape = self.shape().to_vec();
        GenericTensor {
            value: TensorValue::Float32(self.into_dyn()),
            shape,
            dtype: TensorDataType::Numeric,
        }
    }
}
impl IntoGenericTensorExt for FloatMatrix {
    fn to_tensor(self) -> GenericTensor {
        let shape = self.shape().to_vec();
        GenericTensor {
            value: TensorValue::Float32(self.into_dyn()),
            shape,
            dtype: TensorDataType::Numeric,
        }
    }
}
impl IntoGenericTensorExt for FloatCube {
    fn to_tensor(self) -> GenericTensor {
        let shape = self.shape().to_vec();
        GenericTensor {
            value: TensorValue::Float32(self.into_dyn()),
            shape,
            dtype: TensorDataType::Numeric,
        }
    }
}
impl IntoGenericTensorExt for FloatQuad {
    fn to_tensor(self) -> GenericTensor {
        let shape = self.shape().to_vec();
        GenericTensor {
            value: TensorValue::Float32(self.into_dyn()),
            shape,
            dtype: TensorDataType::Numeric,
        }
    }
}
impl IntoGenericTensorExt for FloatPenta {
    fn to_tensor(self) -> GenericTensor {
        let shape = self.shape().to_vec();
        GenericTensor {
            value: TensorValue::Float32(self.into_dyn()),
            shape,
            dtype: TensorDataType::Numeric,
        }
    }
}
