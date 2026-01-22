// File: shared/src/vectors/matrix_math.rs
//!
//! ## Matrix Math
//!
//! This module provides mathematical and utility functions for `FloatMatrix`.
// Import items from sibling modules using `super::`.
use super::error::VectorError;
use super::types::{FloatCube, FloatVector, FloatX};
use super::vector_math::VectorMathExt;
use ndarray::{s, stack, Array, Axis, Ix2};
/// @brief A trait for mathematical operations on 2D matrices.
pub trait MatrixMathExt {
    /// @brief Extracts a column from the matrix as an owned vector, bounds-checked.
    ///
    /// Named `column_checked` (not `column`) because `ndarray` exposes an
    /// inherent `column(index) -> ArrayView1` that *panics* on an out-of-bounds
    /// index and would shadow a trait method called `column` under method-call
    /// syntax — making this checked, `Result`-returning variant unreachable.
    /// @param col_idx The index of the column to extract.
    /// @return A `Result` containing the `FloatVector` or a `VectorError` if the index is out of bounds.
    fn column_checked(&self, col_idx: usize) -> Result<FloatVector, VectorError>;
    /// @brief Finds the index of the maximum value for each row.
    /// @return A `Vec<(usize, FloatX)>` where each tuple contains the column index and the maximum value for a row.
    fn arg_max_rows(&self) -> Vec<(usize, FloatX)>;
    /// @brief Finds the index of the maximum value for each row, returning only the indices.
    /// @return A `Vec<usize>` where each element is the column index of the maximum value for the corresponding row.
    fn arg_max_rows_indices(&self) -> Vec<usize>;
    /// @brief Groups the matrix rows into batches, forming a cube.
    /// @param sequence_length The number of rows in each batch (the depth of the resulting cube).
    /// @return A `Result` with the new `FloatCube` or a `VectorError` if rows aren't divisible by `sequence_length`.
    fn grouped_by(&self, sequence_length: usize) -> Result<FloatCube, VectorError>;
    /// @brief Creates sequences of rows using a sliding window.
    /// @param sequence_length The length of each sequence.
    /// @param advance_step The step size to advance the window.
    /// @return A `FloatCube` where each matrix is a sequence.
    fn as_sequences(
        &self,
        sequence_length: usize,
        advance_step: usize,
    ) -> Result<FloatCube, VectorError>;
}
impl MatrixMathExt for Array<FloatX, Ix2> {
    fn column_checked(&self, col_idx: usize) -> Result<FloatVector, VectorError> {
        if col_idx >= self.shape()[1] {
            return Err(VectorError::InvalidArgument(format!(
                "Column index {} is out of bounds for matrix with {} columns.",
                col_idx,
                self.shape()[1]
            )));
        }
        // `self.column(..)` resolves to ndarray's inherent column accessor
        // (returning a view), which is exactly what we want to own here.
        Ok(self.column(col_idx).to_owned())
    }
    fn arg_max_rows(&self) -> Vec<(usize, FloatX)> {
        self.rows()
            .into_iter()
            .map(|row| row.to_owned().arg_max())
            .collect()
    }
    fn arg_max_rows_indices(&self) -> Vec<usize> {
        self.rows()
            .into_iter()
            .map(|row| row.to_owned().arg_max().0)
            .collect()
    }
    fn grouped_by(&self, sequence_length: usize) -> Result<FloatCube, VectorError> {
        let (rows, cols) = self.dim();
        if rows == 0 {
            return Ok(Array::zeros((0, 0, 0)));
        }
        if sequence_length == 0 || rows % sequence_length != 0 {
            // Guard the divide: with `sequence_length == 0` the short-circuit above
            // skips the `%` check, but `rows / sequence_length` in the error dims
            // would still panic. Report 0 chunks for the degenerate case rather
            // than crashing the (potentially worker-thread) caller.
            let num_chunks = rows.checked_div(sequence_length).unwrap_or(0);
            return Err(VectorError::ReshapeError {
                size: rows,
                dims: vec![num_chunks, sequence_length],
            });
        }
        let num_chunks = rows / sequence_length;
        self.to_shape((num_chunks, sequence_length, cols))
            .map(|cow| cow.to_owned())
            .map_err(VectorError::from)
    }
    fn as_sequences(
        &self,
        sequence_length: usize,
        advance_step: usize,
    ) -> Result<FloatCube, VectorError> {
        if self.is_empty() {
            return Ok(Array::zeros((0, 0, 0)));
        }
        if sequence_length == 0 || advance_step == 0 {
            return Err(VectorError::InvalidArgument(
                "Sequence length and advance step must be positive.".into(),
            ));
        }
        let n_rows = self.shape()[0];
        let cols = self.shape()[1];
        let mut sequences = Vec::new();
        let mut i = 0;
        while i < n_rows {
            let end = std::cmp::min(i + sequence_length, n_rows);
            let mut sequence = self.slice(s![i..end, ..]).to_owned();
            // Pad if the sequence is shorter than required (at the end)
            if sequence.shape()[0] < sequence_length {
                let padding_rows = sequence_length - sequence.shape()[0];
                if padding_rows > 0 {
                    let padding = Array::zeros((padding_rows, cols));
                    sequence
                        .append(Axis(0), padding.view())
                        .map_err(VectorError::from)?;
                }
            }
            sequences.push(sequence);
            i += advance_step;
        }
        if sequences.is_empty() {
            return Ok(Array::zeros((0, sequence_length, self.shape()[1])));
        }
        let views: Vec<_> = sequences.iter().map(|a| a.view()).collect();
        stack(Axis(0), &views).map_err(VectorError::from)
    }
}
