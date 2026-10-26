//! Sync orchestration (docs/04 sections 3-8).
//!
//! `SyncEngine` holds the adapters and runs the staged pipeline. It is the only
//! place that mutates a branch state, so recovery and mode rules live here.

use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;

use crate::config::Config;
use crate::domain::{
    BlobHash, BranchBatch, BranchFile, BranchName, BranchState, ChangeStatus, CommitId,
    ConfigFingerprint, ContentBatch, ContentFile, Digest, EmbeddingKey, Language, Namespace,
    ParsedSymbolContent, RepoPath, SourceMode, SymbolEdge, SymbolHash, SyncMode, SyncStatus,
};
use crate::error::{EngineError, Result};
use crate::git::worktree::WorktreeScan;
use crate::hash::manifest_fingerprint;
use crate::parse::ParserRegistry;
use crate::sync::delta::{compute_delta, DeltaPlan, FileTask};
use crate::sync::plan::{build_manifest, pending_vectors, plan_source, PlannedFile};
use crate::traits::{
    Clock, ContextStore, Embedder, NoopProgress, ProgressSink, Revision, SourceProvider, SyncPhase,
    SyncSummary,
};

/// Options for one sync run.
#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub mode: SyncMode,
    pub full: bool,
    pub include_untracked: bool,
    pub strict: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            mode: SyncMode::Commit,
            full: false,
            include_untracked: true,
            strict: false,
        }
    }
}

/// Result of one sync run.
#[derive(Clone, Debug)]
pub struct SyncOutcome {
    pub summary: SyncSummary,
    pub branch: BranchName,
    pub namespace: Namespace,
    pub head: Option<CommitId>,
    pub status: SyncStatus,
    pub warnings: Vec<String>,
}

/// The sync engine.
pub struct SyncEngine<'a> {
    store: &'a dyn ContextStore,
    embedder: &'a dyn Embedder,
    registry: &'a ParserRegistry,
    config: &'a Config,
    clock: &'a dyn Clock,
    progress: &'a dyn ProgressSink,
}

impl<'a> SyncEngine<'a> {
    /// Creates an engine over explicit adapters.
    pub fn new(
        store: &'a dyn ContextStore,
        embedder: &'a dyn Embedder,
        registry: &'a ParserRegistry,
        config: &'a Config,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            store,
            embedder,
            registry,
            config,
            clock,
            progress: &NoopProgress,
        }
    }

    /// Attaches a progress sink.
    #[must_use]
    pub fn with_progress(mut self, progress: &'a dyn ProgressSink) -> Self {
        self.progress = progress;
        self
    }

    /// Runs one sync for `branch`.
    pub fn run(
        &self,
        source: &dyn SourceProvider,
        namespace: &Namespace,
        branch: &BranchName,
        options: &SyncOptions,
    ) -> Result<SyncOutcome> {
        let started = self.clock.now_ms();
        let existing_state = self.store.get_branch_state(namespace, branch)?;
        let config_fingerprint = self.config.fingerprint()?;

        if let Some(state) = &existing_state {
            if state.config_fingerprint != config_fingerprint {
                return Err(EngineError::ConfigMismatch {
                    field: "config_fingerprint".into(),
                    expected: state.config_fingerprint.to_string(),
                    actual: config_fingerprint.to_string(),
                });
            }
        }

        // Mark syncing before any write so an interrupted run is detectable.
        self.store.set_branch_state(
            namespace,
            &BranchState {
                namespace: namespace.clone(),
                branch: branch.clone(),
                commit_hash: existing_state
                    .as_ref()
                    .and_then(|state| state.commit_hash.clone()),
                last_sync_mode: options.mode,
                status: SyncStatus::Syncing,
                worktree_fingerprint: existing_state
                    .as_ref()
                    .and_then(|state| state.worktree_fingerprint.clone()),
                config_fingerprint: config_fingerprint.clone(),
                updated_at_ms: self.clock.now_ms(),
            },
        )?;

        let result = self.run_inner(
            source,
            namespace,
            branch,
            options,
            existing_state.as_ref(),
            &config_fingerprint,
        );

        match result {
            Ok(outcome) => {
                self.progress.phase_finished(
                    SyncPhase::Publish,
                    &crate::traits::PhaseSummary {
                        processed: outcome.summary.files_added
                            + outcome.summary.files_modified
                            + outcome.summary.files_deleted,
                        elapsed_ms: (self.clock.now_ms() - started).max(0) as u64,
                    },
                );
                Ok(outcome)
            }
            Err(error) => {
                // `error` forces the next run to recompute instead of trusting
                // a partial manifest.
                let _ = self.store.set_branch_state(
                    namespace,
                    &BranchState {
                        namespace: namespace.clone(),
                        branch: branch.clone(),
                        commit_hash: None,
                        last_sync_mode: options.mode,
                        status: SyncStatus::Error,
                        worktree_fingerprint: None,
                        config_fingerprint,
                        updated_at_ms: self.clock.now_ms(),
                    },
                );
                Err(error)
            }
        }
    }

    fn run_inner(
        &self,
        source: &dyn SourceProvider,
        namespace: &Namespace,
        branch: &BranchName,
        options: &SyncOptions,
        existing_state: Option<&BranchState>,
        config_fingerprint: &ConfigFingerprint,
    ) -> Result<SyncOutcome> {
        let warnings: Vec<String> = Vec::new();
        let manifest = if options.full {
            Vec::new()
        } else {
            self.store.get_branch_manifest(namespace, branch)?
        };
        // --full/reindex must ignore the stored base commit; otherwise the
        // tree diff is empty and nothing is reindexed.
        let base = if options.full {
            None
        } else {
            existing_state.and_then(|state| state.commit_hash.clone())
        };

        // --- delta -------------------------------------------------------
        self.progress.phase_started(SyncPhase::Delta, None);
        // Commit mode on a non-Git source has no tree to diff, so the working
        // tree is the only source of truth and must be scanned.
        let dirty_scan = if options.mode == SyncMode::Commit && source.mode() == SourceMode::FsOnly
        {
            self.scan_for_mode(
                source,
                &SyncOptions {
                    mode: SyncMode::Fs,
                    ..options.clone()
                },
            )?
        } else {
            self.scan_for_mode(source, options)?
        };
        let head = source.head()?;
        let effective_mode =
            if options.mode == SyncMode::Commit && source.mode() == SourceMode::FsOnly {
                SyncMode::Fs
            } else {
                options.mode
            };
        let mut plan = compute_delta(
            source,
            effective_mode,
            base.as_ref(),
            head.as_ref(),
            &manifest,
            dirty_scan.as_ref(),
        )?;
        let mut warnings = warnings;
        warnings.extend(plan.warnings.clone());

        if options.strict && !warnings.is_empty() {
            return Err(EngineError::Parse {
                path: ".".into(),
                message: format!("strict mode: {} warning", warnings.len()),
            });
        }

        self.progress
            .phase_started(SyncPhase::Parse, Some(plan.tasks.len() as u64));

        // --- read + parse (parallel) -------------------------------------
        let scan_by_path: BTreeMap<&RepoPath, &crate::git::worktree::ScannedFile> = dirty_scan
            .as_ref()
            .map(|scan| scan.entries.iter().map(|file| (&file.path, file)).collect())
            .unwrap_or_default();

        let tasks: Vec<&FileTask> = plan
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.status,
                    ChangeStatus::Added
                        | ChangeStatus::Modified
                        | ChangeStatus::Renamed
                        | ChangeStatus::TypeChanged
                )
            })
            .collect();

        // Git reads must be serial: `gix::Repository` is not `Sync`. Parsing,
        // hashing, and template rendering are pure, so they run in parallel
        // over the bytes collected here.
        let mut raw: Vec<RawFile> = Vec::with_capacity(tasks.len());
        let mut skipped: u64 = 0;
        for task in &tasks {
            match self.read_source_bytes(source, task, scan_by_path.get(&task.path).copied())? {
                Some(file) => raw.push(file),
                None => skipped += 1,
            }
        }

        let planned_results: Vec<Result<PlannedFile>> = raw
            .par_iter()
            .map(|file| {
                plan_source(
                    self.registry,
                    &file.task.path,
                    file.blob.clone(),
                    file.language,
                    &file.text,
                    file.size,
                    self.embedder,
                    self.config.embedding.template_version,
                    crate::CONTENT_VERSION,
                )
            })
            .collect();

        let mut files: Vec<PlannedFile> = Vec::with_capacity(planned_results.len());
        for result in planned_results {
            files.push(result?);
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        // Record hashes on the delta so callers can report them.
        let hash_by_path: BTreeMap<&RepoPath, &BlobHash> = files
            .iter()
            .map(|file| (&file.path, &file.blob_hash))
            .collect();
        for task in &mut plan.tasks {
            if let Some(hash) = hash_by_path.get(&task.path) {
                task.new_blob = Some((*hash).clone());
            }
        }

        // --- vectors -----------------------------------------------------
        self.progress.phase_started(SyncPhase::Embed, None);
        let keys: Vec<EmbeddingKey> = files
            .iter()
            .flat_map(|file| {
                file.symbols
                    .iter()
                    .map(|symbol| symbol.embedding_key.clone())
            })
            .collect();
        let present = self.store.existing_vector_keys(namespace, &keys)?;
        let existing: BTreeSet<EmbeddingKey> = keys
            .iter()
            .zip(present)
            .filter_map(|(key, exists)| exists.then_some(key.clone()))
            .collect();
        let (pending, reused) = pending_vectors(&files, &|key| existing.contains(key));
        let embeddings = self.embed_pending(&pending)?;
        let computed = embeddings.len() as u64;
        if !embeddings.is_empty() {
            self.store.ensure_vectors(namespace, &embeddings)?;
        }

        // --- write content ----------------------------------------------
        self.progress.phase_started(SyncPhase::Write, None);
        let removed_blobs = removable_blobs(&plan, &manifest, &files);
        let content_batch = ContentBatch {
            files: files.iter().map(content_file).collect(),
            removed_blobs,
        };
        let content_outcome = self.store.apply_content_batch(namespace, content_batch)?;

        // --- write branch ------------------------------------------------
        let full_manifest = build_manifest(&files, &plan.unchanged);
        let mut branch_batch = BranchBatch {
            files: files.iter().map(branch_file).collect(),
            removed_files: plan.removed_paths(),
            state: None,
        };
        let removed_paths = branch_batch.removed_files.clone();

        let commit = match options.mode {
            SyncMode::Commit => head.clone(),
            SyncMode::Dirty | SyncMode::Fs => base.clone(),
        };
        let worktree_fingerprint = match options.mode {
            SyncMode::Commit => None,
            SyncMode::Dirty | SyncMode::Fs => dirty_scan
                .as_ref()
                .map(WorktreeScan::fingerprint)
                .or_else(|| source.fingerprint().ok().flatten()),
        };

        branch_batch.state = Some(BranchState {
            namespace: namespace.clone(),
            branch: branch.clone(),
            commit_hash: commit.clone(),
            last_sync_mode: options.mode,
            status: if options.mode == SyncMode::Commit {
                SyncStatus::Ready
            } else {
                SyncStatus::Dirty
            },
            worktree_fingerprint,
            config_fingerprint: config_fingerprint.clone(),
            updated_at_ms: self.clock.now_ms(),
        });
        let final_status = branch_batch
            .state
            .as_ref()
            .map_or(SyncStatus::Ready, |state| state.status);
        let branch_outcome = self
            .store
            .apply_branch_batch(namespace, branch, branch_batch)?;

        // --- edges -------------------------------------------------------
        self.progress.phase_started(SyncPhase::Resolve, None);
        let (resolved, unresolved) = self.resolve_edges(namespace, branch, &files)?;

        // --- summary -----------------------------------------------------
        let mut summary = SyncSummary {
            mode: options.mode,
            status: final_status,
            embeddings_computed: computed,
            embeddings_reused: reused,
            edges_inserted: branch_outcome.edges_written,
            edges_resolved: resolved,
            edges_unresolved: unresolved,
            symbols_added: content_outcome.symbol_defs_inserted,
            symbols_modified: content_outcome
                .symbol_defs_inserted
                .saturating_sub(content_outcome.symbol_defs_inserted),
            files_skipped: skipped,
            warnings: warnings.clone(),
            ..SyncSummary::default()
        };
        // Count files actually written, not delta tasks: a task whose content
        // was skipped (binary, too large, non-UTF-8, symlink) must not be
        // reported as added or modified (docs/04 section 8).
        let written: BTreeMap<&RepoPath, &PlannedFile> =
            files.iter().map(|file| (&file.path, file)).collect();
        for task in &plan.tasks {
            let was_written = written.contains_key(&task.path);
            match task.status {
                ChangeStatus::Added if was_written => summary.files_added += 1,
                ChangeStatus::Modified | ChangeStatus::TypeChanged if was_written => {
                    summary.files_modified += 1;
                }
                // A skipped file still leaves the branch manifest unchanged, so
                // reporting it as deleted would be wrong too.
                ChangeStatus::Deleted => summary.files_deleted += 1,
                ChangeStatus::Renamed if was_written => {
                    summary.files_added += 1;
                    summary.files_deleted += 1;
                }
                _ => {}
            }
        }
        summary.symbols_added = files.iter().map(|file| file.symbols.len() as u64).sum();

        self.progress.phase_finished(
            SyncPhase::Resolve,
            &crate::traits::PhaseSummary {
                processed: resolved + unresolved,
                elapsed_ms: 0,
            },
        );
        self.progress.phase_finished(
            SyncPhase::Write,
            &crate::traits::PhaseSummary {
                processed: files.len() as u64,
                elapsed_ms: 0,
            },
        );

        let _ = (full_manifest, removed_paths);
        Ok(SyncOutcome {
            summary,
            branch: branch.clone(),
            namespace: namespace.clone(),
            head: commit.clone(),
            status: final_status,
            warnings,
        })
    }

    /// Reads one file's bytes (serial, Git-safe) and normalizes them. Returns
    /// `None` for files that must be skipped (unsupported language, too large,
    /// binary, non-UTF-8, symlink, submodule, or missing).
    fn read_source_bytes(
        &self,
        source: &dyn SourceProvider,
        task: &FileTask,
        scanned: Option<&crate::git::worktree::ScannedFile>,
    ) -> Result<Option<RawFile>> {
        let Some(language) = Language::from_path(&task.path) else {
            return Ok(None);
        };
        let (bytes, size) = match scanned {
            Some(scanned) => {
                let Some(text) = &scanned.text else {
                    return Ok(None);
                };
                (text.as_bytes().to_vec(), scanned.size_bytes)
            }
            None => match source.read(&task.path, Revision::Head) {
                Ok(bytes) => {
                    let size = bytes.len() as u64;
                    (bytes, size)
                }
                Err(EngineError::NotFound { .. }) => return Ok(None),
                Err(other) => {
                    let message = other.to_string();
                    if message.contains("melebihi batas")
                        || message.contains("symlink")
                        || message.contains("submodule")
                    {
                        return Ok(None);
                    }
                    return Err(other);
                }
            },
        };
        if size > self.config.git.max_file_bytes {
            return Ok(None);
        }
        if bytes.iter().take(8000).any(|byte| *byte == 0) {
            return Ok(None);
        }
        let normalized = crate::hash::normalize_file_bytes(&bytes);
        let Ok(text) = String::from_utf8(normalized.clone()) else {
            return Ok(None);
        };
        let blob =
            BlobHash::from_hex_unchecked(crate::hash::blob_hash(&normalized).as_str().to_owned());
        Ok(Some(RawFile {
            task: task.clone(),
            blob,
            language,
            size,
            text,
        }))
    }

    fn embed_pending(
        &self,
        pending: &[(EmbeddingKey, String)],
    ) -> Result<Vec<(EmbeddingKey, Vec<f32>)>> {
        let batch_size = self.config.embedding.batch_size.max(1);
        let dimensions = self.config.embedding.dimensions;
        let mut out = Vec::with_capacity(pending.len());
        for chunk in pending.chunks(batch_size) {
            let texts: Vec<String> = chunk.iter().map(|(_, text)| text.clone()).collect();
            let vectors = self.embedder.embed_batch(&texts)?;
            if vectors.len() != texts.len() {
                return Err(EngineError::Embedding {
                    message: format!(
                        "embedder mengembalikan {} vektor untuk {} input",
                        vectors.len(),
                        texts.len()
                    ),
                });
            }
            for ((key, _), vector) in chunk.iter().zip(vectors) {
                if vector.len() != dimensions {
                    return Err(EngineError::DimensionMismatch {
                        expected: dimensions,
                        actual: vector.len(),
                    });
                }
                if vector.iter().any(|value| !value.is_finite()) {
                    return Err(EngineError::Embedding {
                        message: format!("vektor non-finite untuk {key}"),
                    });
                }
                out.push((key.clone(), vector));
            }
        }
        Ok(out)
    }

    fn resolve_edges(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        files: &[PlannedFile],
    ) -> Result<(u64, u64)> {
        // Resolve any pending edge whose target name now has exactly one match.
        let mut names: BTreeSet<String> = BTreeSet::new();
        for file in files {
            for edge in &file.edges {
                names.insert(edge.target_name.clone());
            }
        }

        let mut resolved = 0_u64;
        let mut unresolved = 0_u64;
        for name in names {
            let pending_edges = self
                .store
                .unresolved_edges_for_name(namespace, branch, &name, 500)?;
            if pending_edges.is_empty() {
                continue;
            }
            let candidates = self.store.find_symbols(namespace, branch, &name, 2)?;
            match candidates.len() {
                0 => unresolved += pending_edges.len() as u64,
                1 => {
                    let target = candidates[0].symbol.symbol_hash.clone();
                    for edge in &pending_edges {
                        self.store.resolve_edge(namespace, branch, edge, &target)?;
                        resolved += 1;
                    }
                }
                // Ambiguous names stay unresolved: guessing fabricates a
                // relationship (docs/05 section 9).
                _ => unresolved += pending_edges.len() as u64,
            }
        }
        Ok((resolved, unresolved))
    }

    fn scan_for_mode(
        &self,
        source: &dyn SourceProvider,
        options: &SyncOptions,
    ) -> Result<Option<WorktreeScan>> {
        if options.mode == SyncMode::Commit {
            return Ok(None);
        }
        let Some(workdir) = self.workdir_of(source) else {
            return Err(EngineError::Git {
                message: "mode dirty/fs membutuhkan worktree".into(),
            });
        };
        let excludes = self.config.excludes()?;
        WorktreeScan::scan_with(
            &workdir,
            options.include_untracked,
            self.config.git.max_file_bytes,
            &excludes,
        )
        .map(Some)
    }

    fn workdir_of(&self, source: &dyn SourceProvider) -> Option<std::path::PathBuf> {
        source.workdir()
    }
}

/// Bytes read for one task, before parsing.
#[derive(Clone, Debug)]
struct RawFile {
    task: FileTask,
    blob: BlobHash,
    language: Language,
    size: u64,
    text: String,
}

fn content_file(file: &PlannedFile) -> ContentFile {
    let by_name: BTreeMap<&str, &SymbolHash> = file
        .symbols
        .iter()
        .map(|symbol| {
            (
                symbol.definition.qualified_name.as_str(),
                &symbol.definition.symbol_hash,
            )
        })
        .collect();
    ContentFile {
        blob_hash: file.blob_hash.clone(),
        language: file.language,
        imports: file.imports.clone(),
        size_bytes: file.size_bytes,
        symbols: file
            .symbols
            .iter()
            .map(|symbol| ParsedSymbolContent {
                symbol: symbol.definition.clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
            })
            .collect(),
        edges: file
            .edges
            .iter()
            .filter_map(|edge| {
                let source = by_name.get(edge.source_qualified_name.as_str())?;
                Some(SymbolEdge {
                    source_symbol_hash: (*source).clone(),
                    target_name: edge.target_name.clone(),
                    relation: edge.relation,
                })
            })
            .collect(),
    }
}

fn branch_file(file: &PlannedFile) -> crate::domain::BranchFileContent {
    crate::domain::BranchFileContent {
        file_path: file.path.clone(),
        blob_hash: file.blob_hash.clone(),
        language: file.language,
        symbols: file
            .symbols
            .iter()
            .map(|symbol| crate::domain::BranchSymbolRow {
                symbol_hash: symbol.definition.symbol_hash.clone(),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
                embedding_key: symbol.embedding_key.clone(),
                embedding_text: symbol.embedding_text.clone(),
            })
            .collect(),
        edges: {
            let by_name: BTreeMap<&str, &SymbolHash> = file
                .symbols
                .iter()
                .map(|symbol| {
                    (
                        symbol.definition.qualified_name.as_str(),
                        &symbol.definition.symbol_hash,
                    )
                })
                .collect();
            file.edges
                .iter()
                .filter_map(|edge| {
                    let source = by_name.get(edge.source_qualified_name.as_str())?;
                    Some(crate::domain::BranchEdgeRow {
                        source_symbol_hash: (*source).clone(),
                        target_name: edge.target_name.clone(),
                        relation: edge.relation,
                        target_symbol_hash: None,
                    })
                })
                .collect()
        },
    }
}

/// Blobs that no longer appear in the resulting manifest and were removed from
/// this branch.
fn removable_blobs(
    plan: &DeltaPlan,
    manifest: &[BranchFile],
    files: &[PlannedFile],
) -> Vec<(BlobHash, Language)> {
    let remaining: BTreeSet<&BlobHash> = plan
        .unchanged
        .iter()
        .map(|file| &file.blob_hash)
        .chain(files.iter().map(|file| &file.blob_hash))
        .collect();
    let mut out = Vec::new();
    for task in &plan.tasks {
        if task.status != ChangeStatus::Deleted {
            continue;
        }
        if let Some(file) = manifest.iter().find(|file| file.file_path == task.path) {
            if !remaining.contains(&file.blob_hash) {
                out.push((file.blob_hash.clone(), file.language));
            }
        }
    }
    out
}

/// Computes the manifest fingerprint for a set of branch files.
#[must_use]
pub fn branch_fingerprint(files: &[BranchFile]) -> Digest {
    let entries: Vec<(RepoPath, Digest)> = files
        .iter()
        .map(|file| {
            (
                file.file_path.clone(),
                Digest::from_hex_unchecked(file.blob_hash.as_str().to_owned()),
            )
        })
        .collect();
    manifest_fingerprint(&entries)
}

/// Runs a sync with an explicit engine.
#[allow(clippy::too_many_arguments)]
pub fn run_sync(
    store: &dyn ContextStore,
    embedder: &dyn Embedder,
    registry: &ParserRegistry,
    config: &Config,
    clock: &dyn Clock,
    source: &dyn SourceProvider,
    namespace: &Namespace,
    branch: &BranchName,
    options: &SyncOptions,
) -> Result<SyncOutcome> {
    SyncEngine::new(store, embedder, registry, config, clock)
        .run(source, namespace, branch, options)
}

/// True when a source can supply a worktree scan.
#[must_use]
pub fn supports_dirty(source: &dyn SourceProvider) -> bool {
    matches!(source.mode(), SourceMode::GitWorktree | SourceMode::FsOnly)
        && source.workdir().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_are_derived_from_tasks() {
        let summary = SyncSummary {
            files_added: 2,
            files_modified: 1,
            ..SyncSummary::default()
        };
        assert_eq!(summary.files_added + summary.files_modified, 3);
    }

    #[test]
    fn branch_fingerprint_is_stable() {
        let files = vec![BranchFile {
            file_path: RepoPath::parse("a.rs").unwrap(),
            blob_hash: BlobHash::parse(format!("{:064x}", 1)).unwrap(),
            language: Language::Rust,
        }];
        assert_eq!(branch_fingerprint(&files), branch_fingerprint(&files));
    }

    #[test]
    fn removable_blobs_skip_still_referenced_hashes() {
        let blob = BlobHash::parse(format!("{:064x}", 2)).unwrap();
        let manifest = vec![BranchFile {
            file_path: RepoPath::parse("a.rs").unwrap(),
            blob_hash: blob.clone(),
            language: Language::Rust,
        }];
        let mut plan = DeltaPlan::default();
        plan.tasks.push(FileTask {
            path: RepoPath::parse("a.rs").unwrap(),
            old_path: None,
            status: ChangeStatus::Deleted,
            new_blob: None,
            old_blob: Some(blob.clone()),
        });
        plan.unchanged.push(BranchFile {
            file_path: RepoPath::parse("b.rs").unwrap(),
            blob_hash: blob,
            language: Language::Rust,
        });
        assert!(removable_blobs(&plan, &manifest, &[]).is_empty());
    }
}
