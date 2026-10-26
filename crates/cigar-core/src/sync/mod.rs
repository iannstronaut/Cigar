//! Incremental sync pipeline (docs/04).
//!
//! The pipeline is deliberately staged so each step can be tested alone:
//!
//! ```text
//! delta -> read+hash -> parse -> plan -> embed -> write content -> write branch
//!       -> resolve edges -> publish refs
//! ```
//!
//! `sync` is idempotent: running it twice on an unchanged tree performs no
//! writes and no embedding.

mod delta;
mod pipeline;
pub mod plan;

pub use delta::{compute_delta, DeltaPlan, FileTask};
pub use pipeline::{
    branch_fingerprint, run_sync, supports_dirty, SyncEngine, SyncOptions, SyncOutcome,
};
pub use plan::{
    build_manifest, pending_vectors, plan_file, plan_source, PlannedFile, PlannedSymbol,
};
