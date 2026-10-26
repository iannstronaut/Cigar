//! `cigar sync` (docs/10 section 5).

use cigar_core::domain::{BranchName, CommitId, SourceMode, SyncMode};
use cigar_core::error::{EngineError, Result};
use cigar_core::git::{FsSource, GitSource};
use cigar_core::parse::ParserRegistry;
use cigar_core::sync::{SyncEngine, SyncOptions};
use cigar_core::traits::{SourceProvider, SystemClock};

use crate::cli::SyncArgs;
use crate::commands::init::{atomic_write, refs_path};
use crate::context::Context;
use crate::embedding::build_embedder;
use crate::output::Printer;

/// Runs `cigar sync`.
pub fn run(context: &Context, args: &SyncArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let _lock = cigar_storage::StoreLock::acquire_with_wait(
        &context.lock_path(),
        std::time::Duration::from_secs(context.wait_secs),
    )?;

    let store = cigar_storage::open_store_for_write(
        &context.store_path(),
        context.config.embedding.dimensions,
    )?;
    let embedder = build_embedder(&context.config, context.offline)?;
    let registry = ParserRegistry::with_defaults();
    let clock = SystemClock;

    let branches = if args.all_branches {
        list_git_branches(context)?
    } else {
        vec![context.resolve_branch()?]
    };

    let mode = if args.dirty {
        SyncMode::Dirty
    } else {
        SyncMode::Commit
    };
    let options = SyncOptions {
        mode,
        full: args.full,
        include_untracked: !args.no_untracked,
        strict: context.strict,
    };

    let mut results = Vec::new();
    for branch in &branches {
        let source = open_source(context)?;
        let engine = SyncEngine::new(
            &store,
            embedder.as_ref(),
            &registry,
            &context.config,
            &clock,
        );
        let outcome = engine.run(source.as_ref(), &context.namespace, branch, &options)?;

        if mode == SyncMode::Commit {
            update_refs(context, branch, outcome.head.as_ref())?;
        }
        let _ = mirror_refs(context, branch, outcome.head.as_ref());

        if !printer.json() && !printer.quiet() && branches.len() > 1 {
            printer.line(&format!(
                "{}: {} file (+{} ~{} -{}), {} simbol, {} vektor baru",
                branch,
                outcome.summary.files_added
                    + outcome.summary.files_modified
                    + outcome.summary.files_deleted,
                outcome.summary.files_added,
                outcome.summary.files_modified,
                outcome.summary.files_deleted,
                outcome.summary.symbols_added,
                outcome.summary.embeddings_computed
            ));
        }
        for warning in &outcome.warnings {
            printer.warn(warning);
        }
        results.push(outcome);
    }

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": cigar_core::SCHEMA_VERSION,
            "namespace": context.namespace.to_string(),
            "mode": mode.to_string(),
            "branches": results.iter().map(|outcome| serde_json::json!({
                "branch": outcome.branch.to_string(),
                "head": outcome.head.as_ref().map(|head| head.as_str().to_owned()),
                "status": outcome.status.to_string(),
                "files_added": outcome.summary.files_added,
                "files_modified": outcome.summary.files_modified,
                "files_deleted": outcome.summary.files_deleted,
                "files_skipped": outcome.summary.files_skipped,
                "symbols_added": outcome.summary.symbols_added,
                "embeddings_computed": outcome.summary.embeddings_computed,
                "embeddings_reused": outcome.summary.embeddings_reused,
                "edges_inserted": outcome.summary.edges_inserted,
                "edges_resolved": outcome.summary.edges_resolved,
                "edges_unresolved": outcome.summary.edges_unresolved,
                "warnings": outcome.warnings,
            })).collect::<Vec<_>>(),
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    if !printer.quiet() {
        let outcome = results.first();
        if let Some(outcome) = outcome {
            if branches.len() == 1 {
                if outcome.summary.files_added
                    + outcome.summary.files_modified
                    + outcome.summary.files_deleted
                    == 0
                {
                    printer.line("Tidak ada perubahan.");
                } else {
                    printer.line(&format!(
                        "Sync {} · {} file (+{} ~{} -{}) · {} simbol · {} vektor baru · {} dipakai ulang",
                        outcome.branch,
                        outcome.summary.files_added
                            + outcome.summary.files_modified
                            + outcome.summary.files_deleted,
                        outcome.summary.files_added,
                        outcome.summary.files_modified,
                        outcome.summary.files_deleted,
                        outcome.summary.symbols_added,
                        outcome.summary.embeddings_computed,
                        outcome.summary.embeddings_reused
                    ));
                }
                if outcome.summary.files_skipped > 0 {
                    printer.info(&format!(
                        "{} file dilewati (biner/terlalu besar/symlink)",
                        outcome.summary.files_skipped
                    ));
                }
                printer.info(&format!(
                    "edge: {} ditulis · {} ter-resolve · {} belum",
                    outcome.summary.edges_inserted,
                    outcome.summary.edges_resolved,
                    outcome.summary.edges_unresolved
                ));
            }
        }
    }
    Ok(0)
}

/// Opens the appropriate source for the configured mode.
pub fn open_source(context: &Context) -> Result<Box<dyn SourceProvider>> {
    let max_file_bytes = context.config.git.max_file_bytes;
    match context.config.git.mode {
        cigar_core::domain::GitMode::FsOnly => {
            Ok(Box::new(FsSource::open(&context.root, max_file_bytes)?))
        }
        cigar_core::domain::GitMode::Bare => Ok(Box::new(GitSource::open_bare(
            &context.root,
            max_file_bytes,
        )?)),
        cigar_core::domain::GitMode::Worktree | cigar_core::domain::GitMode::Auto => Ok(Box::new(
            GitSource::discover(&context.root, max_file_bytes)?,
        )),
    }
}

/// Lists branches present in the repository, for `--all-branches`.
fn list_git_branches(context: &Context) -> Result<Vec<BranchName>> {
    let source = open_source(context)?;
    if source.mode() == SourceMode::FsOnly {
        return Ok(vec![BranchName::parse("main")?]);
    }
    let root = context.root.clone();
    let output = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(&root)
        .output()
        .map_err(|err| EngineError::io(&root, err))?;
    if !output.status.success() {
        // Fall back to the current branch rather than failing the sync.
        return Ok(vec![context.resolve_branch()?]);
    }
    let mut branches = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(branch) = BranchName::parse(line.to_owned()) {
            branches.push(branch);
        }
    }
    if branches.is_empty() {
        branches.push(context.resolve_branch()?);
    }
    Ok(branches)
}

/// Updates `.cig/refs/heads/<branch>` after a commit-mode sync.
fn update_refs(context: &Context, branch: &BranchName, head: Option<&CommitId>) -> Result<()> {
    let path = refs_path(&context.cig_dir, branch);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    atomic_write(
        &path,
        format!("{}\n", head.map_or("", |commit| commit.as_str())).as_bytes(),
    )
}

/// Mirrors refs for non-commit modes without advancing them.
fn mirror_refs(context: &Context, branch: &BranchName, head: Option<&CommitId>) -> Result<()> {
    let path = refs_path(&context.cig_dir, branch);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    atomic_write(
        &path,
        format!("{}\n", head.map_or("", |commit| commit.as_str())).as_bytes(),
    )
}
