//! Embedding backend selection and model preparation.

use std::path::Path;

use cigar_core::config::Config;
use cigar_core::domain::EmbeddingProvider;
use cigar_core::error::{EngineError, Result};
use cigar_core::traits::Embedder;
use cigar_embed::manifest::ModelManifest;
use cigar_embed::{ensure_runtime, FakeEmbedder, OnnxEmbedder};

/// Builds the embedder named by config.
///
/// `offline` forbids network access but does not by itself forbid loading a
/// model that is already on disk: offline `sync` must work.
pub fn build_embedder(config: &Config, offline: bool) -> Result<Box<dyn Embedder>> {
    let _ = offline;
    match config.embedding.provider {
        EmbeddingProvider::Fake => Ok(Box::new(
            FakeEmbedder::new(config.embedding.dimensions)?
                .with_max_tokens(config.embedding.max_tokens),
        )),
        EmbeddingProvider::OnnxLocal => {
            let manifest = resolve_manifest(config)?;
            let model_dir = manifest.directory(&config.embedding.model_dir);
            if !manifest.is_ready(&config.embedding.model_dir) {
                return Err(EngineError::ModelUnavailable {
                    message: format!(
                        "model {} belum diunduh di {}; jalankan `cigar model download`",
                        manifest.id,
                        model_dir.display()
                    ),
                });
            }
            let runtime_dir = config
                .embedding
                .model_dir
                .parent()
                .map_or_else(|| config.embedding.model_dir.clone(), Path::to_path_buf);
            ensure_runtime(&model_dir, &runtime_dir)?;
            let embedder = OnnxEmbedder::load(
                &model_dir,
                &manifest,
                config.embedding.threads,
                &config.embedding.query_prefix,
                &config.embedding.document_prefix,
            )?;
            Ok(Box::new(embedder))
        }
    }
}

/// Resolves the manifest for the configured model, failing clearly when the
/// configured model is not a known compiled-in manifest.
pub fn resolve_manifest(config: &Config) -> Result<ModelManifest> {
    ModelManifest::lookup(&config.embedding.model).ok_or_else(|| EngineError::ModelUnavailable {
        message: format!(
            "model `{}` tidak dikenal; gunakan salah satu: {}",
            config.embedding.model,
            ModelManifest::all()
                .iter()
                .map(|manifest| manifest.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

/// Validates that config dimensions match the model manifest before any work.
pub fn validate_dimensions(config: &Config, manifest: &ModelManifest) -> Result<()> {
    if config.embedding.dimensions != manifest.dimensions {
        return Err(EngineError::DimensionMismatch {
            expected: manifest.dimensions,
            actual: config.embedding.dimensions,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embedder_is_always_available() {
        let mut config = Config::default();
        config.embedding.provider = EmbeddingProvider::Fake;
        let embedder = build_embedder(&config, true).unwrap();
        assert_eq!(embedder.model_id(), "fake");
    }

    #[test]
    fn missing_model_is_actionable() {
        let config = Config::default();
        let error = match build_embedder(&config, false) {
            Ok(embedder) => panic!("harus gagal, dapat {}", embedder.model_id()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("model download"), "{error}");
    }

    #[test]
    fn unknown_model_lists_choices() {
        let mut config = Config::default();
        config.embedding.model = "nope".into();
        let error = resolve_manifest(&config).unwrap_err();
        assert!(error.to_string().contains("granite"));
    }

    #[test]
    fn dimension_mismatch_is_caught() {
        let mut config = Config::default();
        config.embedding.dimensions = 384;
        let manifest = resolve_manifest(&config).unwrap();
        assert!(validate_dimensions(&config, &manifest).is_err());
    }
}
