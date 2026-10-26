//! tree-sitter parser adapters for Rust, TypeScript/TSX, and JavaScript.
//!
//! Each parser extracts imports, symbols, and edges exactly as docs/07
//! specifies. Parsers are pure: input is a `&str`, output is `ParsedFile`, and
//! they never touch the filesystem, hashing, or embeddings.

mod common;
mod javascript;
mod rust;
mod typescript;

pub use common::{ParserRegistry, ParserRegistryBuilder};
pub use javascript::JavaScriptParser;
pub use rust::RustParser;
pub use typescript::TypeScriptParser;

/// Extensions handled by each parser, kept next to the parser that owns them.
pub const RUST_EXTENSIONS: &[&str] = &["rs"];
pub const TYPESCRIPT_EXTENSIONS: &[&str] = &["ts", "mts", "cts"];
pub const TSX_EXTENSIONS: &[&str] = &["tsx"];
pub const JAVASCRIPT_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "jsx"];
