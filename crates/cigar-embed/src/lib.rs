//! Embedding adapters: deterministic fake for tests and ONNX for production.

pub mod download;
pub mod fake;
pub mod manifest;
pub mod onnx;
pub mod runtime;

pub use download::{download_model, verify_file, DownloadReport};
pub use fake::FakeEmbedder;
pub use manifest::{ModelManifest, ModelStatus};
pub use onnx::OnnxEmbedder;
pub use runtime::ensure_runtime;
