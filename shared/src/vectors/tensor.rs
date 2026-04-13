//! Dynamically typed tensors for model inputs and outputs.

use super::error::VectorError;
use super::types::{FloatCube, FloatMatrix, FloatPenta, FloatQuad, FloatVector};
use ndarray::{Array, ArrayView, Dim, Ix2, IxDyn};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TensorDataType {
    String,
    Numeric,
    Dictionary,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenericTensor {
    pub value: TensorValue,
    pub shape: Vec<usize>,
    pub dtype: TensorDataType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TensorValue {
    Float32(Array<f32, IxDyn>),
    Int64(Array<i64, IxDyn>),
    String(String),
    Dictionary(HashMap<String, GenericTensor>),
}

pub fn new_string_tensor(value: String) -> GenericTensor {
    GenericTensor {
        shape: vec![],
        dtype: TensorDataType::String,
        value: TensorValue::String(value),
    }
}

pub fn new_dictionary_tensor(value: HashMap<String, GenericTensor>) -> GenericTensor {
    GenericTensor {
        shape: vec![],
        dtype: TensorDataType::Dictionary,
        value: TensorValue::Dictionary(value),
    }
}
pub trait IntoGenericTensor {
    fn into_generic_tensor(self) -> Result<GenericTensor, VectorError>;
}
/// Token ids / attention masks as a 2D i64 tensor.
pub fn new_2d_tensor_from_i64_vecs(value: Vec<Vec<i64>>) -> Result<GenericTensor, VectorError> {
    let rows = value.len();
    if rows == 0 {
        return Ok(GenericTensor {
            value: TensorValue::Int64(Array::zeros(IxDyn(&[0, 0]))),
            shape: vec![0, 0],
            dtype: TensorDataType::Numeric,
        });
    }
    let cols = value[0].len();
    let flat_vec: Vec<i64> = value.into_iter().flatten().collect();
    let arr = Array::from_shape_vec(IxDyn(&[rows, cols]), flat_vec)?;
    Ok(GenericTensor {
        value: TensorValue::Int64(arr),
        shape: vec![rows, cols],
        dtype: TensorDataType::Numeric,
    })
}

pub fn new_2d_tensor_from_float_vecs(value: Vec<Vec<f32>>) -> Result<GenericTensor, VectorError> {
    let rows = value.len();
    if rows == 0 {
        return Ok(GenericTensor {
            value: TensorValue::Float32(Array::zeros(IxDyn(&[0, 0]))),
            shape: vec![0, 0],
            dtype: TensorDataType::Numeric,
        });
    }
    let cols = value[0].len();
    let flat_vec: Vec<f32> = value.into_iter().flatten().collect();
    let arr = Array::from_shape_vec(IxDyn(&[rows, cols]), flat_vec)?;
    Ok(GenericTensor {
        value: TensorValue::Float32(arr),
        shape: vec![rows, cols],
        dtype: TensorDataType::Numeric,
    })
}

pub fn new_3d_tensor_from_float_vecs(
    value: Vec<Vec<Vec<f32>>>,
) -> Result<GenericTensor, VectorError> {
    if value.is_empty() {
        return Ok(GenericTensor {
            value: TensorValue::Float32(Array::zeros(IxDyn(&[0, 0, 0]))),
            shape: vec![0, 0, 0],
            dtype: TensorDataType::Numeric,
        });
    }
    let d1 = value.len();
    let d2 = value[0].len();
    let d3 = if d2 > 0 { value[0][0].len() } else { 0 };
    let shape = vec![d1, d2, d3];

    let flat_vec: Vec<f32> = value.into_iter().flatten().flatten().collect();
    let arr = Array::from_shape_vec(IxDyn(&shape), flat_vec)?;
    Ok(GenericTensor {
        value: TensorValue::Float32(arr),
        shape,
        dtype: TensorDataType::Numeric,
    })
}
/// Creates a `GenericTensor` from a source value, inferring its shape.
/// @tparam T The type of the value to convert, which must implement `IntoGenericTensor`.
pub fn new_tensor_with_shape_inference<T: IntoGenericTensor>(
    value: T,
) -> Result<GenericTensor, VectorError> {
    value.into_generic_tensor()
}
/// Creates a `GenericTensor` from a source value with a specified shape.
///
/// This function first infers the tensor from the value and then reshapes it
/// to the desired dimensions.
///
/// @tparam T A type that implements `IntoGenericTensor`.
pub fn new_tensor_with_shape<T: IntoGenericTensor>(
    value: T,
    shape: &[i64],
) -> Result<GenericTensor, VectorError> {
    let tensor = new_tensor_with_shape_inference(value)?;
    tensor.reshape(shape)
}
impl GenericTensor {
    /// Reshapes the tensor to a new set of dimensions.
    ///                  Dimensions less than or equal to 0 are treated as dynamic and are
    ///                  inferred.
    pub fn reshape(mut self, new_shape: &[i64]) -> Result<Self, VectorError> {
        let total_elements = self.value.len();
        if new_shape.is_empty() {
            return Ok(self);
        }
        // Only one dimension can be dynamic/inferred.
        if new_shape.iter().filter(|&&d| d <= 0).count() > 1 {
            return Err(VectorError::ReshapeError {
                size: total_elements,
                dims: new_shape.iter().map(|&d| d as usize).collect(),
            });
        }
        let final_shape: Vec<usize> = new_shape
            .iter()
            .map(|&d| {
                if d > 0 {
                    d as usize
                } else {
                    // Infer this dimension
                    let product_of_known_dims: usize = new_shape
                        .iter()
                        .filter(|&&x| x > 0)
                        .map(|&x| x as usize)
                        .product();
                    total_elements
                        .checked_div(product_of_known_dims)
                        .unwrap_or(total_elements)
                }
            })
            .collect();
        if final_shape.iter().product::<usize>() != total_elements {
            return Err(VectorError::ReshapeError {
                size: total_elements,
                dims: final_shape.clone(),
            });
        }
        self.value = self.value.reshape(&final_shape)?;
        self.shape = final_shape;
        Ok(self)
    }
    /// Tries to convert the generic tensor into a flat `FloatVector`.
    ///
    /// This method will flatten the underlying data if it's a higher-dimensional
    /// numeric tensor.
    ///
    /// This fails for non-numeric tensor types (`String`, `Dictionary`).
    ///
    pub fn try_into_vector(self) -> Result<FloatVector, VectorError> {
        match self.dtype {
            TensorDataType::Numeric => {
                let flat_f32 = self.value.flatten_to_f32_vec();
                Ok(FloatVector::from_vec(flat_f32))
            }
            _ => Err(VectorError::ConversionError(
                "Cannot convert non-numeric tensor to a vector.".into(),
            )),
        }
    }
    /// Converts the tensor to a 1D `FloatVector`.
    ///
    /// This method is a more explicit and specific version of `try_into_vector`,
    /// now named to align with the rest of the `as_float_...` methods.
    pub fn as_float_vector(self) -> Result<FloatVector, VectorError> {
        self.try_into_vector()
    }
    /// Tries to convert the tensor to a 2D `FloatMatrix`.
    ///
    /// The tensor must be numeric and have a 2D shape.
    ///
    pub fn as_float_matrix(self) -> Result<FloatMatrix, VectorError> {
        if self.shape.len() != 2 {
            return Err(VectorError::DimensionMismatch {
                expected: vec![
                    self.shape.first().copied().unwrap_or(0),
                    self.shape.get(1).copied().unwrap_or(0),
                ],
                actual: self.shape.clone(),
            });
        }
        if self.dtype != TensorDataType::Numeric {
            return Err(VectorError::ConversionError(
                "Cannot convert non-numeric tensor to a matrix.".into(),
            ));
        }
        let flat_f32 = self.value.flatten_to_f32_vec();
        let rows = self.shape[0];
        let cols = self.shape[1];
        Ok(Array::from_shape_vec(Dim([rows, cols]), flat_f32)?)
    }
    /// Tries to convert the tensor to a 3D `FloatCube`.
    ///
    /// The tensor must be numeric and have a 3D shape.
    ///
    pub fn as_float_cube(self) -> Result<FloatCube, VectorError> {
        if self.shape.len() != 3 {
            return Err(VectorError::DimensionMismatch {
                expected: vec![
                    self.shape.first().copied().unwrap_or(0),
                    self.shape.get(1).copied().unwrap_or(0),
                    self.shape.get(2).copied().unwrap_or(0),
                ],
                actual: self.shape.clone(),
            });
        }
        if self.dtype != TensorDataType::Numeric {
            return Err(VectorError::ConversionError(
                "Cannot convert non-numeric tensor to a cube.".into(),
            ));
        }
        let flat_f32 = self.value.flatten_to_f32_vec();
        let d1 = self.shape[0];
        let d2 = self.shape[1];
        let d3 = self.shape[2];
        Ok(Array::from_shape_vec(Dim([d1, d2, d3]), flat_f32)?)
    }
    /// Tries to convert the tensor to a 4D `FloatQuad`.
    ///
    /// The tensor must be numeric and have a 4D shape.
    ///
    pub fn as_float_quad(self) -> Result<FloatQuad, VectorError> {
        if self.shape.len() != 4 {
            return Err(VectorError::DimensionMismatch {
                expected: vec![
                    self.shape.first().copied().unwrap_or(0),
                    self.shape.get(1).copied().unwrap_or(0),
                    self.shape.get(2).copied().unwrap_or(0),
                    self.shape.get(3).copied().unwrap_or(0),
                ],
                actual: self.shape.clone(),
            });
        }
        if self.dtype != TensorDataType::Numeric {
            return Err(VectorError::ConversionError(
                "Cannot convert non-numeric tensor to a quad.".into(),
            ));
        }
        let flat_f32 = self.value.flatten_to_f32_vec();
        let d1 = self.shape[0];
        let d2 = self.shape[1];
        let d3 = self.shape[2];
        let d4 = self.shape[3];
        Ok(Array::from_shape_vec(Dim([d1, d2, d3, d4]), flat_f32)?)
    }
    /// Tries to convert the tensor to a 5D `FloatPenta`.
    ///
    /// The tensor must be numeric and have a 5D shape.
    ///
    pub fn as_float_penta(self) -> Result<FloatPenta, VectorError> {
        if self.shape.len() != 5 {
            return Err(VectorError::DimensionMismatch {
                expected: vec![
                    self.shape.first().copied().unwrap_or(0),
                    self.shape.get(1).copied().unwrap_or(0),
                    self.shape.get(2).copied().unwrap_or(0),
                    self.shape.get(3).copied().unwrap_or(0),
                    self.shape.get(4).copied().unwrap_or(0),
                ],
                actual: self.shape.clone(),
            });
        }
        if self.dtype != TensorDataType::Numeric {
            return Err(VectorError::ConversionError(
                "Cannot convert non-numeric tensor to a penta.".into(),
            ));
        }
        let flat_f32 = self.value.flatten_to_f32_vec();
        let d1 = self.shape[0];
        let d2 = self.shape[1];
        let d3 = self.shape[2];
        let d4 = self.shape[3];
        let d5 = self.shape[4];
        Ok(Array::from_shape_vec(Dim([d1, d2, d3, d4, d5]), flat_f32)?)
    }

    /// Tries to get a read-only view of the underlying data as an `i64` ndarray.
    ///
    /// This is an efficient way to access `Int64` tensor data without cloning.
    /// It fails if the tensor's underlying `TensorValue` is not `Int64`.
    ///
    pub fn as_int64_array(&self) -> Result<ArrayView<'_, i64, IxDyn>, VectorError> {
        match &self.value {
            TensorValue::Int64(arr) => Ok(arr.view()),
            _ => Err(VectorError::ConversionError(
                "TensorValue is not of type Int64".into(),
            )),
        }
    }
}
impl TensorValue {
    /// Returns the total number of elements in the tensor value.
    pub fn len(&self) -> usize {
        match self {
            TensorValue::Float32(arr) => arr.len(),
            TensorValue::Int64(arr) => arr.len(),
            TensorValue::String(_) => 1,
            TensorValue::Dictionary(d) => d.len(),
        }
    }
    /// Checks if the tensor value is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Reshapes the underlying `ndarray::Array`.
    ///
    pub fn reshape(self, new_shape: &[usize]) -> Result<TensorValue, VectorError> {
        match self {
            TensorValue::Float32(arr) => {
                // Use `into_shape_with_order` to explicitly specify the reshaping order.
                // The default is row-major (C-order), which is the standard expectation.
                let reshaped = arr.into_shape_with_order(IxDyn(new_shape))?;
                Ok(TensorValue::Float32(reshaped))
            }
            TensorValue::Int64(arr) => {
                // Same as above for Int64 tensors.
                let reshaped = arr.into_shape_with_order(IxDyn(new_shape))?;
                Ok(TensorValue::Int64(reshaped))
            }
            _ => Err(VectorError::ConversionError(
                "Cannot reshape non-numeric tensor type".into(),
            )),
        }
    }
    /// Returns the shape of the underlying ndarray.
    pub fn get_shape(&self) -> &[usize] {
        match self {
            TensorValue::Float32(arr) => arr.shape(),
            TensorValue::Int64(arr) => arr.shape(),
            _ => &[],
        }
    }
    /// Flattens the underlying numeric array into a 1D f32 vector.
    /// @note This involves casting, which might lose precision for non-f32 types.
    pub fn flatten_to_f32_vec(self) -> Vec<f32> {
        match self {
            TensorValue::Float32(arr) => arr.into_iter().collect(),
            TensorValue::Int64(arr) => arr.into_iter().map(|x| x as f32).collect(),
            _ => vec![],
        }
    }
    /// Tries to extract the underlying data as a read-only `ndarray::ArrayView`.
    /// This is efficient as it avoids cloning the data.
    pub fn try_as_ndarray_f32_view(&self) -> Result<ArrayView<'_, f32, IxDyn>, VectorError> {
        match self {
            TensorValue::Float32(arr) => Ok(arr.view()),
            _ => Err(VectorError::ConversionError(
                "TensorValue is not of type Float32".into(),
            )),
        }
    }
    /// Tries to convert the underlying data into an owned `ndarray::Array<i64, Ix2>`.
    /// Fails if the tensor is not Int64 or not 2D.
    pub fn try_into_ndarray_i64(self) -> Result<Array<i64, Ix2>, VectorError> {
        match self {
            TensorValue::Int64(arr) => arr.into_dimensionality::<Ix2>().map_err(|e| e.into()),
            _ => Err(VectorError::ConversionError(
                "TensorValue is not of type Int64".into(),
            )),
        }
    }
}
/// Helper macro to define the body of the `into_generic_tensor` implementation.
macro_rules! make_tensor_impl_body {
    ($value:expr) => {{
        let json_val = serde_json::to_value(&$value)?;
        let mut flat_vec_f32 = Vec::new();
        let mut flat_vec_i64 = Vec::new();
        // This function recursively extracts all numbers from a JSON Value.
        fn extract_numbers(
            val: &serde_json::Value,
            f32_vec: &mut Vec<f32>,
            i64_vec: &mut Vec<i64>,
        ) {
            match val {
                serde_json::Value::Array(arr) => {
                    for v in arr {
                        extract_numbers(v, f32_vec, i64_vec);
                    }
                }
                serde_json::Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        f32_vec.push(f as f32);
                    }
                    if let Some(i) = n.as_i64() {
                        i64_vec.push(i);
                    }
                }
                _ => {}
            }
        }
        extract_numbers(&json_val, &mut flat_vec_f32, &mut flat_vec_i64);
        // This function determines the shape of the nested JSON array.
        fn get_shape(val: &serde_json::Value) -> Vec<usize> {
            let mut shape = Vec::new();
            let mut current = val;
            while let serde_json::Value::Array(arr) = current {
                shape.push(arr.len());
                if arr.is_empty() {
                    break;
                }
                current = &arr[0];
            }
            shape
        }
        let shape = get_shape(&json_val);
        // Heuristic to decide if the data can be represented as i64 without loss.
        let use_i64 = flat_vec_f32.len() == flat_vec_i64.len()
            && flat_vec_f32
                .iter()
                .zip(&flat_vec_i64)
                .all(|(f, i)| *f == *i as f32);
        let tensor_value = if use_i64 {
            let arr = Array::from_shape_vec(IxDyn(&shape), flat_vec_i64)?;
            TensorValue::Int64(arr)
        } else {
            let arr = Array::from_shape_vec(IxDyn(&shape), flat_vec_f32)?;
            TensorValue::Float32(arr)
        };
        Ok(GenericTensor {
            value: tensor_value,
            shape,
            dtype: TensorDataType::Numeric,
        })
    }};
}
/// Helper macro to stamp out `IntoGenericTensor` implementations for a numeric type.
macro_rules! make_tensor_impls_for_type {
    ($numeric_type:ty) => {
        impl IntoGenericTensor for Vec<$numeric_type> {
            fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
                make_tensor_impl_body!(self)
            }
        }
        impl IntoGenericTensor for Vec<Vec<$numeric_type>> {
            fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
                make_tensor_impl_body!(self)
            }
        }
        impl IntoGenericTensor for Vec<Vec<Vec<$numeric_type>>> {
            fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
                make_tensor_impl_body!(self)
            }
        }
        impl IntoGenericTensor for Vec<Vec<Vec<Vec<$numeric_type>>>> {
            fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
                make_tensor_impl_body!(self)
            }
        }
        impl IntoGenericTensor for Vec<Vec<Vec<Vec<Vec<$numeric_type>>>>> {
            fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
                make_tensor_impl_body!(self)
            }
        }
    };
}
// Generate implementations for common numeric types.
make_tensor_impls_for_type!(f32);
make_tensor_impls_for_type!(f64);
make_tensor_impls_for_type!(i32);
make_tensor_impls_for_type!(i64);
make_tensor_impls_for_type!(u32);
make_tensor_impls_for_type!(u64);
make_tensor_impls_for_type!(isize);
make_tensor_impls_for_type!(usize);
// Manual implementation for `&serde_json::Value`.
impl IntoGenericTensor for &serde_json::Value {
    fn into_generic_tensor(self) -> Result<GenericTensor, VectorError> {
        let mut flat_vec = Vec::new();
        // This function recursively extracts all numbers from a JSON Value into a flat `Vec<f32>`.
        fn extract_f32(val: &serde_json::Value, vec: &mut Vec<f32>) {
            match val {
                serde_json::Value::Array(arr) => arr.iter().for_each(|v| extract_f32(v, vec)),
                serde_json::Value::Number(n) => {
                    if let Some(f) = n.as_f64() {
                        vec.push(f as f32)
                    }
                }
                _ => {}
            }
        }
        extract_f32(self, &mut flat_vec);
        // This function determines the shape of the nested JSON array.
        fn get_shape(val: &serde_json::Value) -> Vec<usize> {
            let mut shape = Vec::new();
            let mut current = val;
            while let serde_json::Value::Array(arr) = current {
                shape.push(arr.len());
                if arr.is_empty() {
                    break;
                }
                current = &arr[0];
            }
            shape
        }
        let shape = get_shape(self);
        let array = Array::from_shape_vec(IxDyn(&shape), flat_vec)?;
        Ok(GenericTensor {
            value: TensorValue::Float32(array),
            shape,
            dtype: TensorDataType::Numeric,
        })
    }
}
