//! SQLite + sqlite-vec storage adapter.
//!
//! Responsibilities:
//! - register sqlite-vec once per process,
//! - own the single writer connection,
//! - create/maintain schema v2 (docs/21 ?3),
//! - apply content and branch batches atomically,
//! - provide KNN and graph reads filtered to one branch.

mod batch;
mod context_store;
mod lock;
mod migrate;
mod schema;
mod sqlite;
mod store;

pub use context_store::vector_blob;
pub use lock::StoreLock;
pub use migrate::{backup_store, is_schema_v1, require_reindex_for_v1};
pub use schema::{SCHEMA_SQL_V2, SUPPORTED_SCHEMA_VERSION};
pub use sqlite::{register_sqlite_vec, SqliteStore};
pub use store::{
    open_or_init_store, open_store, open_store_for_write, record_model_meta,
    stored_config_fingerprint, verify_model_compat,
};

pub use cigar_core::domain::{
    BranchBatch, BranchEdgeRef, BranchFile, BranchName, BranchOutcome, BranchState, ContentBatch,
    ContentOutcome, GcOptions, GcReport, Namespace, ScoredOccurrence, StoreHealth, StoreStats,
    SymbolOccurrence,
};
