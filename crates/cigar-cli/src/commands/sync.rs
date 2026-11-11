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
use crate::output::Printer;

/// Shared implementation used by the CLI and the MCP `cigar_sync` tool.
///
/// Holds the store lock, resolves branches, runs the engine, and mirrors refs.
pub fn run_captured(context: &Context, args: &SyncArgs) -> Result<serde_json::Value> {
    let _lock = cigar_storage::StoreLock::acquire_with_wait(
        &context.lock_path(),
        std::time::Duration::from_secs(context.wait_secs),
    )?;
    let store = cigar_storage::open_store_for_write(
        &context.store_path(),
        context.config.embedding.dimensions,
    )?
    .with_search_config(&context.config.search);
    let embedder = None;
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

    let current_branch = context.resolve_branch()?;
    let mut payload = Vec::new();
    for branch in &branches {
        let source = open_source(context)?;
        let engine = SyncEngine::new(&store, embedder, &registry, &context.config, &clock);
        let outcome = engine.run(source.as_ref(), &context.namespace, branch, &options)?;
        if mode == SyncMode::Commit {
            update_refs(context, branch, outcome.head.as_ref())?;
            if branch == &current_branch {
                atomic_write(
                    &context.cig_dir.join("HEAD"),
                    outcome
                        .head
                        .as_ref()
                        .map_or("", |head| head.as_str())
                        .as_bytes(),
                )?;
            }
        }
        mirror_refs(context, branch, outcome.head.as_ref())?;
        payload.push(serde_json::json!({
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
        }));
    }

    Ok(serde_json::json!({
        "schema_version": cigar_core::SCHEMA_VERSION,
        "namespace": context.namespace.to_string(),
        "mode": mode.to_string(),
        "branches": payload,
    }))
}

/// Runs `cigar sync`.
pub fn run(context: &Context, args: &SyncArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let value = run_captured(context, args)?;

    if printer.json() {
        printer.json_document(&value);
        return Ok(0);
    }
    if printer.quiet() {
        return Ok(0);
    }

    let branches = value["branches"].as_array().cloned().unwrap_or_default();
    for branch in &branches {
        let name = branch["branch"].as_str().unwrap_or("?");
        let added = branch["files_added"].as_u64().unwrap_or(0);
        let modified = branch["files_modified"].as_u64().unwrap_or(0);
        let deleted = branch["files_deleted"].as_u64().unwrap_or(0);
        let total = added + modified + deleted;
        if branches.len() == 1 {
            if total == 0 {
                printer.line("Tidak ada perubahan.");
            } else {
                printer.line(&format!(
                    "Sync {} - {} file (+{} ~{} -{}) - {} simbol - {} vektor baru - {} dipakai ulang",
                    name,
                    total,
                    added,
                    modified,
                    deleted,
                    branch["symbols_added"].as_u64().unwrap_or(0),
                    branch["embeddings_computed"].as_u64().unwrap_or(0),
                    branch["embeddings_reused"].as_u64().unwrap_or(0)
                ));
            }
            let skipped = branch["files_skipped"].as_u64().unwrap_or(0);
            if skipped > 0 {
                printer.info(&format!(
                    "{skipped} file dilewati (biner/terlalu besar/symlink)"
                ));
            }
            printer.info(&format!(
                "edge: {} ditulis - {} ter-resolve - {} belum",
                branch["edges_inserted"].as_u64().unwrap_or(0),
                branch["edges_resolved"].as_u64().unwrap_or(0),
                branch["edges_unresolved"].as_u64().unwrap_or(0)
            ));
        } else {
            printer.line(&format!(
                "{}: {} file, {} simbol, {} vektor baru",
                name,
                total,
                branch["symbols_added"].as_u64().unwrap_or(0),
                branch["embeddings_computed"].as_u64().unwrap_or(0)
            ));
        }
        for warning in branch["warnings"].as_array().cloned().unwrap_or_default() {
            if let Some(text) = warning.as_str() {
                printer.warn(text);
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
pub fn update_refs(context: &Context, branch: &BranchName, head: Option<&CommitId>) -> Result<()> {
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
