//! Git and filesystem source adapters (docs/11).
//!
//! The Git adapter uses `gix` directly, never the `git` CLI, for HEAD, tree
//! diffs, and blob reads (docs/11 section 5). Filesystem scanning lives in
//! `worktree` because dirty mode hashes bytes rather than trusting mtimes.

pub mod git;
pub mod worktree;

pub use git::GitSource;
pub use worktree::{FsSource, WorktreeScan};
