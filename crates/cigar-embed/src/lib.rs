// SPDX-License-Identifier: MPL-2.0

//! Embedding adapters: deterministic fake for tests and ONNX for production.

#[cfg(feature = "semantic")]
pub mod download;
pub mod fake;
pub mod manifest;
#[cfg(feature = "semantic")]
pub mod onnx;
pub mod runtime;

#[cfg(feature = "semantic")]
pub use download::{download_model, verify_file, DownloadReport};
pub use fake::FakeEmbedder;
pub use manifest::{ModelManifest, ModelStatus};
#[cfg(feature = "semantic")]
pub use onnx::OnnxEmbedder;
pub use runtime::ensure_runtime;
