//! Load and query binary word2vec models.

use super::error::VectorError;
use super::types::{FloatMatrix, FloatVector, FloatX, Pair, PairList};
use super::vector_math::VectorMathExt;
use byteorder::{LittleEndian, ReadBytesExt};
use ndarray::Axis;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The default dimension for word vectors if not specified.
static W2V_DEFAULT_DIMENSION: AtomicUsize = AtomicUsize::new(300);
/// A global, lazily-initialized, thread-safe WordVectorsModel instance.
static W2V_MODEL: Lazy<RwLock<Option<WordVectorsModel>>> = Lazy::new(|| RwLock::new(None));

#[derive(Debug, Clone, Default)]
pub struct WordVectorsModel {
    /// The dimensionality of the vectors in the model.
    dim: usize,
    /// A map from words to their corresponding `FloatVector`.
    words: HashMap<String, FloatVector>,
}

pub fn set_w2v_default_dimension(dim: usize) {
    W2V_DEFAULT_DIMENSION.store(dim, Ordering::SeqCst);
}

impl WordVectorsModel {
    pub fn new(dim: usize, words: HashMap<String, FloatVector>) -> Self {
        // All lookups (`word_vector`, `contains`, `similarity`, and the
        // `most_similar` query filter) key on the *lowercased* query, so the
        // stored keys must also be lowercased — otherwise any mixed-case vocab
        // entry (e.g. "Hello" in a model file) would be permanently unreachable.
        // Normalising here fixes both construction paths at once, since
        // `load_binary_word_vectors` also builds the model through `new`.
        // (Keys differing only in case collapse to one entry, which is the
        // intended behaviour for a case-insensitive model.)
        let words = words
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v))
            .collect();
        Self { dim, words }
    }
    pub fn word_vector(&self, word: &str) -> Result<FloatVector, VectorError> {
        self.words
            .get(&word.to_lowercase())
            .cloned()
            .ok_or_else(|| VectorError::WordNotFound(word.to_string()))
    }
    pub fn default_word_vector(&self) -> FloatVector {
        FloatVector::zeros(self.dim)
    }
    pub fn contains(&self, word: &str) -> bool {
        self.words.contains_key(&word.to_lowercase())
    }
    pub fn dim(&self) -> usize {
        self.dim
    }
    pub fn size(&self) -> usize {
        self.words.len()
    }
    pub fn similarity(&self, x: &str, y: &str) -> Result<f32, VectorError> {
        let vec1 = self.word_vector(x)?;
        let vec2 = self.word_vector(y)?;
        Ok(vec1.dot(&vec2))
    }
    pub fn most_similar(
        &self,
        positives: &[&str],
        negatives: &[&str],
        n: usize,
    ) -> Result<PairList, VectorError> {
        let mut target_vec = FloatVector::zeros(self.dim);
        for word in positives {
            target_vec += &self.word_vector(word)?;
        }
        for word in negatives {
            target_vec -= &self.word_vector(word)?;
        }
        target_vec.normalize_in_place().ok(); // Ignore error if norm is 0
        let mut similarities: Vec<Pair> = self
            .words
            .iter()
            .map(|(word, vec)| Pair {
                key: word.clone(),
                value: (target_vec.dot(vec)) as f64,
            })
            .collect();
        similarities.sort_by(|a, b| {
            b.value
                .partial_cmp(&a.value)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Exclude the query words themselves
        let query_words: HashSet<String> = positives
            .iter()
            .chain(negatives.iter())
            .map(|s| s.to_lowercase())
            .collect();
        let filtered: Vec<_> = similarities
            .into_iter()
            .filter(|p| !query_words.contains(&p.key.to_lowercase()))
            .take(n)
            .collect();
        Ok(PairList(filtered))
    }
    pub fn average_word_vector(&self, words: &[&str]) -> Result<FloatVector, VectorError> {
        let vecs: Vec<_> = words
            .iter()
            .filter_map(|w| self.word_vector(w).ok())
            .collect();
        if vecs.is_empty() {
            return Err(VectorError::InvalidList(
                "Cannot compute average; no valid words provided.".into(),
            ));
        }
        // Build a matrix from the collected vectors
        let matrix = FloatMatrix::from_shape_vec(
            (vecs.len(), self.dim),
            vecs.into_iter().flatten().collect(),
        )
        .map_err(|e| VectorError::ConversionError(e.to_string()))?;
        // The mean along axis 0 gives the average vector
        Ok(matrix.mean_axis(Axis(0)).unwrap())
    }
}
pub fn load_word_vectors(file_path: &str) -> Result<(), VectorError> {
    let model = load_binary_word_vectors(file_path)?;
    *W2V_MODEL.write() = Some(model);
    Ok(())
}
pub fn load_binary_word_vectors(filename: &str) -> Result<WordVectorsModel, VectorError> {
    let file = File::open(filename)?;
    let mut reader = BufReader::new(file);
    let mut header = String::new();
    reader.read_line(&mut header)?;
    let parts: Vec<&str> = header.split_whitespace().collect();
    if parts.len() != 2 {
        return Err(VectorError::BinaryError(
            "Invalid header format in w2v model file.".into(),
        ));
    }
    let size: usize = parts[0]
        .parse()
        .map_err(|_| VectorError::BinaryError("Invalid vocabulary size".into()))?;
    let dim: usize = parts[1]
        .parse()
        .map_err(|_| VectorError::BinaryError("Invalid dimension".into()))?;
    let mut words = HashMap::with_capacity(size);
    let mut vec_buffer = vec![0.0 as FloatX; dim];
    for _ in 0..size {
        let mut word_bytes = Vec::new();
        reader.read_until(b' ', &mut word_bytes)?;
        if word_bytes.is_empty() {
            continue;
        }
        // The last byte is the space delimiter, trim it and any other whitespace
        let word = String::from_utf8_lossy(&word_bytes[..word_bytes.len() - 1])
            .trim()
            .to_string();
        reader.read_f32_into::<LittleEndian>(&mut vec_buffer)?;
        let mut vector = FloatVector::from(vec_buffer.clone());
        vector.normalize_in_place().ok(); // Normalize vector, ignore if norm is 0
        words.insert(word, vector);
        // Some formats have a newline/space after the vector, which we need to consume.
        if let Ok(b) = reader.read_u8() {
            if b != b'\n' && !b.is_ascii_whitespace() {
                // It's part of the next word. Go back one byte.
                reader.seek(SeekFrom::Current(-1))?;
            }
        }
    }
    Ok(WordVectorsModel::new(dim, words))
}
pub fn average_vector(vectors: &FloatMatrix) -> Result<FloatVector, VectorError> {
    if vectors.is_empty() {
        return Err(VectorError::EmptyVector);
    }
    // `mean_axis` will return `None` if the axis is empty, but we've checked `is_empty`.
    Ok(vectors.mean_axis(Axis(0)).unwrap())
}
pub fn average_vector_with_dimension(
    vectors: &FloatMatrix,
    dimension: usize,
) -> Result<FloatVector, VectorError> {
    if vectors.is_empty() {
        return Err(VectorError::EmptyVector);
    }
    if vectors.shape()[1] != dimension {
        return Err(VectorError::DimensionMismatch {
            expected: vec![vectors.shape()[0], dimension],
            actual: vectors.shape().to_vec(),
        });
    }
    Ok(vectors.mean_axis(Axis(0)).unwrap())
}
pub fn weighted_vector(
    vectors: &FloatMatrix,
    weights: &FloatVector,
) -> Result<FloatVector, VectorError> {
    if vectors.is_empty() {
        return Err(VectorError::EmptyVector);
    }
    weighted_vector_with_dimension(vectors, weights, vectors.shape()[1])
}
pub fn weighted_vector_with_dimension(
    vectors: &FloatMatrix,
    weights: &FloatVector,
    dimension: usize,
) -> Result<FloatVector, VectorError> {
    if vectors.shape()[0] != weights.len() {
        return Err(VectorError::DimensionMismatch {
            expected: vec![weights.len()],
            actual: vec![vectors.shape()[0]],
        });
    }
    if vectors.shape()[1] != dimension {
        return Err(VectorError::DimensionMismatch {
            expected: vec![vectors.shape()[0], dimension],
            actual: vectors.shape().to_vec(),
        });
    }
    let weights_sum = weights.sum();
    if weights_sum == 0.0 {
        return Err(VectorError::DivisionByZero);
    }
    // computes (weights^T * vectors), resulting in a weighted sum for each column.
    let weighted_sum_vector = weights.dot(vectors);
    // Divide by the sum of weights to get the weighted mean.
    Ok(weighted_sum_vector / weights_sum)
}
