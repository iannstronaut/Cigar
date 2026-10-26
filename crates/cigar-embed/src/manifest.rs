//! Model manifests and the on-disk model cache (docs/08 sections 3, 6, 7).
//!
//! A manifest is compiled in so the exact revision and checksum are auditable
//! and reproducible. Nothing here performs network I/O; `download` does.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use cigar_core::domain::{EmbeddingProvider, Pooling};
use cigar_core::error::{EngineError, Result};

/// Marker file written after a successful download + checksum verification.
pub const READY_MARKER: &str = ".ready";

/// One compiled-in model manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Stable identifier used in config and `meta`.
    pub id: String,
    /// Hugging Face repository.
    pub repo: String,
    /// Pinned revision (commit hash), never a branch.
    pub revision: String,
    /// ONNX file name inside the repository.
    pub onnx_file: String,
    /// SHA-256 of the ONNX file.
    pub sha256: String,
    /// Size in bytes.
    pub size_bytes: u64,
    /// Output dimension.
    pub dimensions: usize,
    /// Maximum tokens the model was trained for.
    pub max_tokens: usize,
    /// Pooling the model was trained with.
    pub pooling: Pooling,
    /// Tokenizer padding token.
    pub pad_token: String,
    /// SPDX license identifier.
    pub license: String,
    /// True when the tokenizer must be present.
    pub needs_tokenizer: bool,
    /// Provider that serves this model.
    pub provider: EmbeddingProvider,
}

impl ModelManifest {
    /// IBM Granite 125M English: 768-dim, CLS pooling, 512-token context.
    #[must_use]
    pub fn granite_125m() -> Self {
        Self {
            id: "granite-embedding-125m-english".into(),
            repo: "ibm-granite/granite-embedding-125m-english".into(),
            revision: "4ab61ffd423be45cd932b21a7c696063d82bf45f".into(),
            onnx_file: "model.onnx".into(),
            sha256: "d5a8fd811d55d90cbefb7b7434628629d69d06f9a8ad803fe941d75f44f66c62".into(),
            size_bytes: 498_821_859,
            dimensions: 768,
            max_tokens: 512,
            pooling: Pooling::Cls,
            pad_token: "<pad>".into(),
            license: "Apache-2.0".into(),
            needs_tokenizer: true,
            provider: EmbeddingProvider::OnnxLocal,
        }
    }

    /// IBM Granite 97M multilingual R2: 384-dim, CLS pooling, 32k context.
    #[must_use]
    pub fn granite_97m() -> Self {
        Self {
            id: "granite-embedding-97m-multilingual-r2".into(),
            repo: "ibm-granite/granite-embedding-97m-multilingual-r2".into(),
            revision: "835ad14087e140460703cf0fae09f97d469d65c2".into(),
            onnx_file: "model.onnx".into(),
            sha256: "68e592b160673d30250824c1116bc6ab33f70efb22b97c9e1d7ce1e69c1c9d70".into(),
            size_bytes: 390_004_608,
            dimensions: 384,
            max_tokens: 512,
            pooling: Pooling::Cls,
            pad_token: "<pad>".into(),
            license: "Apache-2.0".into(),
            needs_tokenizer: true,
            provider: EmbeddingProvider::OnnxLocal,
        }
    }

    /// Looks up a compiled-in manifest by the configured model name.
    #[must_use]
    pub fn lookup(model: &str) -> Option<Self> {
        for manifest in [Self::granite_125m(), Self::granite_97m()] {
            if manifest.id == model
                || manifest.id.starts_with(model)
                || model.starts_with(&manifest.id)
            {
                return Some(manifest);
            }
        }
        // Accept the short config default from docs (`granite-embedding-97m`).
        if model.contains("97m") {
            return Some(Self::granite_97m());
        }
        if model.contains("125m") {
            return Some(Self::granite_125m());
        }
        None
    }

    /// Every designed model, for `model status`.
    #[must_use]
    pub fn all() -> Vec<Self> {
        vec![Self::granite_125m(), Self::granite_97m()]
    }

    /// URL for the ONNX weights at the pinned revision.
    #[must_use]
    pub fn onnx_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.onnx_file
        )
    }

    /// URL for the tokenizer at the pinned revision.
    #[must_use]
    pub fn tokenizer_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/tokenizer.json",
            self.repo, self.revision
        )
    }

    /// Directory layout: `<model_dir>/<id>/<revision>`.
    #[must_use]
    pub fn directory(&self, model_dir: &Path) -> PathBuf {
        model_dir.join(&self.id).join(&self.revision)
    }

    /// True when the marker file and both artifacts exist.
    #[must_use]
    pub fn is_ready(&self, model_dir: &Path) -> bool {
        let dir = self.directory(model_dir);
        dir.join(READY_MARKER).is_file()
            && dir.join(&self.onnx_file).is_file()
            && (!self.needs_tokenizer || dir.join("tokenizer.json").is_file())
    }
}

/// Status of one model on disk.
#[derive(Clone, Debug)]
pub struct ModelStatus {
    pub manifest: ModelManifest,
    pub directory: PathBuf,
    pub ready: bool,
    pub onnx_bytes: Option<u64>,
    pub tokenizer_bytes: Option<u64>,
}

impl ModelStatus {
    /// Inspects a model directory without loading anything.
    #[must_use]
    pub fn inspect(manifest: &ModelManifest, model_dir: &Path) -> Self {
        let directory = manifest.directory(model_dir);
        let onnx = std::fs::metadata(directory.join(&manifest.onnx_file))
            .ok()
            .map(|meta| meta.len());
        let tokenizer = std::fs::metadata(directory.join("tokenizer.json"))
            .ok()
            .map(|meta| meta.len());
        Self {
            ready: manifest.is_ready(model_dir),
            directory,
            onnx_bytes: onnx,
            tokenizer_bytes: tokenizer,
            manifest: manifest.clone(),
        }
    }
}

/// Model directory for a manifest, creating it if needed.
pub fn ensure_model_dir(manifest: &ModelManifest, model_dir: &Path) -> Result<PathBuf> {
    let dir = manifest.directory(model_dir);
    std::fs::create_dir_all(&dir).map_err(|err| EngineError::io(&dir, err))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_matches_short_names() {
        assert_eq!(
            ModelManifest::lookup("granite-embedding-97m")
                .unwrap()
                .dimensions,
            384
        );
        assert_eq!(
            ModelManifest::lookup("granite-embedding-125m-english")
                .unwrap()
                .dimensions,
            768
        );
        assert!(ModelManifest::lookup("unknown-model").is_none());
    }

    #[test]
    fn urls_are_pinned_to_revision() {
        let manifest = ModelManifest::granite_125m();
        assert!(manifest.onnx_url().contains(&manifest.revision));
        assert!(manifest.tokenizer_url().ends_with("tokenizer.json"));
    }

    #[test]
    fn ready_requires_marker_and_artifacts() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = ModelManifest::granite_125m();
        assert!(!manifest.is_ready(dir.path()));
        let model_dir = ensure_model_dir(&manifest, dir.path()).unwrap();
        std::fs::write(model_dir.join(&manifest.onnx_file), b"x").unwrap();
        std::fs::write(model_dir.join("tokenizer.json"), b"{}").unwrap();
        assert!(!manifest.is_ready(dir.path()));
        std::fs::write(model_dir.join(READY_MARKER), b"").unwrap();
        assert!(manifest.is_ready(dir.path()));
    }

    #[test]
    fn status_reports_sizes() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = ModelManifest::granite_125m();
        let model_dir = ensure_model_dir(&manifest, dir.path()).unwrap();
        std::fs::write(model_dir.join(&manifest.onnx_file), b"abc").unwrap();
        let status = ModelStatus::inspect(&manifest, dir.path());
        assert_eq!(status.onnx_bytes, Some(3));
        assert!(!status.ready);
    }
}
