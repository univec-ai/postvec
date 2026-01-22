// File: shared/src/vectors/mod.rs
//! # vecs
//!
//! A comprehensive Rust library for numerical computing, specializing in vector and matrix
//! operations. This module provides a rich set of functionalities including, but not limited to:
//!
//! - **N-Dimensional Arrays:** Type aliases and robust operations for vectors, matrices, and tensors using `ndarray`.
//!
//! - **Mathematical Operations:** A rich `VectorMathExt` trait for statistical analysis, similarity metrics, normalization, and more.
//!
//! - **Generic Tensors:** A flexible `GenericTensor` structure for working with dynamically typed and shaped data, essential for machine learning model inputs/outputs.
//!
//! - **Word Embeddings:** A `WordVectorsModel` for loading and interacting with pre-trained word2vec models.
//!
//! The library is designed to be performant, type-safe, and idiomatic, leveraging Rust's powerful features to provide a reliable foundation for numerical applications.
// Declare all the sub-modules that make up the `vectors` functionality.
// Each module is defined in a corresponding file within this directory
// (e.g., `error.rs`, `matrix_math.rs`).
pub mod error;
pub mod matrix_math;
pub mod ndarray_ops;
pub mod tensor;
pub mod types;
pub mod utils;
pub mod vector_math;
pub mod wordvectors;
// Re-export key types, traits, and functions for convenient access from outside
// this module (and from the `shared` crate root, which also re-exports them).
pub use error::VectorError;
pub use matrix_math::MatrixMathExt;
pub use ndarray_ops::{
    shape_match, AsFloatCube, AsFloatMatrix, AsFloatPenta, AsFloatQuad, AsFloatVector, NdArrayExt,
    ReshapeExt,
};
pub use tensor::{
    new_2d_tensor_from_float_vecs, new_2d_tensor_from_i64_vecs, new_3d_tensor_from_float_vecs,
    new_dictionary_tensor, new_string_tensor, new_tensor_with_shape,
    new_tensor_with_shape_inference, GenericTensor, IntoGenericTensor, TensorDataType, TensorValue,
};
pub use types::{
    Float32Cube, Float32Matrix, Float32Penta, Float32Quad, Float32Vector, Float64Matrix,
    Float64Vector, FloatCube, FloatMatrix, FloatPenta, FloatQuad, FloatVector, FloatX,
    IntoGenericTensorExt, Pair, PairList, INF, NA, NEG_INF,
};
pub use vector_math::{
    deserialize_float_vector, gaussian_vector, gaussian_vector_normalized, new_float_vector,
    random_vector, serialize_float_vector, uniform_vector, VectorMathExt,
};
pub use wordvectors::{
    average_vector, average_vector_with_dimension, load_binary_word_vectors, load_word_vectors,
    set_w2v_default_dimension, weighted_vector, weighted_vector_with_dimension, WordVectorsModel,
};
