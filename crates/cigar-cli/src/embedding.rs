//! Embedding backend selection.

use cigar_core::config::Config;
use cigar_core::error::{EngineError, Result};
use cigar_core::traits::Embedder;
use cigar_embed::FakeEmbedder;

/// Builds the embedder named by config.
///
/// Fase 2 ships the fake backend only; the ONNX backend arrives in Fase 3 and is
/// selected by the same config key, so callers never branch on it.
pub fn build_embedder(config: &Config, offline: bool) -> Result<Box<dyn Embedder>> {
    match config.embedding.provider {
        cigar_core::domain::EmbeddingProvider::Fake => Ok(Box::new(
            FakeEmbedder::new(config.embedding.dimensions)?
                .with_max_tokens(config.embedding.max_tokens),
        )),
        cigar_core::domain::EmbeddingProvider::OnnxLocal => Err(EngineError::ModelUnavailable {
            message: if offline {
                "provider onnx-local belum tersedia pada build ini; gunakan --offline dengan provider fake"
                    .into()
            } else {
                "provider onnx-local belum tersedia pada build ini; set embedding.provider=\"fake\" sementara"
                    .into()
            },
        }),
    }
}
