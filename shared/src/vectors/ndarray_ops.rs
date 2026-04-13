//! Convert nested `Vec`s into ndarray tensors; reshape and expand dims.

// The `AsFloat{Vector,Matrix,Cube,Quad,Penta}` traits are *consuming*
// conversions (they take `self` by value, like `into_*`) but are deliberately
// named `as_*` for symmetry with the wider codebase's tensor-conversion API
// (~30 call sites across the engine executors). Renaming to `into_*` is the
// idiomatic fix but a wide, churny change with no behavioural benefit, so we
// silence the self-convention lint for this module instead.
#![allow(clippy::wrong_self_convention)]

use super::error::VectorError;
use super::types::{FloatCube, FloatMatrix, FloatPenta, FloatQuad, FloatVector, FloatX};
use ndarray::{Array, Axis, Ix1, Ix2, Ix3, Ix4, Ix5};
use serde_json::Value;

pub trait AsFloatVector {
    fn as_float_vector(self) -> Result<FloatVector, VectorError>;
}

pub trait AsFloatMatrix {
    fn as_float_matrix(self) -> Result<FloatMatrix, VectorError>;
}

pub trait AsFloatCube {
    fn as_float_cube(self) -> Result<FloatCube, VectorError>;
}

pub trait AsFloatQuad {
    fn as_float_quad(self) -> Result<FloatQuad, VectorError>;
}

pub trait AsFloatPenta {
    fn as_float_penta(self) -> Result<FloatPenta, VectorError>;
}

// --- Trait Implementations for Standard Types ---

impl<T> AsFloatVector for Vec<T>
where
    T: Into<FloatX> + Copy,
{
    fn as_float_vector(self) -> Result<FloatVector, VectorError> {
        Ok(Array::from_vec(
            self.into_iter().map(|v| v.into()).collect(),
        ))
    }
}

impl AsFloatVector for &Value {
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

/// Expand dims, flatten, and read shape on `ndarray::Array`.
pub trait NdArrayExt {
    type LargerDim;

    /// Insert a leading axis of size 1. `(3, 4)` becomes `(1, 3, 4)`.
    fn expand_dimension(self) -> Self::LargerDim;

    fn flatten_to_vector(self) -> FloatVector;

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
    /// A placeholder type, as expanding a 5D tensor is unsupported here.
    type LargerDim = ();

    /// Not supported for 5D tensors in this context.
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

/// Reshape a 1D vector into 2D-5D when the element count matches.
pub trait ReshapeExt {
    fn reshape_2d(self, rows: usize, cols: usize) -> Result<FloatMatrix, VectorError>;

    fn reshape_3d(self, d1: usize, d2: usize, d3: usize) -> Result<FloatCube, VectorError>;

    fn reshape_4d(
        self,
        d1: usize,
        d2: usize,
        d3: usize,
        d4: usize,
    ) -> Result<FloatQuad, VectorError>;

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

/// Shape compatibility. `0` is a wildcard. When `dimension_check` is true,
/// non-wildcard sizes must match at the same position.
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
