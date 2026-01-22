// File: shared/src/vectors/types.rs
//!
//! ## Core Data Types
//!
//! This module defines the fundamental types used throughout the library.
//!
//! It leverages the `ndarray` crate to define type aliases for vectors,
//! matrices, and higher-dimensional tensors, providing a strong, type-safe
//! foundation for numerical computing.

// Import from sibling modules.
use super::error::VectorError;
use super::tensor::{GenericTensor, TensorDataType, TensorValue};
use ndarray::{Array, Ix1, Ix2, Ix3, Ix4, Ix5};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

// When changing the underlying type of FloatX, the change is reflected
// throughout the crate automatically by Rust's type system.
/// Custom-precision float type, aliased to `f32`.
pub type FloatX = f32;

// --- N-Dimensional Array Type Aliases ---
/// A 1-dimensional vector of `FloatX` values.
pub type FloatVector = Array<FloatX, Ix1>;
/// A 2-dimensional matrix of `FloatX` values.
pub type FloatMatrix = Array<FloatX, Ix2>;
/// A 3-dimensional tensor (cube) of `FloatX` values.
pub type FloatCube = Array<FloatX, Ix3>;
/// A 4-dimensional tensor of `FloatX` values.
pub type FloatQuad = Array<FloatX, Ix4>;
/// A 5-dimensional tensor of `FloatX` values.
pub type FloatPenta = Array<FloatX, Ix5>;
/// A 1-dimensional vector of `f64` values.
pub type Float64Vector = Array<f64, Ix1>;
/// A 2-dimensional matrix of `f64` values.
pub type Float64Matrix = Array<f64, Ix2>;
/// A 1-dimensional vector of `f32` values.
pub type Float32Vector = Array<f32, Ix1>;
/// A 2-dimensional matrix of `f32` values.
pub type Float32Matrix = Array<f32, Ix2>;
/// A 3-dimensional tensor (cube) of `f32` values.
pub type Float32Cube = Array<f32, Ix3>;
/// A 4-dimensional tensor of `f32` values.
pub type Float32Quad = Array<f32, Ix4>;
/// A 5-dimensional tensor of `f32` values.
pub type Float32Penta = Array<f32, Ix5>;

// --- Constants ---
/// Represents "Not Available", using the smallest non-zero f64 value.
pub const NA: f64 = f64::MIN_POSITIVE;
/// Represents positive infinity.
pub const INF: f64 = f64::INFINITY;
/// Represents negative infinity.
pub const NEG_INF: f64 = f64::NEG_INFINITY;

/// A key-value pair, typically used for storing words and their similarity scores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pair {
    pub key: String,
    pub value: f64,
}
/// A list of `Pair` instances.
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
