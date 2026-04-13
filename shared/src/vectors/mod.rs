//! Vectors, matrices, tensors and a word2vec loader.

pub mod error;
pub mod matrix_math;
pub mod ndarray_ops;
pub mod tensor;
pub mod types;
pub mod utils;
pub mod vector_math;
pub mod wordvectors;

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
