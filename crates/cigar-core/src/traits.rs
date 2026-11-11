//! Ports implemented by adapters (docs/06, docs/21 ?8.3).
//!
//! Every trait is object-safe and blocking. Core never knows about SQLite,
//! ONNX, or the CLI.

use crate::domain::{
    BlobHash, BranchBatch, BranchEdgeRef, BranchFile, BranchName, BranchOutcome, BranchState,
    ChangeStatus, CommitId, ContentBatch, ContentOutcome, Digest, GcOptions, GcReport,
    GraphNeighborhood, Language, Namespace, OccurrenceRef, RepoPath, ScoredOccurrence, StoreHealth,
    StoreStats, SymbolHash, SymbolOccurrence, SyncMode, SyncStatus,
};
use crate::error::{EngineError, Result};
use crate::hash::EmbeddingTemplate;

/// Embedding backend.
pub trait Embedder: Send + Sync {
    /// Stable model identifier, e.g. `granite-embedding-97m`.
    fn model_id(&self) -> &str;

    /// Output dimension; must match config and the vec0 column.
    fn dimensions(&self) -> usize;

    /// Maximum tokens per input, after tokenization.
    fn max_tokens(&self) -> usize;

    /// Model revision string recorded in `meta`.
    fn revision(&self) -> &str {
        ""
    }

    /// Token estimate used for context budgeting (docs/06 ?3).
    fn count_tokens(&self, text: &str) -> usize {
        let chars = text.chars().count();
        chars.div_ceil(4).saturating_mul(11) / 10
    }

    /// Embeds a batch; the result length must equal `texts.len()`.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embeds a query, applying the model's query prefix if any.
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text.to_owned()])?;
        out.pop().ok_or_else(|| EngineError::Embedding {
            message: "embedder tidak mengembalikan vektor query".into(),
        })
    }

    /// Renders and keys a template using this model's token budget.
    /// Implementations may override to share a tokenizer.
    fn template_for(
        &self,
        path: &RepoPath,
        imports: &str,
        symbol: &crate::domain::SymbolDef,
        template_version: u32,
    ) -> (EmbeddingTemplate, crate::domain::EmbeddingKey) {
        let counter = |text: &str| self.count_tokens(text);
        let template = EmbeddingTemplate::render_with_budget(
            path,
            imports,
            symbol,
            Some(self.max_tokens()),
            Some(&counter),
        );
        let key = template.key(template_version);
        (template, key)
    }
}

/// Persistent store.
pub trait ContextStore: Send + Sync {
    fn init_schema(&self, dimensions: usize) -> Result<()>;
    fn schema_version(&self) -> Result<u32>;
    fn health_check(&self) -> Result<StoreHealth>;

    fn get_branch_state(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
    ) -> Result<Option<BranchState>>;
    fn set_branch_state(&self, namespace: &Namespace, state: &BranchState) -> Result<()>;
    fn list_branches(&self, namespace: &Namespace) -> Result<Vec<BranchState>>;

    fn get_branch_manifest(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
    ) -> Result<Vec<BranchFile>>;

    fn apply_content_batch(
        &self,
        namespace: &Namespace,
        batch: ContentBatch,
    ) -> Result<ContentOutcome>;
    fn apply_branch_batch(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        batch: BranchBatch,
    ) -> Result<BranchOutcome>;

    fn search_symbols(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        query_vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<ScoredOccurrence>>;

    /// Searches the deterministic FTS5 lexical index. Implementations combine
    /// weighted BM25 with graph centrality and return the same occurrence
    /// projection as vector search, so callers can use one expansion pipeline.
    fn search_lexical(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<ScoredOccurrence>>;

    fn get_neighbors(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        occurrence: &OccurrenceRef,
        depth: u8,
    ) -> Result<GraphNeighborhood>;

    fn get_occurrence(
        &self,
        namespace: &Namespace,
        occurrence: &OccurrenceRef,
    ) -> Result<Option<SymbolOccurrence>>;

    fn find_symbols(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        name: &str,
        limit: usize,
    ) -> Result<Vec<SymbolOccurrence>>;

    fn stats(&self, namespace: &Namespace, branch: &BranchName) -> Result<StoreStats>;

    fn unresolved_edges_for_name(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        target_name: &str,
        limit: usize,
    ) -> Result<Vec<BranchEdgeRef>>;

    fn resolve_edge(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        edge: &BranchEdgeRef,
        target: &SymbolHash,
    ) -> Result<()>;

    fn gc(&self, namespace: &Namespace, opts: GcOptions) -> Result<GcReport>;

    /// Stores a vector for an `embedding_key` if it is not present yet.
    fn ensure_vectors(
        &self,
        namespace: &Namespace,
        vectors: &[(crate::domain::EmbeddingKey, Vec<f32>)],
    ) -> Result<u64>;

    /// Returns the set of `embedding_key`s already stored in this namespace.
    fn existing_vector_keys(
        &self,
        namespace: &Namespace,
        keys: &[crate::domain::EmbeddingKey],
    ) -> Result<Vec<bool>>;

    /// Reads blob-level symbols for a parsed blob (for branch materialization).
    fn blob_symbols(&self, blob: &BlobHash, language: Language) -> Result<Vec<BlobSymbolRow>>;
}

/// One `blob_symbols` row joined with its definition, used to build branch
/// occurrences without reparsing.
#[derive(Clone, Debug)]
pub struct BlobSymbolRow {
    pub symbol_hash: SymbolHash,
    pub start_line: u32,
    pub end_line: u32,
    pub symbol: crate::domain::SymbolDef,
    pub imports: String,
}

/// Git or filesystem source.
///
/// `gix::Repository` is deliberately not `Send + Sync`, so this port is used
/// only on the thread that performs delta computation. Parsing, hashing, and
/// embedding happen after all bytes are read.
pub trait SourceProvider {
    fn mode(&self) -> crate::domain::SourceMode;
    fn head(&self) -> Result<Option<CommitId>>;
    fn head_branch(&self) -> Result<Option<BranchName>>;
    fn changes(&self, base: Option<&CommitId>, mode: SyncMode) -> Result<ChangeSet>;
    fn read(&self, path: &RepoPath, revision: Revision) -> Result<Vec<u8>>;
    fn fingerprint(&self) -> Result<Option<Digest>>;

    /// Worktree root, when this source has one. `None` for bare repositories.
    fn workdir(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Files available at `revision`, sorted. Used by `--full`.
    fn list(&self, revision: Revision) -> Result<Vec<RepoPath>> {
        let _ = revision;
        Ok(Vec::new())
    }
}

/// Changes to apply for one sync.
#[derive(Clone, Debug, Default)]
pub struct ChangeSet {
    pub changes: Vec<FileChange>,
    pub base: Option<CommitId>,
    pub head: Option<CommitId>,
    pub mode: SyncMode,
    pub warnings: Vec<String>,
}

/// One changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    pub status: ChangeStatus,
    pub size: Option<u64>,
}

/// Which revision to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Revision {
    Head,
    Commit(CommitId),
    Worktree,
}

/// One language parser.
pub trait LanguageParser: Send + Sync {
    fn language(&self) -> Language;
    fn extensions(&self) -> &'static [&'static str];
    fn parse(&self, path: &RepoPath, source: &str) -> std::result::Result<ParsedFile, ParseError>;
}

/// Parser output (docs/07 ?3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedFile {
    pub imports: String,
    pub symbols: Vec<ParsedSymbol>,
    pub edges: Vec<ParsedEdge>,
}

/// One extracted symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSymbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: crate::domain::SymbolKind,
    pub signature: String,
    pub docstring: Option<String>,
    pub code: String,
    pub start_line: u32,
    pub end_line: u32,
    /// Normalized token stream used for `body_hash` (docs/07 ?7.1).
    pub body_stream: String,
}

/// One extracted edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedEdge {
    pub source_qualified_name: String,
    pub target_name: String,
    pub relation: crate::domain::EdgeRelation,
}

/// Non-fatal parse failure for a single file.
#[derive(Debug, thiserror::Error)]
#[error("gagal memparse {path}: {message}")]
pub struct ParseError {
    pub path: String,
    pub message: String,
}

impl ParseError {
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Progress reporting; must not block the pipeline.
pub trait ProgressSink: Send + Sync {
    fn phase_started(&self, phase: SyncPhase, total: Option<u64>);
    fn phase_advanced(&self, phase: SyncPhase, delta: u64);
    fn phase_finished(&self, phase: SyncPhase, summary: &PhaseSummary);
    fn warn(&self, message: &str);
}

/// Sync phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPhase {
    Lock,
    Delta,
    Parse,
    Hash,
    Embed,
    Write,
    Resolve,
    Publish,
}

/// Per-phase summary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhaseSummary {
    pub processed: u64,
    pub elapsed_ms: u64,
}

/// Injectable clock so tests are deterministic.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Wall-clock implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as i64)
    }
}

/// Progress sink that discards everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopProgress;

impl ProgressSink for NoopProgress {
    fn phase_started(&self, _phase: SyncPhase, _total: Option<u64>) {}
    fn phase_advanced(&self, _phase: SyncPhase, _delta: u64) {}
    fn phase_finished(&self, _phase: SyncPhase, _summary: &PhaseSummary) {}
    fn warn(&self, _message: &str) {}
}

/// Convenience alias for a sync result with its status.
#[derive(Clone, Debug, Default)]
pub struct SyncSummary {
    pub mode: SyncMode,
    pub status: SyncStatus,
    pub files_added: u64,
    pub files_modified: u64,
    pub files_deleted: u64,
    pub files_skipped: u64,
    pub symbols_added: u64,
    pub symbols_modified: u64,
    pub symbols_deleted: u64,
    pub embeddings_computed: u64,
    pub embeddings_reused: u64,
    pub edges_inserted: u64,
    pub edges_resolved: u64,
    pub edges_unresolved: u64,
    pub warnings: Vec<String>,
}
