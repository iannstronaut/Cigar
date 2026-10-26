//! Domain records.
//!
//! These types are shared between adapters and are deliberately free of storage
//! concerns: no rowids, no connection handles, no SQL.

use crate::domain::{
    BlobHash, BranchName, CommitId, ConfigFingerprint, Digest, EmbeddingKey, Language, Namespace,
    OccurrenceRef, RepoPath, SymbolHash, SymbolKind, SyncMode, SyncStatus,
};
use crate::hash::EmbeddingTemplate;

/// State row for one `(namespace, branch)` pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchState {
    pub namespace: Namespace,
    pub branch: BranchName,
    pub commit_hash: Option<CommitId>,
    pub last_sync_mode: SyncMode,
    pub status: SyncStatus,
    pub worktree_fingerprint: Option<Digest>,
    pub config_fingerprint: ConfigFingerprint,
    pub updated_at_ms: i64,
}

/// One file in a branch manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchFile {
    pub file_path: RepoPath,
    pub blob_hash: BlobHash,
    pub language: Language,
}

/// Fully resolved symbol definition (content level, shared across branches).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolDef {
    pub symbol_hash: SymbolHash,
    pub language: Language,
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub signature: String,
    pub docstring: Option<String>,
    pub code_snippet: String,
    pub body_hash: Digest,
}

/// A symbol occurrence inside one branch file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolOccurrence {
    pub symbol: SymbolDef,
    pub branch: BranchName,
    pub file_path: RepoPath,
    pub start_line: u32,
    pub end_line: u32,
    pub embedding_key: EmbeddingKey,
}

/// Content-level mapping of a symbol inside a blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobSymbol {
    pub blob_hash: BlobHash,
    pub language: Language,
    pub symbol_hash: SymbolHash,
    pub start_line: u32,
    pub end_line: u32,
}

/// One row of `branch_symbols`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchSymbol {
    pub file_path: RepoPath,
    pub blob_hash: BlobHash,
    pub language: Language,
    pub symbol_hash: SymbolHash,
    pub start_line: u32,
    pub end_line: u32,
    pub embedding_key: EmbeddingKey,
}

/// Raw (content-level) edge emitted by a parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolEdge {
    pub source_symbol_hash: SymbolHash,
    pub target_name: String,
    pub relation: crate::domain::EdgeRelation,
}

/// A branch edge row with an unresolved target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchEdgeRef {
    pub branch: BranchName,
    pub file_path: RepoPath,
    pub source_symbol_hash: SymbolHash,
    pub target_name: String,
    pub relation: crate::domain::EdgeRelation,
}

/// A vector search hit, already filtered to a branch manifest.
#[derive(Clone, Debug)]
pub struct ScoredOccurrence {
    pub embedding_key: EmbeddingKey,
    pub symbol_hash: SymbolHash,
    pub file_path: RepoPath,
    pub start_line: u32,
    pub end_line: u32,
    pub distance: f32,
    pub score: f32,
    /// Code snippet cached by the query engine for rendering; never persisted.
    pub pending_code: String,
}

/// Result of a vector search before branch filtering, as stored in the pool.
#[derive(Clone, Debug)]
pub struct PoolHit {
    pub embedding_key: EmbeddingKey,
    pub distance: f32,
}

/// Symbol plus relation for graph expansion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedOccurrence {
    pub occurrence: OccurrenceRef,
    pub relation: crate::domain::EdgeRelation,
    pub depth: u8,
}

/// Neighborhood around the root occurrence.
#[derive(Clone, Debug, Default)]
pub struct GraphNeighborhood {
    pub root: Option<SymbolOccurrence>,
    pub callers: Vec<RelatedOccurrence>,
    pub callees: Vec<RelatedOccurrence>,
    pub types_used: Vec<RelatedOccurrence>,
    pub implements: Vec<RelatedOccurrence>,
    pub implemented_by: Vec<RelatedOccurrence>,
    pub truncated: bool,
}

/// Counts for `status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub vectors: u64,
    pub unresolved_edges: u64,
    pub db_size_bytes: u64,
    pub last_sync: Option<BranchState>,
}

/// Integrity report from `status --verify`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreHealth {
    pub ok: bool,
    pub foreign_key_violations: u64,
    pub quick_check_ok: bool,
    pub orphan_vectors: u64,
    pub symbols_without_vectors: u64,
    pub dangling_edges: u64,
    pub manifest_files_without_symbols: u64,
    pub messages: Vec<String>,
}

impl StoreHealth {
    /// Recomputes `ok` from the individual checks.
    #[must_use]
    pub fn finish(mut self) -> Self {
        self.ok = self.foreign_key_violations == 0
            && self.quick_check_ok
            && self.orphan_vectors == 0
            && self.symbols_without_vectors == 0
            && self.dangling_edges == 0
            && self.manifest_files_without_symbols == 0;
        self
    }
}

/// Options for garbage collection (docs/21 ?6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcOptions {
    pub dry_run: bool,
    pub prune_branches: bool,
    pub vacuum: bool,
}

/// Result of garbage collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub branches_pruned: u64,
    pub vectors_deleted: u64,
    pub symbol_defs_deleted: u64,
    pub symbol_edges_deleted: u64,
    pub file_blobs_deleted: u64,
    pub blob_symbols_deleted: u64,
    pub orphan_branch_symbols: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Content-batch description: immutable, shared rows.
#[derive(Clone, Debug, Default)]
pub struct ContentBatch {
    pub files: Vec<ContentFile>,
    pub removed_blobs: Vec<(BlobHash, Language)>,
}

/// One file's parsed content, ready to be written.
#[derive(Clone, Debug)]
pub struct ContentFile {
    pub blob_hash: BlobHash,
    pub language: Language,
    pub imports: String,
    pub size_bytes: u64,
    pub symbols: Vec<ParsedSymbolContent>,
    pub edges: Vec<SymbolEdge>,
}

/// A symbol with its content-level identity and location range in the blob.
#[derive(Clone, Debug)]
pub struct ParsedSymbolContent {
    pub symbol: SymbolDef,
    pub start_line: u32,
    pub end_line: u32,
}

impl ParsedSymbolContent {
    /// Renders the exact embedding template for a specific file path and
    /// imports (docs/08 ?5).
    #[must_use]
    pub fn embedding_template(&self, path: &RepoPath, imports: &str) -> EmbeddingTemplate {
        EmbeddingTemplate::render(path, imports, &self.symbol)
    }
}

/// Outcome of a content batch.
#[derive(Clone, Debug, Default)]
pub struct ContentOutcome {
    pub blobs_inserted: u64,
    pub symbol_defs_inserted: u64,
    pub symbol_edges_inserted: u64,
}

/// Branch-batch description: manifests and occurrences.
#[derive(Clone, Debug, Default)]
pub struct BranchBatch {
    pub files: Vec<BranchFileContent>,
    pub removed_files: Vec<RepoPath>,
    pub state: Option<BranchState>,
}

/// One file's branch-level content.
#[derive(Clone, Debug)]
pub struct BranchFileContent {
    pub file_path: RepoPath,
    pub blob_hash: BlobHash,
    pub language: Language,
    pub symbols: Vec<BranchSymbolRow>,
    pub edges: Vec<BranchEdgeRow>,
}

/// A `branch_symbols` row plus its rendered embedding template text.
#[derive(Clone, Debug)]
pub struct BranchSymbolRow {
    pub symbol_hash: SymbolHash,
    pub start_line: u32,
    pub end_line: u32,
    pub embedding_key: EmbeddingKey,
    pub embedding_text: String,
}

/// A `branch_edges` row with an optional resolved target.
#[derive(Clone, Debug)]
pub struct BranchEdgeRow {
    pub source_symbol_hash: SymbolHash,
    pub target_name: String,
    pub relation: crate::domain::EdgeRelation,
    pub target_symbol_hash: Option<SymbolHash>,
}

/// Outcome of a branch batch.
#[derive(Clone, Debug, Default)]
pub struct BranchOutcome {
    pub files_written: u64,
    pub symbols_written: u64,
    pub edges_written: u64,
    pub nulled_target_names: Vec<String>,
}

/// A pending vector to embed before the branch batch is applied.
#[derive(Clone, Debug)]
pub struct PendingVector {
    pub embedding_key: EmbeddingKey,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Digest;

    fn sample_symbol() -> SymbolDef {
        SymbolDef {
            symbol_hash: SymbolHash::parse("a".repeat(64)).unwrap(),
            language: Language::Rust,
            name: "authenticate".into(),
            qualified_name: "authenticate".into(),
            kind: SymbolKind::Function,
            signature: "fn authenticate(req: Request) -> Result<Claims, AuthError>".into(),
            docstring: Some("Memeriksa bearer token".into()),
            code_snippet: "fn authenticate() {}".into(),
            body_hash: Digest::parse("b".repeat(64)).unwrap(),
        }
    }

    #[test]
    fn health_ok_requires_every_check() {
        let clean = StoreHealth {
            quick_check_ok: true,
            ..StoreHealth::default()
        }
        .finish();
        assert!(clean.ok);

        let dirty = StoreHealth {
            quick_check_ok: true,
            orphan_vectors: 1,
            ..StoreHealth::default()
        }
        .finish();
        assert!(!dirty.ok);
    }

    #[test]
    fn embedding_template_uses_path_and_imports() {
        let symbol = sample_symbol();
        let path = RepoPath::parse("src/auth.rs").unwrap();
        let template = EmbeddingTemplate::render(&path, "use axum::http::Request;", &symbol);
        let text = template.text();
        assert!(text.contains("path: src/auth.rs"));
        assert!(text.contains("imports: use axum::http::Request;"));
        assert!(text.contains("---"));
    }
}
