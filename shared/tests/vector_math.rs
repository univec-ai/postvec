//! End-to-end tests for `shared::vectors::vector_math` — the `VectorMathExt`
//! trait and the free vector-construction/serialisation helpers.

mod common;

use common::{assert_close, assert_close_eps, assert_vec_close, fv};
use shared::{
    deserialize_float_vector, gaussian_vector, gaussian_vector_normalized, new_float_vector,
    random_vector, serialize_float_vector, uniform_vector, FloatVector, VectorError, VectorMathExt,
};

// ---------------------------------------------------------------------------
// Construction / (de)serialisation
// ---------------------------------------------------------------------------

#[test]
fn new_float_vector_fills() {
    let v = new_float_vector(2.5, 4);
    assert_vec_close(&v, &[2.5, 2.5, 2.5, 2.5]);
}

#[test]
fn serialize_deserialize_roundtrip() {
    let v = fv(&[1.0, -2.5, 3.125, 0.0, 42.0]);
    let bytes = serialize_float_vector(&v);
    assert_eq!(bytes.len(), v.len() * 4);
    let back = deserialize_float_vector(&bytes).unwrap();
    assert_vec_close(&back, &[1.0, -2.5, 3.125, 0.0, 42.0]);
}

#[test]
fn deserialize_rejects_non_multiple_of_four() {
    let err = deserialize_float_vector(&[0u8, 1, 2]).unwrap_err();
    assert!(
        matches!(err, VectorError::ConversionError(_)),
        "got {err:?}"
    );
}

#[test]
fn deserialize_empty_is_empty() {
    let v = deserialize_float_vector(&[]).unwrap();
    assert_eq!(v.len(), 0);
}

#[test]
fn uniform_vector_is_deterministic_and_in_range() {
    // `uniform_vector` is seeded with a fixed RNG, so it is reproducible.
    let a = uniform_vector(16);
    let b = uniform_vector(16);
    assert_eq!(a, b);
    assert!(a.iter().all(|&x| (0.0..1.0).contains(&x)));
}

#[test]
fn random_vector_respects_bound() {
    let v = random_vector(1000, 5.0);
    assert!(v.iter().all(|&x| (-5.0..=5.0).contains(&x)));
}

#[test]
fn gaussian_normalized_has_unit_norm() {
    let v = gaussian_vector_normalized(128);
    assert_close_eps(v.norm_l2() as f64, 1.0, 1e-4);
}

#[test]
fn gaussian_vector_has_requested_dim() {
    assert_eq!(gaussian_vector(64).len(), 64);
}

// ---------------------------------------------------------------------------
// Similarity / distance
// ---------------------------------------------------------------------------

#[test]
fn cosine_similarity_identical_orthogonal_opposite() {
    let a = fv(&[1.0, 0.0]);
    let b = fv(&[0.0, 1.0]);
    let neg = fv(&[-1.0, 0.0]);
    assert_close(a.cosine_similarity(&a).unwrap(), 1.0);
    assert_close(a.cosine_similarity(&b).unwrap(), 0.0);
    assert_close(a.cosine_similarity(&neg).unwrap(), -1.0);
}

#[test]
fn cosine_similarity_zero_vector_is_zero() {
    let a = fv(&[0.0, 0.0, 0.0]);
    let b = fv(&[1.0, 2.0, 3.0]);
    assert_close(a.cosine_similarity(&b).unwrap(), 0.0);
}

#[test]
fn cosine_similarity_dimension_mismatch() {
    let a = fv(&[1.0, 2.0]);
    let b = fv(&[1.0, 2.0, 3.0]);
    assert!(matches!(
        a.cosine_similarity(&b),
        Err(VectorError::DimensionMismatch { .. })
    ));
}

#[test]
fn cosine_distance_is_one_minus_similarity() {
    let a = fv(&[1.0, 0.0]);
    let b = fv(&[0.0, 1.0]);
    assert_close(a.cosine_distance(&b).unwrap(), 1.0);
    assert_close(a.cosine_distance(&a).unwrap(), 0.0);
}

#[test]
fn euclidean_distance_basic_and_mismatch() {
    let a = fv(&[0.0, 0.0]);
    let b = fv(&[3.0, 4.0]);
    assert_close(a.euclidean_distance(&b).unwrap(), 5.0);
    assert!(matches!(
        a.euclidean_distance(&fv(&[1.0])),
        Err(VectorError::DimensionMismatch { .. })
    ));
}

#[test]
fn angular_similarity_identical_orthogonal_opposite() {
    let a = fv(&[1.0, 0.0]);
    let b = fv(&[0.0, 1.0]);
    let neg = fv(&[-1.0, 0.0]);
    assert_close(a.angular_similarity(&a).unwrap(), 1.0);
    assert_close_eps(a.angular_similarity(&b).unwrap(), 0.5, 1e-4);
    assert_close_eps(a.angular_similarity(&neg).unwrap(), 0.0, 1e-4);
}

// ---------------------------------------------------------------------------
// Norms / normalisation
// ---------------------------------------------------------------------------

#[test]
fn norm_l2_matches_known() {
    assert_close(fv(&[3.0, 4.0]).norm_l2() as f64, 5.0);
}

#[test]
fn normalize_in_place_unit_norm() {
    let mut v = fv(&[3.0, 4.0]);
    v.normalize_in_place().unwrap();
    assert_close(v.norm_l2() as f64, 1.0);
    assert_vec_close(&v, &[0.6, 0.8]);
}

#[test]
fn normalize_in_place_zero_norm_errors() {
    let mut v = fv(&[0.0, 0.0]);
    assert!(matches!(v.normalize_in_place(), Err(VectorError::ZeroNorm)));
}

#[test]
fn normalized_returns_unit_and_zero_safe() {
    let v = fv(&[0.0, 3.0, 4.0]);
    assert_close(v.normalized().norm_l2() as f64, 1.0);
    // Zero vector does not panic (epsilon guard) and stays all-zero.
    let z = fv(&[0.0, 0.0]).normalized();
    assert_vec_close(&z, &[0.0, 0.0]);
}

// ---------------------------------------------------------------------------
// Reductions / statistics
// ---------------------------------------------------------------------------

#[test]
fn sum_min_max() {
    let v = fv(&[1.0, -2.0, 3.0, 4.0]);
    assert_close(v.sum_f64(), 6.0);
    assert_close(v.min_f64(), -2.0);
    assert_close(v.max_f64(), 4.0);
}

#[test]
fn mean_and_empty_error() {
    assert_close(fv(&[2.0, 4.0, 6.0]).mean_f64().unwrap(), 4.0);
    assert!(matches!(
        FloatVector::from(vec![]).mean_f64(),
        Err(VectorError::EmptyVector)
    ));
}

#[test]
fn mean_f64_is_reachable_via_method_syntax() {
    // Regression guard: `mean_f64` must not collide with ndarray's inherent
    // `mean()`, so this resolves to the trait method returning `Result<f64,_>`.
    let v = fv(&[1.0, 2.0, 3.0, 4.0]);
    let m: Result<f64, VectorError> = v.mean_f64();
    assert_close(m.unwrap(), 2.5);
}

#[test]
fn variance_and_stdev_sample() {
    // Sample (n-1) variance of [1,2,3] is 1.0, stdev 1.0.
    let v = fv(&[1.0, 2.0, 3.0]);
    assert_close(v.variance().unwrap(), 1.0);
    assert_close(v.stdev().unwrap(), 1.0);
    // Single element → variance 0 (no division by zero).
    assert_close(fv(&[5.0]).variance().unwrap(), 0.0);
}

#[test]
fn median_even_and_odd() {
    assert_close(fv(&[3.0, 1.0, 2.0]).median().unwrap(), 2.0);
    assert_close(fv(&[1.0, 2.0, 3.0, 4.0]).median().unwrap(), 2.5);
    assert!(matches!(
        FloatVector::from(vec![]).median(),
        Err(VectorError::EmptyVector)
    ));
}

#[test]
fn percentile_bounds_and_interpolation() {
    let v = fv(&[1.0, 2.0, 3.0, 4.0]);
    assert_close(v.percentile(0.0).unwrap(), 1.0);
    assert_close(v.percentile(100.0).unwrap(), 4.0);
    assert_close(v.percentile(50.0).unwrap(), 2.5);
    assert_close(v.percentile(25.0).unwrap(), 1.75);
    assert!(matches!(
        v.percentile(150.0),
        Err(VectorError::InvalidArgument(_))
    ));
}

#[test]
fn z_scores_and_zero_stdev() {
    let z = fv(&[1.0, 2.0, 3.0]).z_scores().unwrap();
    assert_vec_close(&z, &[-1.0, 0.0, 1.0]);
    // Constant vector → zero stdev → DivisionByZero.
    assert!(matches!(
        fv(&[2.0, 2.0, 2.0]).z_scores(),
        Err(VectorError::DivisionByZero)
    ));
}

// ---------------------------------------------------------------------------
// Activations
// ---------------------------------------------------------------------------

#[test]
fn softmax_sums_to_one_and_orders() {
    let p = fv(&[1.0, 2.0, 3.0]).softmax();
    assert_close_eps(p.sum_f64(), 1.0, 1e-5);
    // Monotonic in the input.
    assert!(p[0] < p[1] && p[1] < p[2]);
}

#[test]
fn softmax_is_shift_invariant() {
    let a = fv(&[1.0, 2.0, 3.0]).softmax();
    let b = fv(&[101.0, 102.0, 103.0]).softmax();
    assert_vec_close(&a, b.as_slice().unwrap());
}

#[test]
fn sigmoid_known_values() {
    let s = fv(&[0.0]).sigmoid();
    assert_close_eps(s[0] as f64, 0.5, 1e-6);
    let big = fv(&[100.0, -100.0]).sigmoid();
    assert_close_eps(big[0] as f64, 1.0, 1e-6);
    assert_close_eps(big[1] as f64, 0.0, 1e-6);
}

// ---------------------------------------------------------------------------
// Index / ordering helpers
// ---------------------------------------------------------------------------

#[test]
fn arg_max_returns_index_and_value() {
    let (idx, val) = fv(&[1.0, 5.0, 3.0, 5.0]).arg_max();
    assert_eq!(idx, 1); // first occurrence of the max
    assert_close(val as f64, 5.0);
}

#[test]
fn arg_sort_descending_order() {
    let idx = fv(&[1.0, 3.0, 2.0]).arg_sort_descending();
    assert_eq!(idx, vec![1, 2, 0]);
}

#[test]
fn cum_sum_accumulates() {
    let c = fv(&[1.0, 2.0, 3.0, 4.0]).cum_sum();
    assert_vec_close(&c, &[1.0, 3.0, 6.0, 10.0]);
}

#[test]
fn sort_in_place_and_sorted() {
    let mut v = fv(&[3.0, 1.0, 2.0]);
    v.sort_in_place(false);
    assert_vec_close(&v, &[1.0, 2.0, 3.0]);
    let d = fv(&[3.0, 1.0, 2.0]).sorted(true);
    assert_vec_close(&d, &[3.0, 2.0, 1.0]);
}

// ---------------------------------------------------------------------------
// Set-like / hashing / misc
// ---------------------------------------------------------------------------

#[test]
fn deduplicate_and_count_uniques() {
    let v = fv(&[1.0, 2.0, 2.0, 3.0, 1.0]);
    assert_vec_close(&v.deduplicate(), &[1.0, 2.0, 3.0]);
    assert_eq!(v.count_uniques(), 3);
}

#[test]
fn custom_hash_stable_and_distinct() {
    let a = fv(&[1.0, 2.0, 3.0]);
    let b = fv(&[1.0, 2.0, 3.0]);
    let c = fv(&[1.0, 2.0, 4.0]);
    assert_eq!(a.custom_hash(), b.custom_hash());
    assert_ne!(a.custom_hash(), c.custom_hash());
}

#[test]
fn size_bytes_is_four_per_element() {
    assert_eq!(fv(&[1.0, 2.0, 3.0]).size_bytes(), 12);
}

#[test]
fn is_zero_detects_all_zero() {
    assert!(fv(&[0.0, 0.0]).is_zero());
    assert!(!fv(&[0.0, 1.0]).is_zero());
}

#[test]
fn element_wise_multiply_and_mismatch() {
    let r = fv(&[1.0, 2.0, 3.0])
        .element_wise_multiply(&fv(&[4.0, 5.0, 6.0]))
        .unwrap();
    assert_vec_close(&r, &[4.0, 10.0, 18.0]);
    assert!(matches!(
        fv(&[1.0]).element_wise_multiply(&fv(&[1.0, 2.0])),
        Err(VectorError::DimensionMismatch { .. })
    ));
}

#[test]
fn as_f64_and_i64_vecs() {
    let v = fv(&[1.7, 2.2, -3.9]);
    assert_eq!(v.as_i64_vec(), vec![1, 2, -3]); // truncation toward zero
    let f = v.as_f64_vec();
    assert_eq!(f.len(), 3);
    assert_close(f[0], 1.7);
}

// ---------------------------------------------------------------------------
// Scaling
// ---------------------------------------------------------------------------

#[test]
fn scale_to_unit_values() {
    let r = fv(&[1.0, 2.0, 4.0]).scale_to_unit_values().unwrap();
    assert_vec_close(&r, &[0.25, 0.5, 1.0]);
    assert!(matches!(
        fv(&[0.0, 0.0]).scale_to_unit_values(),
        Err(VectorError::DivisionByZero)
    ));
}

#[test]
fn scale_to_unit_sum() {
    let r = fv(&[1.0, 1.0, 2.0]).scale_to_unit_sum().unwrap();
    assert_vec_close(&r, &[0.25, 0.25, 0.5]);
    assert!(matches!(
        fv(&[0.0, 0.0]).scale_to_unit_sum(),
        Err(VectorError::DivisionByZero)
    ));
}

#[test]
fn quantile_slice_filters_to_range() {
    let v = fv(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    // Between the 25th (=2.0) and 75th (=4.0) percentiles, inclusive.
    let r = v.quantile_slice(25.0, 75.0).unwrap();
    assert_vec_close(&r, &[2.0, 3.0, 4.0]);
    // Arguments may be supplied in either order.
    let r2 = v.quantile_slice(75.0, 25.0).unwrap();
    assert_vec_close(&r2, &[2.0, 3.0, 4.0]);
}

// ---------------------------------------------------------------------------
// In-place fills
// ---------------------------------------------------------------------------

#[test]
fn fill_in_place_sets_all() {
    let mut v = fv(&[0.0, 0.0, 0.0]);
    v.fill_in_place(7.0);
    assert_vec_close(&v, &[7.0, 7.0, 7.0]);
}

#[test]
fn fill_uniform_in_place_in_range() {
    let mut v = new_float_vector(0.0, 256);
    v.fill_uniform_in_place();
    assert!(v.iter().all(|&x| (0.0..1.0).contains(&x)));
}

#[test]
fn fill_gaussian_in_place_changes_values() {
    let mut v = new_float_vector(0.0, 256);
    v.fill_gaussian_in_place();
    assert!(!v.is_zero());
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

#[test]
fn multinomial_returns_requested_count_in_range() {
    let probs = fv(&[0.1, 0.2, 0.7]);
    let samples = probs.multinomial(50);
    assert_eq!(samples.len(), 50);
    assert!(samples.iter().all(|&i| i < 3));
}

#[test]
fn multinomial_zero_distribution_returns_zeros() {
    let samples = fv(&[0.0, 0.0, 0.0]).multinomial(5);
    assert_eq!(samples, vec![0; 5]);
}

#[test]
fn temperature_sampling_validates_args() {
    let logits = fv(&[1.0, 2.0, 3.0]);
    assert_eq!(logits.temperature_sampling(1.0, 10).unwrap().len(), 10);
    assert!(matches!(
        logits.temperature_sampling(0.0, 10),
        Err(VectorError::InvalidArgument(_))
    ));
    assert!(matches!(
        FloatVector::from(vec![]).temperature_sampling(1.0, 1),
        Err(VectorError::EmptyVector)
    ));
}

#[test]
fn nucleus_sampling_validates_and_samples() {
    let logits = fv(&[1.0, 2.0, 3.0, 4.0]);
    let s = logits.nucleus_sampling(0.9, 1.0, 20).unwrap();
    assert_eq!(s.len(), 20);
    assert!(s.iter().all(|&i| i < 4));
    assert!(matches!(
        logits.nucleus_sampling(1.5, 1.0, 1),
        Err(VectorError::InvalidArgument(_))
    ));
    assert!(matches!(
        FloatVector::from(vec![]).nucleus_sampling(0.5, 1.0, 1),
        Err(VectorError::EmptyVector)
    ));
}

#[test]
fn random_sample_with_and_without_replacement() {
    let v = fv(&[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(v.random_sample(10, true).unwrap().len(), 10);
    assert_eq!(v.random_sample(3, false).unwrap().len(), 3);
    // Without replacement, cannot request more than the vector length.
    assert!(matches!(
        v.random_sample(5, false),
        Err(VectorError::InvalidArgument(_))
    ));
}
