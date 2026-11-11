// SPDX-License-Identifier: MPL-2.0

//! SQLite FTS5 storage with atomic namespace sync.

mod lock;
mod target;

pub use lock::StoreLock;
pub use target::{
    TargetBatch, TargetEdge, TargetFile, TargetHit, TargetNeighbor, TargetStats, TargetStore,
    TargetSymbol,
};
