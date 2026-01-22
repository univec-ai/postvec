//! End-to-end tests for `shared::vectors::wordvectors` — the `WordVectorsModel`,
//! the binary word2vec loader, and the matrix aggregation helpers.

mod common;

use shared::vectors::wordvectors::WordVectorsModel;
use shared::{
    average_vector, average_vector_with_dimension, load_binary_word_vectors, weighted_vector,
    weighted_vector_with_dimension, FloatMatrix, FloatVector, VectorError,
};
use std::collections::HashMap;
use std::io::Write;

/// Build a small model with unit-length axis vectors keyed by lowercase words.
fn axis_model() -> WordVectorsModel {
    let mut m = HashMap::new();
    m.insert("apple".to_string(), FloatVector::from(vec![1.0, 0.0, 0.0]));
    m.insert("banana".to_string(), FloatVector::from(vec![0.0, 1.0, 0.0]));
    m.insert("cherry".to_string(), FloatVector::from(vec![0.0, 0.0, 1.0]));
    WordVectorsModel::new(3, m)
}

// ---------------------------------------------------------------------------
// WordVectorsModel basics
// ---------------------------------------------------------------------------

#[test]
fn dim_size_contains_and_lookup() {
    let model = axis_model();
    assert_eq!(model.dim(), 3);
    assert_eq!(model.size(), 3);
    assert!(model.contains("apple"));
    assert!(!model.contains("durian"));
    common::assert_vec_close(&model.word_vector("banana").unwrap(), &[0.0, 1.0, 0.0]);
}

#[test]
fn lookup_is_case_insensitive() {
    let model = axis_model();
    // Query case should not matter.
    assert!(model.contains("APPLE"));
    common::assert_vec_close(&model.word_vector("Apple").unwrap(), &[1.0, 0.0, 0.0]);
}

#[test]
fn mixed_case_vocab_is_reachable() {
    // A model constructed with a mixed-case key must still be reachable through
    // the case-insensitive lookups (keys are normalised to lowercase on build).
    let mut m = HashMap::new();
    m.insert("Hello".to_string(), FloatVector::from(vec![1.0, 2.0]));
    let model = WordVectorsModel::new(2, m);
    assert!(model.contains("hello"));
    assert!(model.contains("HELLO"));
    assert!(model.contains("Hello"));
    common::assert_vec_close(&model.word_vector("hello").unwrap(), &[1.0, 2.0]);
}

#[test]
fn word_not_found_errors() {
    let model = axis_model();
    assert!(matches!(
        model.word_vector("durian"),
        Err(VectorError::WordNotFound(_))
    ));
}

#[test]
fn default_word_vector_is_zeros() {
    let model = axis_model();
    common::assert_vec_close(&model.default_word_vector(), &[0.0, 0.0, 0.0]);
}

#[test]
fn similarity_is_dot_product() {
    let model = axis_model();
    // Orthogonal axis vectors → 0; self → 1.
    common::assert_close(model.similarity("apple", "banana").unwrap() as f64, 0.0);
    common::assert_close(model.similarity("apple", "apple").unwrap() as f64, 1.0);
}

#[test]
fn most_similar_ranks_and_excludes_query() {
    // Two near-identical vectors plus a distractor.
    let mut m = HashMap::new();
    m.insert("king".to_string(), FloatVector::from(vec![1.0, 0.0]));
    m.insert("monarch".to_string(), FloatVector::from(vec![0.99, 0.14]));
    m.insert("apple".to_string(), FloatVector::from(vec![0.0, 1.0]));
    let model = WordVectorsModel::new(2, m);

    let res = model.most_similar(&["king"], &[], 5).unwrap();
    // The query word itself is excluded from the results.
    assert!(res.0.iter().all(|p| p.key != "king"));
    // The closest remaining word ranks first.
    assert_eq!(res.0[0].key, "monarch");
}

#[test]
fn average_word_vector_of_known_words() {
    let model = axis_model();
    let avg = model.average_word_vector(&["apple", "banana"]).unwrap();
    common::assert_vec_close(&avg, &[0.5, 0.5, 0.0]);
}

#[test]
fn average_word_vector_no_valid_words_errors() {
    let model = axis_model();
    assert!(matches!(
        model.average_word_vector(&["durian", "mango"]),
        Err(VectorError::InvalidList(_))
    ));
}

// ---------------------------------------------------------------------------
// Binary loader
// ---------------------------------------------------------------------------

/// Serialise one entry: `word` + space + `dim` little-endian f32s + newline.
fn write_entry(buf: &mut Vec<u8>, word: &str, vec: &[f32]) {
    buf.extend_from_slice(word.as_bytes());
    buf.push(b' ');
    for &f in vec {
        buf.extend_from_slice(&f.to_le_bytes());
    }
    buf.push(b'\n');
}

#[test]
fn load_binary_word_vectors_roundtrip() {
    let dim = 3usize;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format!("2 {dim}\n").as_bytes());
    // Non-unit vectors so we can observe load-time normalisation.
    write_entry(&mut bytes, "alpha", &[3.0, 4.0, 0.0]); // norm 5
    write_entry(&mut bytes, "beta", &[0.0, 0.0, 2.0]); // norm 2

    let dir = std::env::temp_dir().join(format!("shared_w2v_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("vectors.bin");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&bytes).unwrap();
    }

    let model = load_binary_word_vectors(path.to_str().unwrap()).unwrap();
    assert_eq!(model.dim(), 3);
    assert_eq!(model.size(), 2);

    // Vectors are L2-normalised on load.
    let alpha = model.word_vector("alpha").unwrap();
    common::assert_vec_close(&alpha, &[0.6, 0.8, 0.0]);
    common::assert_close_eps(alpha.iter().map(|x| (x * x) as f64).sum::<f64>(), 1.0, 1e-5);
    common::assert_vec_close(&model.word_vector("beta").unwrap(), &[0.0, 0.0, 1.0]);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_binary_word_vectors_bad_header_errors() {
    let dir = std::env::temp_dir().join(format!("shared_w2v_bad_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bad.bin");
    std::fs::write(&path, b"not-a-valid-header\n").unwrap();

    let err = load_binary_word_vectors(path.to_str().unwrap()).unwrap_err();
    assert!(matches!(err, VectorError::BinaryError(_)), "got {err:?}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_binary_missing_file_is_io_error() {
    let err = load_binary_word_vectors("/nonexistent/path/to/model.bin").unwrap_err();
    assert!(matches!(err, VectorError::IoError(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Matrix aggregation helpers
// ---------------------------------------------------------------------------

#[test]
fn average_vector_over_rows() {
    let m = FloatMatrix::from_shape_vec((2, 3), vec![1.0, 2.0, 3.0, 3.0, 4.0, 5.0]).unwrap();
    common::assert_vec_close(&average_vector(&m).unwrap(), &[2.0, 3.0, 4.0]);
}

#[test]
fn average_vector_empty_errors() {
    let m = FloatMatrix::from_shape_vec((0, 0), vec![]).unwrap();
    assert!(matches!(average_vector(&m), Err(VectorError::EmptyVector)));
}

#[test]
fn average_vector_with_dimension_checks_width() {
    let m = FloatMatrix::from_shape_vec((2, 3), vec![1.0, 2.0, 3.0, 3.0, 4.0, 5.0]).unwrap();
    assert!(average_vector_with_dimension(&m, 3).is_ok());
    assert!(matches!(
        average_vector_with_dimension(&m, 4),
        Err(VectorError::DimensionMismatch { .. })
    ));
}

#[test]
fn weighted_vector_computes_weighted_mean() {
    // Rows [1,1], [3,3] with weights [1, 3] → (1*1 + 3*3)/4 = 2.5 per column.
    let m = FloatMatrix::from_shape_vec((2, 2), vec![1.0, 1.0, 3.0, 3.0]).unwrap();
    let w = FloatVector::from(vec![1.0, 3.0]);
    common::assert_vec_close(&weighted_vector(&m, &w).unwrap(), &[2.5, 2.5]);
}

#[test]
fn weighted_vector_length_mismatch_errors() {
    let m = FloatMatrix::from_shape_vec((2, 2), vec![1.0, 1.0, 3.0, 3.0]).unwrap();
    let w = FloatVector::from(vec![1.0, 2.0, 3.0]); // wrong length
    assert!(matches!(
        weighted_vector(&m, &w),
        Err(VectorError::DimensionMismatch { .. })
    ));
}

#[test]
fn weighted_vector_zero_weights_is_division_by_zero() {
    let m = FloatMatrix::from_shape_vec((2, 2), vec![1.0, 1.0, 3.0, 3.0]).unwrap();
    let w = FloatVector::from(vec![0.0, 0.0]);
    assert!(matches!(
        weighted_vector_with_dimension(&m, &w, 2),
        Err(VectorError::DivisionByZero)
    ));
}
