// File: shared/src/vectors/vector_math.rs
//! ## Vector Math and Utilities
//!
//!
//! This module provides extensive mathematical and utility functions for `FloatVector`.
//! Operations include statistical calculations, similarity metrics, normalization,
//! and random vector generation.
//!
// Import from sibling modules.
use super::error::VectorError;
use super::types::{FloatVector, FloatX, INF, NEG_INF};
use byteorder::{ByteOrder, LittleEndian};
use ndarray::{Array, Ix1};
use rand::distributions::{Distribution, Uniform};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::StandardNormal;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

const RANDOM_SEED: u64 = 1234;

/// @brief Creates a new float vector of a given size, filled with a specific value.
/// @param fill_value The `FloatX` value to fill the vector with.
/// @param size The size of the new vector.
/// @return A new `FloatVector`.
pub fn new_float_vector(fill_value: FloatX, size: usize) -> FloatVector {
    Array::from_elem(size, fill_value)
}
/// @brief Creates a new `FloatVector` from a slice of `f64` values.
/// @param values A slice of `f64` values.
/// @return A new `FloatVector`.
pub fn new_float_vector_from_float64(values: &[f64]) -> FloatVector {
    values.iter().map(|&v| v as FloatX).collect()
}
/// @brief Serializes a `FloatVector` into a byte vector.
///
/// The serialization uses Little Endian byte order.
/// @param v A reference to the `FloatVector` to serialize.
/// @return A `Vec<u8>` containing the serialized vector data.
pub fn serialize_float_vector(v: &FloatVector) -> Vec<u8> {
    let mut bytes = vec![0; v.len() * 4];
    // This is safe because FloatX is f32. The explicit `as *const f32` is kept
    // intentionally: it pins the element type that the unsafe `from_raw_parts` +
    // `write_f32_into` below rely on, so this code fails loudly rather than
    // silently misreinterpreting memory if `FloatX` is ever changed away from f32.
    #[allow(clippy::unnecessary_cast)]
    let slice_f32: &[f32] =
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const f32, v.len()) };
    LittleEndian::write_f32_into(slice_f32, &mut bytes);
    bytes
}
/// @brief Deserializes a byte slice into a `FloatVector`.
///
/// The byte slice is expected to be in Little Endian format. The length of the
/// slice must be a multiple of 4.
/// @param bytes A slice of bytes to deserialize.
/// @return A `Result` containing the `FloatVector` or a `VectorError`.
pub fn deserialize_float_vector(bytes: &[u8]) -> Result<FloatVector, VectorError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(VectorError::ConversionError(
            "Byte slice length is not a multiple of 4".into(),
        ));
    }
    let mut vec_f32 = vec![0.0; bytes.len() / 4];
    LittleEndian::read_f32_into(bytes, &mut vec_f32);
    Ok(Array::from(vec_f32))
}
// --- Random Vector Generation ---
/// @brief Generates a vector with elements from a uniform distribution [0, 1).
/// @param dim The dimension (size) of the vector.
/// @return A new `FloatVector`.
pub fn uniform_vector(dim: usize) -> FloatVector {
    let mut rng = StdRng::seed_from_u64(RANDOM_SEED);
    let dist = Uniform::new(0.0, 1.0);
    Array::from_iter(dist.sample_iter(&mut rng).take(dim).map(|x| x as FloatX))
}
/// @brief Generates a vector with random elements in a given range.
///
/// Elements are distributed uniformly in `[-top_bound, top_bound]`.
/// @param dim The dimension (size) of the vector.
/// @param top_bound The upper bound for the random value range.
/// @return A new `FloatVector`.
pub fn random_vector(dim: usize, top_bound: f64) -> FloatVector {
    let mut rng = rand::thread_rng();
    Array::from_iter((0..dim).map(|_| ((rng.gen::<f64>() * 2.0 - 1.0) * top_bound) as FloatX))
}
/// @brief Generates a vector with elements from a standard Gaussian (normal) distribution.
/// @param dim The dimension (size) of the vector.
/// @return A new `FloatVector`.
pub fn gaussian_vector(dim: usize) -> FloatVector {
    let mut rng = rand::thread_rng();
    Array::from_iter(
        StandardNormal
            .sample_iter(&mut rng)
            .take(dim)
            .map(|x: f64| x as FloatX),
    )
}
/// @brief Generates a normalized vector from a Gaussian distribution.
///
/// The resulting vector will have a unit L2 norm.
/// @param dim The dimension (size) of the vector.
/// @return A new, normalized `FloatVector`.
pub fn gaussian_vector_normalized(dim: usize) -> FloatVector {
    let vec = gaussian_vector(dim);
    let norm = vec.norm_l2();
    if norm > 1e-9 {
        vec / norm
    } else {
        vec
    }
}
/// @brief A trait providing extensive mathematical operations for float vectors.
pub trait VectorMathExt {
    /// @brief Computes the cosine similarity between two vectors.
    /// @param other The other vector.
    /// @return A `Result` containing the similarity score (f64) or a `VectorError`.
    fn cosine_similarity(&self, other: &Self) -> Result<f64, VectorError>;
    /// @brief Computes the L2 norm (Euclidean norm) of the vector.
    /// @return The L2 norm as a `FloatX`.
    fn norm_l2(&self) -> FloatX;
    /// @brief Normalizes the vector in-place to have a unit L2 norm.
    /// @return A `Result` indicating success or a `VectorError::ZeroNorm`.
    fn normalize_in_place(&mut self) -> Result<(), VectorError>;
    /// @brief Returns a new, normalized version of the vector with unit L2 norm.
    /// @return A new `FloatVector`.
    fn normalized(&self) -> FloatVector;
    /// @brief Computes the sum of all elements in the vector.
    /// @return The sum as an `f64`.
    fn sum_f64(&self) -> f64;
    /// @brief Finds the maximum element in the vector.
    /// @return The maximum value as an `f64`.
    fn max_f64(&self) -> f64;
    /// @brief Finds the minimum element in the vector.
    /// @return The minimum value as an `f64`.
    fn min_f64(&self) -> f64;
    /// @brief Calculates the mean (average) of the vector's elements.
    ///
    /// Named `mean_f64` (not `mean`) for the same reason as `sum_f64`/`max_f64`/
    /// `min_f64`: `ndarray` provides an inherent `mean(&self) -> Option<A>` that
    /// would shadow a trait method called `mean` under method-call syntax,
    /// silently giving callers `Option<f32>` instead of this `Result<f64, _>`.
    /// @return A `Result` containing the mean as an `f64` or a `VectorError::EmptyVector`.
    fn mean_f64(&self) -> Result<f64, VectorError>;
    /// @brief Calculates the sample variance of the vector's elements.
    /// @return A `Result` containing the variance as an `f64` or an error.
    fn variance(&self) -> Result<f64, VectorError>;
    /// @brief Calculates the sample standard deviation of the vector's elements.
    /// @return A `Result` containing the standard deviation as an `f64` or an error.
    fn stdev(&self) -> Result<f64, VectorError>;
    /// @brief Applies the Softmax function to the vector.
    /// @return A new `FloatVector` of probabilities.
    fn softmax(&self) -> FloatVector;
    /// @brief Finds the index and value of the maximum element.
    /// @return A tuple `(usize, FloatX)` of the index and value.
    fn arg_max(&self) -> (usize, FloatX);
    /// @brief Converts the vector to a `Vec<f64>`.
    /// @return A `Vec<f64>` containing the vector's data.
    fn as_f64_vec(&self) -> Vec<f64>;
    /// @brief Samples indices from the vector, treating its values as probabilities.
    /// @param num_samples The number of indices to sample.
    /// @return A `Vec<usize>` of sampled indices.
    fn multinomial(&self, num_samples: usize) -> Vec<usize>;
    /// @brief Performs temperature-based sampling on the vector (logits).
    /// @param temperature The temperature for scaling. Must be positive.
    /// @param num_samples The number of samples to draw.
    /// @return A `Result` containing a `Vec<usize>` of sampled indices or an error.
    fn temperature_sampling(
        &self,
        temperature: f64,
        num_samples: usize,
    ) -> Result<Vec<usize>, VectorError>;
    /// @brief Performs nucleus (top-p) sampling on the vector (logits).
    /// @param top_p The cumulative probability threshold. Must be in [0.0, 1.0].
    /// @param temperature The temperature for scaling.
    /// @param num_samples The number of samples to draw.
    /// @return A `Result` containing a `Vec<usize>` of sampled indices or an error.
    fn nucleus_sampling(
        &self,
        top_p: f64,
        temperature: f64,
        num_samples: usize,
    ) -> Result<Vec<usize>, VectorError>;
    /// @brief Computes the median of the vector's elements.
    /// @return A `Result` with the median value or an error if the vector is empty.
    fn median(&self) -> Result<f64, VectorError>;
    /// @brief Calculates the p-th percentile of the vector's elements.
    /// @param p The percentile to compute (0.0 to 100.0).
    /// @return A `Result` with the percentile value or an error.
    fn percentile(&self, p: f64) -> Result<f64, VectorError>;
    /// @brief Checks if all elements of the vector are zero.
    /// @return `true` if all elements are zero, `false` otherwise.
    fn is_zero(&self) -> bool;
    /// @brief Computes the Euclidean distance to another vector.
    /// @param other The other vector.
    /// @return A `Result` with the distance or an error for dimension mismatch.
    fn euclidean_distance(&self, other: &Self) -> Result<f64, VectorError>;
    /// @brief Computes the cosine distance (1 - cosine similarity).
    /// @param other The other vector.
    /// @return A `Result` with the distance or an error.
    fn cosine_distance(&self, other: &Self) -> Result<f64, VectorError>;
    /// @brief Applies the sigmoid function element-wise.
    /// @return A new `FloatVector` with the transformed values.
    fn sigmoid(&self) -> FloatVector;
    /// @brief Computes the cumulative sum of the vector's elements.
    /// @return A new `FloatVector` containing the cumulative sum.
    fn cum_sum(&self) -> FloatVector;
    /// @brief Sorts the vector's indices in descending order based on their values.
    /// @return A `Vec<usize>` of indices sorted by the vector's values.
    fn arg_sort_descending(&self) -> Vec<usize>;
    /// @brief Computes the Z-scores (standard scores) for each element.
    ///
    /// The Z-score is the number of standard deviations by which the value of a
    /// raw score is above or below the mean value of what is being observed.
    /// @return A `Result` with a new `FloatVector` of Z-scores or an error if the
    ///         standard deviation is zero.
    fn z_scores(&self) -> Result<FloatVector, VectorError>;
    /// @brief Computes the element-wise multiplication of two vectors.
    /// @param other The vector to multiply with.
    /// @return A `Result` with the resulting `FloatVector` or an error.
    fn element_wise_multiply(&self, other: &Self) -> Result<FloatVector, VectorError>;
    /// @brief Sorts the vector in place.
    /// @param descending If `true`, sort in descending order; otherwise, ascending.
    fn sort_in_place(&mut self, descending: bool);
    /// @brief Returns a new, sorted version of the vector.
    /// @param descending If `true`, sort in descending order; otherwise, ascending.
    /// @return A new, sorted `FloatVector`.
    fn sorted(&self, descending: bool) -> FloatVector;
    /// @brief Removes duplicate elements, preserving the first occurrence.
    /// @return A new `FloatVector` with unique elements.
    fn deduplicate(&self) -> FloatVector;
    /// @brief Computes a hash of the vector's contents.
    /// @return A `u64` hash value.
    fn custom_hash(&self) -> u64;
    /// @brief Gets the size of the vector in bytes.
    /// @return The size in bytes as `usize`.
    fn size_bytes(&self) -> usize;
    /// @brief Creates a random sample of elements from the vector.
    /// @param num_samples The number of items to sample.
    /// @param with_replacement If `true`, samples with replacement.
    /// @return A `Result` containing the sampled `FloatVector` or an error.
    fn random_sample(
        &self,
        num_samples: usize,
        with_replacement: bool,
    ) -> Result<FloatVector, VectorError>;
    // --- NEWLY PORTED FUNCTIONS ---
    /// @brief Prints the first `n` values of the vector to the console for debugging.
    /// @param message A descriptive message to print alongside the values.
    /// @param n The maximum number of elements to print.
    fn print_values(&self, message: &str, n: usize);
    /// @brief Creates a new vector containing only the values between two percentiles.
    /// @param low_q The lower percentile bound (0.0 to 100.0).
    /// @param high_q The upper percentile bound (0.0 to 100.0).
    /// @return A `Result` containing the new `FloatVector` or an error.
    fn quantile_slice(&self, low_q: f64, high_q: f64) -> Result<FloatVector, VectorError>;
    /// @brief Computes the angular similarity between two vectors.
    ///
    /// The angular similarity is defined as `1 - (arccos(cosine_similarity) / PI)`.
    /// It ranges from 0 (opposite) to 1 (identical).
    /// @param other The other vector.
    /// @return A `Result` containing the angular similarity or an error.
    fn angular_similarity(&self, other: &Self) -> Result<f64, VectorError>;
    /// @brief Fills the vector in-place with a specified value.
    /// @param value The value to fill the vector with.
    fn fill_in_place(&mut self, value: FloatX);
    /// @brief Fills the vector in-place with random values from a standard Gaussian distribution.
    fn fill_gaussian_in_place(&mut self);
    /// @brief Fills the vector in-place with random values from a uniform distribution [0, 1).
    fn fill_uniform_in_place(&mut self);
    /// @brief Scales the vector so that its maximum value is 1.0.
    /// @return A `Result` with the new scaled `FloatVector` or an error if the max value is zero or invalid.
    fn scale_to_unit_values(&self) -> Result<FloatVector, VectorError>;
    /// @brief Scales the vector so that its elements' sum is 1.0.
    /// @return A `Result` with the new scaled `FloatVector` or an error if the sum is zero or invalid.
    fn scale_to_unit_sum(&self) -> Result<FloatVector, VectorError>;
    /// @brief Counts the number of unique elements in the vector.
    /// @return The count of unique elements.
    fn count_uniques(&self) -> usize;
    /// @brief Converts the vector to a `Vec<i64>`, casting each element.
    /// @return A `Vec<i64>` containing the vector's data.
    fn as_i64_vec(&self) -> Vec<i64>;
}

impl VectorMathExt for Array<FloatX, Ix1> {
    fn cosine_similarity(&self, other: &Self) -> Result<f64, VectorError> {
        if self.len() != other.len() {
            return Err(VectorError::DimensionMismatch {
                expected: vec![self.len()],
                actual: vec![other.len()],
            });
        }
        let norm_a = self.norm_l2();
        let norm_b = other.norm_l2();
        if norm_a < 1e-9 || norm_b < 1e-9 {
            return Ok(0.0);
        }
        let dot_product = self.dot(other);
        Ok((dot_product / (norm_a * norm_b)) as f64)
    }

    fn norm_l2(&self) -> FloatX {
        self.dot(self).sqrt()
    }

    fn normalize_in_place(&mut self) -> Result<(), VectorError> {
        let norm = self.norm_l2();
        if norm < 1e-9 {
            return Err(VectorError::ZeroNorm);
        }
        *self /= norm;
        Ok(())
    }

    fn normalized(&self) -> FloatVector {
        let norm = self.norm_l2();
        // Add a small epsilon to avoid division by zero.
        let norm = if norm == 0.0 { 1e-9 } else { norm };
        self / norm
    }

    fn sum_f64(&self) -> f64 {
        self.iter().map(|&x| x as f64).sum()
    }

    fn max_f64(&self) -> f64 {
        self.iter().fold(NEG_INF as FloatX, |acc, &x| acc.max(x)) as f64
    }

    fn min_f64(&self) -> f64 {
        self.iter().fold(INF as FloatX, |acc, &x| acc.min(x)) as f64
    }

    fn mean_f64(&self) -> Result<f64, VectorError> {
        if self.is_empty() {
            return Err(VectorError::EmptyVector);
        }
        Ok(self.sum_f64() / self.len() as f64)
    }

    fn variance(&self) -> Result<f64, VectorError> {
        if self.len() <= 1 {
            return Ok(0.0);
        }
        let mean = <Self as VectorMathExt>::mean_f64(self)?;
        let var =
            self.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / (self.len() - 1) as f64;
        Ok(var)
    }

    fn stdev(&self) -> Result<f64, VectorError> {
        self.variance().map(|v| v.sqrt())
    }

    fn softmax(&self) -> FloatVector {
        let max_val = self.max_f64() as FloatX;
        let mut exp_vec: FloatVector = self.iter().map(|&x| (x - max_val).exp()).collect();
        let sum = exp_vec.sum();
        if sum > 1e-9 {
            exp_vec /= sum;
        }
        exp_vec
    }

    fn arg_max(&self) -> (usize, FloatX) {
        self.iter()
            .enumerate()
            .fold((0, NEG_INF as FloatX), |(idx_max, v_max), (idx, &v)| {
                if v > v_max {
                    (idx, v)
                } else {
                    (idx_max, v_max)
                }
            })
    }

    fn as_f64_vec(&self) -> Vec<f64> {
        self.iter().map(|&x| x as f64).collect()
    }

    fn multinomial(&self, num_samples: usize) -> Vec<usize> {
        if self.is_empty() || self.sum_f64().abs() < 1e-9 {
            return vec![0; num_samples];
        }
        let mut rng = StdRng::seed_from_u64(RANDOM_SEED);
        let cdf: Vec<FloatX> = self
            .iter()
            .scan(0.0, |acc, &x| {
                *acc += x;
                Some(*acc)
            })
            .collect();
        let sum = cdf.last().cloned().unwrap_or(0.0);
        if sum <= 0.0 {
            return vec![0; num_samples];
        }
        (0..num_samples)
            .map(|_| {
                let r = rng.gen::<FloatX>() * sum;
                cdf.iter().position(|&p| r <= p).unwrap_or(self.len() - 1) // Fallback to last index
            })
            .collect()
    }

    fn temperature_sampling(
        &self,
        temperature: f64,
        num_samples: usize,
    ) -> Result<Vec<usize>, VectorError> {
        if self.is_empty() {
            return Err(VectorError::EmptyVector);
        }
        if temperature <= 0.0 {
            return Err(VectorError::InvalidArgument(
                "Temperature must be positive.".into(),
            ));
        }
        let scaled_vector = self / (temperature as FloatX);
        let probs = scaled_vector.softmax();
        Ok(probs.multinomial(num_samples))
    }

    fn nucleus_sampling(
        &self,
        top_p: f64,
        temperature: f64,
        num_samples: usize,
    ) -> Result<Vec<usize>, VectorError> {
        if self.is_empty() {
            return Err(VectorError::EmptyVector);
        }
        if !(0.0..=1.0).contains(&top_p) {
            return Err(VectorError::InvalidArgument(
                "top_p must be between 0.0 and 1.0.".into(),
            ));
        }
        let scaled_vector = if temperature > 0.0 {
            self / (temperature as FloatX)
        } else {
            self.clone()
        };
        let mut probs_with_indices: Vec<_> =
            scaled_vector.softmax().into_iter().enumerate().collect();
        // Sort probabilities in descending order
        probs_with_indices
            .sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // Find the set of tokens whose cumulative probability exceeds top_p
        let last_idx_to_keep = probs_with_indices
            .iter()
            .scan(0.0, |state, &(_, prob)| {
                *state += prob;
                Some(*state)
            })
            .position(|p| p >= top_p as FloatX)
            .unwrap_or(probs_with_indices.len() - 1);
        // Create a new probability distribution with only the top-p tokens
        let mut final_probs = Array::zeros(self.len());
        let mut truncated_sum = 0.0;
        for &(idx, prob) in probs_with_indices.iter().take(last_idx_to_keep + 1) {
            final_probs[idx] = prob;
            truncated_sum += prob;
        }
        if truncated_sum > 0.0 {
            final_probs /= truncated_sum;
        }
        Ok(final_probs.multinomial(num_samples))
    }

    fn median(&self) -> Result<f64, VectorError> {
        if self.is_empty() {
            return Err(VectorError::EmptyVector);
        }
        let mut sorted = self.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            Ok(((sorted[mid - 1] + sorted[mid]) / 2.0) as f64)
        } else {
            Ok(sorted[mid] as f64)
        }
    }

    fn percentile(&self, p: f64) -> Result<f64, VectorError> {
        if !(0.0..=100.0).contains(&p) {
            return Err(VectorError::InvalidArgument(
                "Percentile must be between 0 and 100.".into(),
            ));
        }
        if self.is_empty() {
            return Err(VectorError::EmptyVector);
        }
        let mut sorted = self.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        if p == 100.0 {
            return Ok(*sorted.last().unwrap() as f64);
        }
        if p == 0.0 {
            return Ok(*sorted.first().unwrap() as f64);
        }
        let n = sorted.len();
        let rank = (p / 100.0) * (n as f64 - 1.0);
        let lower_idx = rank.floor() as usize;
        let upper_idx = rank.ceil() as usize;
        if lower_idx >= n {
            return Ok(*sorted.last().unwrap() as f64);
        }
        if upper_idx >= n {
            return Ok(*sorted.last().unwrap() as f64);
        }
        let d = rank - lower_idx as f64;
        if lower_idx == upper_idx {
            Ok(sorted[lower_idx] as f64)
        } else {
            let lower_val = sorted[lower_idx] as f64;
            let upper_val = sorted[upper_idx] as f64;
            Ok(lower_val + d * (upper_val - lower_val))
        }
    }

    fn is_zero(&self) -> bool {
        self.iter().all(|&x| x == 0.0)
    }

    fn euclidean_distance(&self, other: &Self) -> Result<f64, VectorError> {
        if self.len() != other.len() {
            return Err(VectorError::DimensionMismatch {
                expected: vec![self.len()],
                actual: vec![other.len()],
            });
        }
        let diff = self - other;
        Ok(diff.norm_l2() as f64)
    }

    fn cosine_distance(&self, other: &Self) -> Result<f64, VectorError> {
        self.cosine_similarity(other).map(|sim| 1.0 - sim)
    }

    fn sigmoid(&self) -> FloatVector {
        self.mapv(|x| 1.0 / (1.0 + (-x).exp()))
    }

    fn cum_sum(&self) -> FloatVector {
        let mut sum = 0.0;
        self.map(|&x| {
            sum += x;
            sum
        })
    }

    fn arg_sort_descending(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.len()).collect();
        indices.sort_by(|&a, &b| self[b].partial_cmp(&self[a]).unwrap_or(Ordering::Equal));
        indices
    }

    fn z_scores(&self) -> Result<FloatVector, VectorError> {
        // Use Universal Function Call Syntax (UFCS) to disambiguate which
        // `mean` and `stdev` methods to call, ensuring we use our custom
        // trait's implementation that returns a Result.
        let mean = <Self as VectorMathExt>::mean_f64(self)? as FloatX;
        let stdev = <Self as VectorMathExt>::stdev(self)? as FloatX;
        if stdev < 1e-9 {
            return Err(VectorError::DivisionByZero);
        }
        // Apply the Z-score formula to each element.
        Ok(self.mapv(|x| (x - mean) / stdev))
    }

    fn element_wise_multiply(&self, other: &Self) -> Result<FloatVector, VectorError> {
        if self.len() != other.len() {
            return Err(VectorError::DimensionMismatch {
                expected: vec![self.len()],
                actual: vec![other.len()],
            });
        }
        Ok(self * other)
    }

    fn sort_in_place(&mut self, descending: bool) {
        let mut temp_vec = self.to_vec();
        temp_vec.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
        if descending {
            temp_vec.reverse();
        }
        *self = Array::from(temp_vec);
    }

    fn sorted(&self, descending: bool) -> FloatVector {
        let mut clone = self.clone();
        clone.sort_in_place(descending);
        clone
    }

    fn deduplicate(&self) -> FloatVector {
        let mut seen = HashSet::new();
        let mut unique_vec = Vec::new();
        for &item in self {
            // Hash f32 by its bit representation for reliable hashing
            if seen.insert(item.to_bits()) {
                unique_vec.push(item);
            }
        }
        Array::from(unique_vec)
    }

    fn custom_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for &val in self {
            val.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }

    fn size_bytes(&self) -> usize {
        self.len() * std::mem::size_of::<FloatX>()
    }

    fn random_sample(
        &self,
        num_samples: usize,
        with_replacement: bool,
    ) -> Result<FloatVector, VectorError> {
        if !with_replacement && num_samples > self.len() {
            return Err(VectorError::InvalidArgument(
                "Sample size cannot be larger than vector size without replacement.".into(),
            ));
        }
        if self.is_empty() {
            return Ok(Array::from(vec![]));
        }
        let mut rng = rand::thread_rng();
        let mut sampled_vec = Vec::with_capacity(num_samples);
        if with_replacement {
            let dist = Uniform::new(0, self.len());
            for _ in 0..num_samples {
                let index = dist.sample(&mut rng);
                sampled_vec.push(self[index]);
            }
        } else {
            let indices: Vec<usize> = (0..self.len()).collect();
            let chosen_indices = rand::seq::index::sample(&mut rng, self.len(), num_samples);
            for index in chosen_indices.iter() {
                sampled_vec.push(self[indices[index]]);
            }
        }
        Ok(Array::from(sampled_vec))
    }

    // --- NEWLY PORTED FUNCTION IMPLEMENTATIONS ---
    fn print_values(&self, message: &str, n: usize) {
        let num_elements = std::cmp::min(n, self.len());
        let slice = self.slice(ndarray::s![..num_elements]);
        println!("{}: {:?}...", message, slice.to_vec());
    }

    fn quantile_slice(&self, low_q: f64, high_q: f64) -> Result<FloatVector, VectorError> {
        let lower_bound = self.percentile(low_q.min(high_q))? as FloatX;
        let upper_bound = self.percentile(low_q.max(high_q))? as FloatX;
        let filtered: Vec<FloatX> = self
            .iter()
            .filter(|&&v| v >= lower_bound && v <= upper_bound)
            .cloned()
            .collect();
        Ok(filtered.into())
    }

    fn angular_similarity(&self, other: &Self) -> Result<f64, VectorError> {
        let cos_sim = self.cosine_similarity(other)?;
        // Clamp the cosine similarity to the valid range [-1.0, 1.0] to avoid NaN from acos.
        // Deliberately NOT `f64::clamp`: `max(-1.0).min(1.0)` flattens a NaN input to -1.0
        // (IEEE max/min ignore NaN), whereas `clamp` propagates NaN — which would feed
        // `acos(NaN)` and reintroduce the exact NaN this guard exists to prevent.
        #[allow(clippy::manual_clamp)]
        let clamped_cos_sim = cos_sim.max(-1.0).min(1.0);
        let angle = clamped_cos_sim.acos();
        Ok(1.0 - (angle / std::f64::consts::PI))
    }

    fn fill_in_place(&mut self, value: FloatX) {
        self.fill(value);
    }

    fn fill_gaussian_in_place(&mut self) {
        let mut rng = rand::thread_rng();
        // The StandardNormal distribution is generic. We explicitly sample an f64
        // to match the original Go implementation's use of NormFloat64() and then cast it.
        // This resolves the type ambiguity for the compiler.
        let dist = StandardNormal;
        self.iter_mut().for_each(|elem| {
            let sample_f64: f64 = dist.sample(&mut rng);
            *elem = sample_f64 as FloatX;
        });
    }

    fn fill_uniform_in_place(&mut self) {
        let mut rng = rand::thread_rng();
        let dist = Uniform::new(0.0, 1.0);
        self.iter_mut().for_each(|elem| {
            *elem = dist.sample(&mut rng) as FloatX;
        });
    }

    fn scale_to_unit_values(&self) -> Result<FloatVector, VectorError> {
        let max_val = self.max_f64() as FloatX;
        if max_val.abs() < 1e-9 {
            return Err(VectorError::DivisionByZero);
        }
        Ok(self / max_val)
    }

    fn scale_to_unit_sum(&self) -> Result<FloatVector, VectorError> {
        let sum = self.sum_f64() as FloatX;
        if sum.abs() < 1e-9 {
            return Err(VectorError::DivisionByZero);
        }
        Ok(self / sum)
    }

    fn count_uniques(&self) -> usize {
        let mut seen = HashSet::with_capacity(self.len());
        for &item in self {
            seen.insert(item.to_bits());
        }
        seen.len()
    }

    fn as_i64_vec(&self) -> Vec<i64> {
        self.iter().map(|&x| x as i64).collect()
    }
}
