// File: shared/src/vectors/ndarray_ops.rs
//!
//! @file ndarray_ops.rs
//! @brief ND-Array Operations
//!
//! This module provides functionality for creating, converting, reshaping,
//! and manipulating N-dimensional arrays (`ndarray`). It defines a set of traits
//! for converting nested `Vec` structures into typed `ndarray` tensors
//! (`FloatVector`, `FloatMatrix`, etc.) and for extending `ndarray` arrays
//! with additional capabilities like reshaping and dimension manipulation.
//!
//! This module is the Rust equivalent of the original Go implementation's `ndarray.go` file.
//!

// The `AsFloat{Vector,Matrix,Cube,Quad,Penta}` traits are *consuming*
// conversions (they take `self` by value, like `into_*`) but are deliberately
// named `as_*` for symmetry with the wider codebase's tensor-conversion API
// (~30 call sites across the engine executors). Renaming to `into_*` is the
// idiomatic fix but a wide, churny change with no behavioural benefit, so we
// silence the self-convention lint for this module instead.
#![allow(clippy::wrong_self_convention)]

// Import from sibling modules.
use super::error::VectorError;
use super::types::{FloatCube, FloatMatrix, FloatPenta, FloatQuad, FloatVector, FloatX};
use ndarray::{Array, Axis, Ix1, Ix2, Ix3, Ix4, Ix5};
use serde_json::Value;

// --- Conversion Traits ---

/// @brief A trait for types that can be converted into a `FloatVector`.
///
/// This provides a common interface for creating a 1D `ndarray` array of `FloatX`
/// from various data sources, such as a `Vec<T>` or a `serde_json::Value`.
pub trait AsFloatVector {
    /// @brief Performs the conversion into a `FloatVector`.
    ///
    /// @return A `Result` containing the new `FloatVector` on success,
    ///         or a `VectorError` if the conversion fails.
    fn as_float_vector(self) -> Result<FloatVector, VectorError>;
}

/// @brief A trait for types that can be converted into a `FloatMatrix`.
///
/// This provides a common interface for creating a 2D `ndarray` array of `FloatX`
/// from various data sources, like a `Vec<Vec<T>>`.
pub trait AsFloatMatrix {
    /// @brief Performs the conversion into a `FloatMatrix`.
    ///
    /// @return A `Result` containing the new `FloatMatrix` on success,
    ///         or a `VectorError` if the conversion fails (e.g., due to inconsistent row lengths).
    fn as_float_matrix(self) -> Result<FloatMatrix, VectorError>;
}

/// @brief A trait for types that can be converted into a `FloatCube`.
///
/// This provides a common interface for creating a 3D `ndarray` array of `FloatX`
/// from various data sources, like a `Vec<Vec<Vec<T>>>`.
pub trait AsFloatCube {
    /// @brief Performs the conversion into a `FloatCube`.
    ///
    /// @return A `Result` containing the new `FloatCube` on success,
    ///         or a `VectorError` if the conversion fails (e.g., due to inconsistent dimensions).
    fn as_float_cube(self) -> Result<FloatCube, VectorError>;
}

/// @brief A trait for types that can be converted into a `FloatQuad` (4D tensor).
///
/// This provides a common interface for creating a 4D `ndarray` array of `FloatX`
/// from various data sources, like a `Vec<Vec<Vec<Vec<T>>>>`.
pub trait AsFloatQuad {
    /// @brief Performs the conversion into a `FloatQuad`.
    ///
    /// @return A `Result` containing the new `FloatQuad` on success,
    ///         or a `VectorError` if the conversion fails.
    fn as_float_quad(self) -> Result<FloatQuad, VectorError>;
}

/// @brief A trait for types that can be converted into a `FloatPenta` (5D tensor).
///
/// This provides a common interface for creating a 5D `ndarray` array of `FloatX`
/// from various data sources, like a `Vec<Vec<Vec<Vec<Vec<T>>>>>`.
pub trait AsFloatPenta {
    /// @brief Performs the conversion into a `FloatPenta`.
    ///
    /// @return A `Result` containing the new `FloatPenta` on success,
    ///         or a `VectorError` if the conversion fails.
    fn as_float_penta(self) -> Result<FloatPenta, VectorError>;
}

// --- Trait Implementations for Standard Types ---

impl<T> AsFloatVector for Vec<T>
where
    T: Into<FloatX> + Copy,
{
    /// @brief Converts a `Vec<T>` into a `FloatVector`.
    /// @details Each element in the vector is converted into a `FloatX`.
    fn as_float_vector(self) -> Result<FloatVector, VectorError> {
        Ok(Array::from_vec(
            self.into_iter().map(|v| v.into()).collect(),
        ))
    }
}

impl AsFloatVector for &Value {
    /// @brief Converts a reference to a `serde_json::Value` into a `FloatVector`.
    /// @details The JSON value must be an array of numbers.
    ///
    /// @return A `Result` containing the `FloatVector` or a `VectorError::ConversionError`
    ///         if the value is not a JSON array or its elements are not numbers.
    fn as_float_vector(self) -> Result<FloatVector, VectorError> {
        let arr = self
            .as_array()
            .ok_or_else(|| VectorError::ConversionError("Input is not a JSON array".into()))?;

        let mut vector_output = Vec::with_capacity(arr.len());
        for (idx, elem) in arr.iter().enumerate() {
            let val = elem.as_f64().ok_or_else(|| {
                VectorError::ConversionError(format!(
                    "Element {idx} ('{elem}') is not a valid float number"
                ))
            })?;
            vector_output.push(val as FloatX);
        }
        Ok(Array::from_vec(vector_output))
    }
}

impl<T> AsFloatMatrix for Vec<Vec<T>>
where
    T: Into<FloatX> + Copy,
{
    /// @brief Converts a nested `Vec<Vec<T>>` into a `FloatMatrix`.
    /// @details The inner vectors must all have the same length.
    ///
    /// @return A `Result` containing the `FloatMatrix` or a `VectorError::ConversionError`
    ///         if the row lengths are inconsistent.
    fn as_float_matrix(self) -> Result<FloatMatrix, VectorError> {
        if self.is_empty() {
            return Ok(Array::zeros((0, 0)));
        }
        let rows = self.len();
        let cols = self[0].len();
        if !self.iter().all(|row| row.len() == cols) {
            return Err(VectorError::ConversionError(
                "Inconsistent row lengths in matrix".into(),
            ));
        }
        let flat: Vec<FloatX> = self.into_iter().flatten().map(|v| v.into()).collect();
        Array::from_shape_vec((rows, cols), flat)
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }
}

impl<T> AsFloatCube for Vec<Vec<Vec<T>>>
where
    T: Into<FloatX> + Copy,
{
    /// @brief Converts a 3D `Vec<Vec<Vec<T>>>` into a `FloatCube`.
    /// @details All dimensions must be consistent across the nested vectors.
    ///
    /// @return A `Result` containing the `FloatCube` or a `VectorError::ConversionError`
    ///         if any dimensions are inconsistent.
    fn as_float_cube(self) -> Result<FloatCube, VectorError> {
        if self.is_empty() {
            return Ok(Array::zeros((0, 0, 0)));
        }
        let d1 = self.len();
        if self[0].is_empty() {
            return Ok(Array::zeros((d1, 0, 0)));
        }
        let d2 = self[0].len();
        if self[0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, 0)));
        }
        let d3 = self[0][0].len();
        let mut flat_data = Vec::with_capacity(d1 * d2 * d3);
        for matrix in self {
            if matrix.len() != d2 {
                return Err(VectorError::ConversionError(
                    "Inconsistent matrix dimensions in cube.".into(),
                ));
            }
            for row in matrix {
                if row.len() != d3 {
                    return Err(VectorError::ConversionError(
                        "Inconsistent row lengths in cube.".into(),
                    ));
                }
                flat_data.extend(row.into_iter().map(|v| v.into()));
            }
        }
        Array::from_shape_vec((d1, d2, d3), flat_data)
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }
}

impl<T> AsFloatQuad for Vec<Vec<Vec<Vec<T>>>>
where
    T: Into<FloatX> + Copy,
{
    /// @brief Converts a 4D `Vec<Vec<Vec<Vec<T>>>>` into a `FloatQuad`.
    /// @details All dimensions must be consistent.
    ///
    /// @return A `Result` containing the `FloatQuad` or a `VectorError::ConversionError`.
    fn as_float_quad(self) -> Result<FloatQuad, VectorError> {
        if self.is_empty() {
            return Ok(Array::zeros((0, 0, 0, 0)));
        }
        let d1 = self.len();
        if self[0].is_empty() {
            return Ok(Array::zeros((d1, 0, 0, 0)));
        }
        let d2 = self[0].len();
        if self[0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, 0, 0)));
        }
        let d3 = self[0][0].len();
        if self[0][0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, d3, 0)));
        }
        let d4 = self[0][0][0].len();
        let mut flat_data = Vec::with_capacity(d1 * d2 * d3 * d4);
        for cube in self {
            if cube.len() != d2 {
                return Err(VectorError::ConversionError(
                    "Inconsistent cube dimensions in quad.".into(),
                ));
            }
            for matrix in cube {
                if matrix.len() != d3 {
                    return Err(VectorError::ConversionError(
                        "Inconsistent matrix dimensions in quad.".into(),
                    ));
                }
                for row in matrix {
                    if row.len() != d4 {
                        return Err(VectorError::ConversionError(
                            "Inconsistent row lengths in quad.".into(),
                        ));
                    }
                    flat_data.extend(row.into_iter().map(|v| v.into()));
                }
            }
        }
        Array::from_shape_vec((d1, d2, d3, d4), flat_data)
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }
}

impl<T> AsFloatPenta for Vec<Vec<Vec<Vec<Vec<T>>>>>
where
    T: Into<FloatX> + Copy,
{
    /// @brief Converts a 5D `Vec<Vec<Vec<Vec<Vec<T>>>>>` into a `FloatPenta`.
    /// @details All dimensions must be consistent.
    ///
    /// @return A `Result` containing the `FloatPenta` or a `VectorError::ConversionError`.
    fn as_float_penta(self) -> Result<FloatPenta, VectorError> {
        if self.is_empty() {
            return Ok(Array::zeros((0, 0, 0, 0, 0)));
        }
        let d1 = self.len();
        if self[0].is_empty() {
            return Ok(Array::zeros((d1, 0, 0, 0, 0)));
        }
        let d2 = self[0].len();
        if self[0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, 0, 0, 0)));
        }
        let d3 = self[0][0].len();
        if self[0][0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, d3, 0, 0)));
        }
        let d4 = self[0][0][0].len();
        if self[0][0][0][0].is_empty() {
            return Ok(Array::zeros((d1, d2, d3, d4, 0)));
        }
        let d5 = self[0][0][0][0].len();
        let mut flat_data = Vec::with_capacity(d1 * d2 * d3 * d4 * d5);
        for quad in self {
            if quad.len() != d2 {
                return Err(VectorError::ConversionError(
                    "Inconsistent quad dimensions in penta.".into(),
                ));
            }
            for cube in quad {
                if cube.len() != d3 {
                    return Err(VectorError::ConversionError(
                        "Inconsistent cube dimensions in penta.".into(),
                    ));
                }
                for matrix in cube {
                    if matrix.len() != d4 {
                        return Err(VectorError::ConversionError(
                            "Inconsistent matrix dimensions in penta.".into(),
                        ));
                    }
                    for row in matrix {
                        if row.len() != d5 {
                            return Err(VectorError::ConversionError(
                                "Inconsistent row lengths in penta.".into(),
                            ));
                        }
                        flat_data.extend(row.into_iter().map(|v| v.into()));
                    }
                }
            }
        }
        Array::from_shape_vec((d1, d2, d3, d4, d5), flat_data)
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }
}

// --- ND-Array Extension Trait ---

/// @brief Provides extension methods for `ndarray::Array` types.
///
/// This trait adds functionality for common operations like adding a new axis
/// to increase dimensionality, flattening an array to a 1D vector, and retrieving
/// the shape of the array.
pub trait NdArrayExt {
    /// @brief The type of the array with one higher dimension.
    type LargerDim;

    /// @brief Expands the dimensionality of the array by one.
    /// @details Inserts a new axis of size 1 at the beginning of the shape.
    /// For example, a `(3, 4)` matrix becomes a `(1, 3, 4)` cube.
    /// @return The new array with an additional dimension.
    fn expand_dimension(self) -> Self::LargerDim;

    /// @brief Flattens any N-dimensional array into a 1D `FloatVector`.
    /// @details Consumes the array and returns a new 1D vector containing all its elements
    ///          in logical (row-major) order.
    /// @return A `FloatVector`.
    fn flatten_to_vector(self) -> FloatVector;

    /// @brief Retrieves the shape of the array.
    /// @return A slice of `usize` representing the length of each dimension.
    fn get_shape(&self) -> &[usize];
}

impl NdArrayExt for Array<FloatX, Ix1> {
    type LargerDim = FloatMatrix;

    fn expand_dimension(self) -> Self::LargerDim {
        self.insert_axis(Axis(0))
    }

    fn flatten_to_vector(self) -> FloatVector {
        // A 1D vector is already flat.
        self
    }

    fn get_shape(&self) -> &[usize] {
        self.shape()
    }
}

impl NdArrayExt for Array<FloatX, Ix2> {
    type LargerDim = FloatCube;

    fn expand_dimension(self) -> Self::LargerDim {
        self.insert_axis(Axis(0))
    }

    fn flatten_to_vector(self) -> FloatVector {
        // The `into_flat` method consumes the array and returns a 1D representation,
        // which is the most correct and efficient way to flatten.
        self.into_flat()
    }

    fn get_shape(&self) -> &[usize] {
        self.shape()
    }
}

impl NdArrayExt for Array<FloatX, Ix3> {
    type LargerDim = FloatQuad;

    fn expand_dimension(self) -> Self::LargerDim {
        self.insert_axis(Axis(0))
    }

    fn flatten_to_vector(self) -> FloatVector {
        // The `into_flat` method consumes the array and returns a 1D representation.
        self.into_flat()
    }

    fn get_shape(&self) -> &[usize] {
        self.shape()
    }
}

impl NdArrayExt for Array<FloatX, Ix4> {
    type LargerDim = FloatPenta;

    fn expand_dimension(self) -> Self::LargerDim {
        self.insert_axis(Axis(0))
    }

    fn flatten_to_vector(self) -> FloatVector {
        // The `into_flat` method consumes the array and returns a 1D representation.
        self.into_flat()
    }

    fn get_shape(&self) -> &[usize] {
        self.shape()
    }
}

impl NdArrayExt for Array<FloatX, Ix5> {
    /// @brief A placeholder type, as expanding a 5D tensor is unsupported here.
    type LargerDim = ();

    /// @brief Not supported for 5D tensors in this context.
    /// @details `ndarray` does not provide a standard type alias for 6D tensors.
    /// @return This method will panic if called.
    fn expand_dimension(self) -> Self::LargerDim {
        unimplemented!("Expanding a 5D tensor is not supported in this context.");
    }

    fn flatten_to_vector(self) -> FloatVector {
        // The `into_flat` method consumes the array and returns a 1D representation.
        self.into_flat()
    }

    fn get_shape(&self) -> &[usize] {
        self.shape()
    }
}

// --- Reshape Extension Trait ---

/// @brief Provides reshaping capabilities for a `FloatVector`.
///
/// This trait allows a 1D vector to be transformed into a higher-dimensional
/// array (2D, 3D, 4D, or 5D) provided the total number of elements matches.
pub trait ReshapeExt {
    /// @brief Reshapes a vector into a 2D matrix.
    ///
    /// @param rows The number of rows in the new matrix.
    /// @param cols The number of columns in the new matrix.
    /// @return A `Result` containing the `FloatMatrix` or a `VectorError::ReshapeError`
    ///         if `rows * cols` does not equal the vector's length.
    fn reshape_2d(self, rows: usize, cols: usize) -> Result<FloatMatrix, VectorError>;

    /// @brief Reshapes a vector into a 3D cube.
    ///
    /// @param d1 The size of the first dimension.
    /// @param d2 The size of the second dimension.
    /// @param d3 The size of the third dimension.
    /// @return A `Result` containing the `FloatCube` or a `VectorError::ReshapeError`.
    fn reshape_3d(self, d1: usize, d2: usize, d3: usize) -> Result<FloatCube, VectorError>;

    /// @brief Reshapes a vector into a 4D tensor.
    ///
    /// @param d1 The size of the first dimension.
    /// @param d2 The size of the second dimension.
    /// @param d3 The size of the third dimension.
    /// @param d4 The size of the fourth dimension.
    /// @return A `Result` containing the `FloatQuad` or a `VectorError::ReshapeError`.
    fn reshape_4d(
        self,
        d1: usize,
        d2: usize,
        d3: usize,
        d4: usize,
    ) -> Result<FloatQuad, VectorError>;

    /// @brief Reshapes a vector into a 5D tensor.
    ///
    /// @param d1 The size of the first dimension.
    /// @param d2 The size of the second dimension.
    /// @param d3 The size of the third dimension.
    /// @param d4 The size of the fourth dimension.
    /// @param d5 The size of the fifth dimension.
    /// @return A `Result` containing the `FloatPenta` or a `VectorError::ReshapeError`.
    fn reshape_5d(
        self,
        d1: usize,
        d2: usize,
        d3: usize,
        d4: usize,
        d5: usize,
    ) -> Result<FloatPenta, VectorError>;
}

impl ReshapeExt for Array<FloatX, Ix1> {
    fn reshape_2d(self, rows: usize, cols: usize) -> Result<FloatMatrix, VectorError> {
        let size = self.len();
        if rows * cols != size {
            return Err(VectorError::ReshapeError {
                size,
                dims: vec![rows, cols],
            });
        }
        // Use `into_shape_with_order` to be explicit about memory layout.
        // The default is row-major (C order), which is the standard expectation
        // when reshaping a flat vector.
        self.into_shape_with_order((rows, cols))
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }

    fn reshape_3d(self, d1: usize, d2: usize, d3: usize) -> Result<FloatCube, VectorError> {
        let size = self.len();
        if d1 * d2 * d3 != size {
            return Err(VectorError::ReshapeError {
                size,
                dims: vec![d1, d2, d3],
            });
        }
        self.into_shape_with_order((d1, d2, d3))
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }

    fn reshape_4d(
        self,
        d1: usize,
        d2: usize,
        d3: usize,
        d4: usize,
    ) -> Result<FloatQuad, VectorError> {
        let size = self.len();
        if d1 * d2 * d3 * d4 != size {
            return Err(VectorError::ReshapeError {
                size,
                dims: vec![d1, d2, d3, d4],
            });
        }
        self.into_shape_with_order((d1, d2, d3, d4))
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }

    fn reshape_5d(
        self,
        d1: usize,
        d2: usize,
        d3: usize,
        d4: usize,
        d5: usize,
    ) -> Result<FloatPenta, VectorError> {
        let size = self.len();
        if d1 * d2 * d3 * d4 * d5 != size {
            return Err(VectorError::ReshapeError {
                size,
                dims: vec![d1, d2, d3, d4, d5],
            });
        }
        self.into_shape_with_order((d1, d2, d3, d4, d5))
            .map_err(|e| VectorError::ConversionError(e.to_string()))
    }
}

/// @brief Compares two shapes for compatibility.
///
/// @param a The first shape slice.
/// @param b The second shape slice.
/// @param dimension_check If true, checks that non-wildcard dimensions are equal.
///                        If false, only checks that the number of dimensions is the same
///                        (unless there are wildcards).
///
/// @details
/// This function supports "wildcard" dimensions, represented by a `0`.
/// If `dimension_check` is enabled, it compares the non-wildcard dimensions
/// for equality.
///
/// @return `true` if the shapes are compatible, `false` otherwise.
pub fn shape_match(a: &[usize], b: &[usize], dimension_check: bool) -> bool {
    let has_zero_dim = a.contains(&0) || b.contains(&0);
    // If there are no wildcards, the number of dimensions must be equal.
    if !has_zero_dim && a.len() != b.len() {
        return false;
    }
    // If not checking specific dimensions, compatibility is based on rank only.
    if !dimension_check {
        return true;
    }
    // Same rank: compare positionally. A `0` on *either* side is a wildcard
    // that matches any size at that position. Comparison must stay positional —
    // filtering wildcards out of both slices first would misalign the remaining
    // dimensions (e.g. `[2, 0, 3]` vs `[2, 5, 3]` would compare `[2, 3]` against
    // `[2, 5, 3]` and spuriously fail), so we keep indices aligned here.
    if a.len() == b.len() {
        return a
            .iter()
            .zip(b.iter())
            .all(|(&d1, &d2)| d1 == 0 || d2 == 0 || d1 == d2);
    }
    // Differing ranks are only reachable when a wildcard is present (the rank
    // guard above already rejected the wildcard-free case). Fall back to a
    // best-effort comparison of the concrete (non-wildcard) dimensions in order.
    let a_eff: Vec<_> = a.iter().filter(|&&d| d != 0).copied().collect();
    let b_eff: Vec<_> = b.iter().filter(|&&d| d != 0).copied().collect();
    a_eff.iter().zip(b_eff.iter()).all(|(d1, d2)| d1 == d2)
}
