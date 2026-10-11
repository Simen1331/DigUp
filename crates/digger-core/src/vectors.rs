//! Vectors are stored as 768 × float16 (1.5 KB each), kept that way in memory too, and scored as float32.

use half::f16;

pub const DIMENSION: usize = 768;

/// A vector as the index stores it: float16, little-endian.
pub fn half_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| f16::from_f32(*value).to_le_bytes())
        .collect()
}

/// The vectors search scores, as stored (float16), with room to turn `BLOCK_ROWS` of them into float32 at a time:
/// half the memory of keeping float32 for ~2 ms more a query.
pub struct VectorStorage {
    pub rows: usize,
    halves: Vec<f16>,
    block: Vec<f32>,
}

impl VectorStorage {
    pub const BLOCK_ROWS: usize = 1024;

    pub fn new(rows: usize) -> VectorStorage {
        VectorStorage {
            rows,
            halves: Vec::with_capacity(rows * DIMENSION),
            block: vec![0.0; Self::BLOCK_ROWS * DIMENSION],
        }
    }

    /// Appends one stored vector (`DIMENSION` float16s). False if it's the wrong size.
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() != DIMENSION * 2 {
            return false;
        }
        self.halves
            .extend(bytes.as_chunks::<2>().0.iter().map(|pair| f16::from_le_bytes(*pair)));
        true
    }

    pub fn len(&self) -> usize {
        self.halves.len() / DIMENSION
    }

    pub fn is_empty(&self) -> bool {
        self.halves.is_empty()
    }

    /// Cosine similarity of `query` with every stored vector (all are L2-normalized): a matrix-vector product, done a
    /// block of rows at a time, each block turned into float32 first.
    pub fn scores(&mut self, query: &[f32]) -> Vec<f32> {
        let rows = self.len();
        let mut scores = vec![0.0f32; rows];
        let mut row = 0;
        while row < rows {
            let count = Self::BLOCK_ROWS.min(rows - row);
            let block = &mut self.block[..count * DIMENSION];
            half::slice::HalfFloatSliceExt::convert_to_f32_slice(
                &self.halves[row * DIMENSION..(row + count) * DIMENSION],
                block,
            );
            for (offset, vector) in block.as_chunks::<DIMENSION>().0.iter().enumerate() {
                scores[row + offset] = dot(vector, query);
            }
            row += count;
        }
        scores
    }
}

/// A dot product the compiler vectorizes: eight running sums.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut sums = [0.0f32; 8];
    let ((a_chunks, a_tail), (b_chunks, b_tail)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    let tail: f32 = a_tail.iter().zip(b_tail).map(|(x, y)| x * y).sum();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for lane in 0..8 {
            sums[lane] += x[lane] * y[lane];
        }
    }
    sums.iter().sum::<f32>() + tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_vectors_convert_back_exactly() {
        let vector: Vec<f32> = (0..DIMENSION)
            .map(|i| ((i as f64 * 0.37).sin() * 0.05) as f32)
            .collect();
        let mut storage = VectorStorage::new(2);
        assert!(storage.push(&half_bytes(&vec![0.0; DIMENSION])));
        assert!(storage.push(&half_bytes(&vector)));
        let rounded: Vec<f32> = vector.iter().map(|value| f16::from_f32(*value).to_f32()).collect();
        let scores = storage.scores(&rounded);
        let expected: f32 = rounded.iter().map(|value| value * value).sum();
        assert_eq!(scores[0], 0.0);
        assert!((scores[1] - expected).abs() < 1e-6);
    }
}
