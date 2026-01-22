// Shared test helpers for the `shared` crate integration tests.
//
// Each integration-test file is compiled as its own crate, so this module is
// pulled in with `mod common;`. Only some helpers are used by any given file,
// hence the broad `dead_code` allowance.
#![allow(dead_code)]

use shared::FloatVector;

/// Default tolerance for f32-backed computations. `FloatX` is `f32`, so most
/// reductions accumulate noticeable rounding error; 1e-5 is tight enough to
/// catch real regressions while tolerating legitimate float drift.
pub const EPS: f64 = 1e-5;

/// Assert two floats are within `EPS`.
#[track_caller]
pub fn assert_close(a: f64, b: f64) {
    assert_close_eps(a, b, EPS);
}

/// Assert two floats are within an explicit tolerance.
#[track_caller]
pub fn assert_close_eps(a: f64, b: f64, eps: f64) {
    assert!(
        (a - b).abs() <= eps,
        "expected {a} ≈ {b} (|Δ| = {} > {eps})",
        (a - b).abs()
    );
}

/// Assert two slices of floats are elementwise within `EPS`.
#[track_caller]
pub fn assert_slice_close(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len(), "length mismatch: {a:?} vs {b:?}");
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        assert!(
            (*x as f64 - *y as f64).abs() <= EPS,
            "element {i} differs: {x} vs {y} (full: {a:?} vs {b:?})"
        );
    }
}

/// Assert a `FloatVector` matches an expected slice within `EPS`.
#[track_caller]
pub fn assert_vec_close(v: &FloatVector, expected: &[f32]) {
    assert_slice_close(v.as_slice().expect("contiguous vector"), expected);
}

/// Build a `FloatVector` from a slice literal.
pub fn fv(data: &[f32]) -> FloatVector {
    FloatVector::from(data.to_vec())
}
