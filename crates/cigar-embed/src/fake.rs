// SPDX-License-Identifier: MPL-2.0

//! Deterministic fake embedder for tests and `--offline`.
//!
//! Produces stable, unit-norm vectors derived from the BLAKE3 digest of the
//! input text. It is not semantically meaningful, so it must never be the
//! default in production config; it exists so the whole pipeline can be tested
//! without downloading a model.

use std::fmt;

use blake3::Hasher;
use cigar_core::domain::EmbeddingKey;
use cigar_core::error::{EngineError, Result};
use cigar_core::traits::Embedder;

/// Deterministic embedder.
#[derive(Clone, Debug)]
pub struct FakeEmbedder {
    dimensions: usize,
    max_tokens: usize,
    model_id: String,
    revision: String,
}

impl FakeEmbedder {
    /// Creates a fake embedder with the given dimension.
    pub fn new(dimensions: usize) -> Result<Self> {
        if dimensions == 0 || dimensions > cigar_core::config::MAX_DIMENSIONS {
            return Err(EngineError::invalid(format!(
                "dimensi fake embedder tidak valid: {dimensions}"
            )));
        }
        Ok(Self {
            dimensions,
            max_tokens: 512,
            model_id: "fake".into(),
            revision: "fake-v1".into(),
        })
    }

    /// Overrides the token budget used by template rendering.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn vector_for(&self, text: &str) -> Vec<f32> {
        let mut seed = Hasher::new();
        seed.update(b"fake-embed\0");
        seed.update(text.as_bytes());
        let digest = seed.finalize();
        let bytes = digest.as_bytes();
        let mut state = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
        let mut vector = Vec::with_capacity(self.dimensions);
        for _ in 0..self.dimensions {
            // SplitMix64: stable across platforms, no external RNG.
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let unit = (z >> 11) as f64 / (1_u64 << 53) as f64;
            vector.push((unit * 2.0 - 1.0) as f32);
        }
        normalize(&mut vector);
        vector
    }
}

fn normalize(vector: &mut [f32]) {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > f64::EPSILON {
        for value in vector.iter_mut() {
            *value = (*value as f64 / norm) as f32;
        }
    }
}

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn revision(&self) -> &str {
        &self.revision
    }

    fn count_tokens(&self, text: &str) -> usize {
        // The fake embedder has no tokenizer; the contract default is fine, and
        // keeping it explicit makes template tests stable.
        let chars = text.chars().count();
        chars.div_ceil(4).saturating_mul(11) / 10
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let vector = self.vector_for(text);
            if vector.len() != self.dimensions {
                return Err(EngineError::DimensionMismatch {
                    expected: self.dimensions,
                    actual: vector.len(),
                });
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(EngineError::Embedding {
                    message: "fake embedder menghasilkan nilai non-finite".into(),
                });
            }
            out.push(vector);
        }
        Ok(out)
    }
}

impl fmt::Display for FakeEmbedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fake({}d)", self.dimensions)
    }
}

/// Convenience: render a template text and return both the text and its key.
#[must_use]
pub fn template_key(text: &str, template_version: u32) -> EmbeddingKey {
    cigar_core::hash::embedding_key(template_version, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_across_calls() {
        let embedder = FakeEmbedder::new(16).unwrap();
        let a = embedder.embed_batch(&["hello".into()]).unwrap();
        let b = embedder.embed_batch(&["hello".into()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_text_differs() {
        let embedder = FakeEmbedder::new(16).unwrap();
        let a = embedder.embed_batch(&["hello".into()]).unwrap();
        let b = embedder.embed_batch(&["world".into()]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn vectors_are_unit_norm_and_finite() {
        let embedder = FakeEmbedder::new(768).unwrap();
        let vector = embedder.embed_query("query").unwrap();
        assert_eq!(vector.len(), 768);
        assert!(vector.iter().all(|value| value.is_finite()));
        let norm = vector
            .iter()
            .map(|value| f64::from(*value) * f64::from(*value))
            .sum::<f64>()
            .sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "{norm}");
    }

    #[test]
    fn empty_batch_is_empty() {
        let embedder = FakeEmbedder::new(8).unwrap();
        assert!(embedder.embed_batch(&[]).unwrap().is_empty());
    }

    #[test]
    fn invalid_dimensions_rejected() {
        assert!(FakeEmbedder::new(0).is_err());
    }
}
