//! CIGAR engine core.
//!
//! This crate owns the domain model, the ports (traits) that adapters implement,
//! and the pure algorithms (hashing, template rendering, delta planning). It must
//! never depend on SQLite, ONNX, or the CLI.
pub mod config;
pub mod domain;
pub mod error;
pub mod git;
pub mod hash;
pub mod parse;
pub mod sync;
pub mod traits;

pub use error::{EngineError, Result};

/// Schema version produced by this engine.
pub const SCHEMA_VERSION: u32 = 2;

/// Content version: bump when symbol extraction or body normalization changes
/// meaning (see docs/21 ?2.2). Included in `symbol_hash`.
pub const CONTENT_VERSION: u32 = 1;

/// Embedding template version: bump when the template text format changes.
/// Included in `embedding_key`.
pub const TEMPLATE_VERSION: u32 = 1;

/// Engine version reported by `cigar version` and stored in `meta`.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
