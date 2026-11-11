// SPDX-License-Identifier: MPL-2.0

//! `cigar sync` (docs/10 section 5).

use cigar_core::domain::{BranchName, CommitId, SyncMode};
use cigar_core::error::{EngineError, Result};
use cigar_core::git::{FsSource, GitSource};
use cigar_core::parse::ParserRegistry;
use cigar_core::sync::target::sync_target;
use cigar_core::traits::SourceProvider;

use crate::cli::SyncArgs;
use crate::commands::init::{atomic_write, refs_path};
use crate::context::Context;
use crate::output::Printer;

/// Shared implementation used by the CLI and the MCP `cigar_sync` tool.
///
/// Holds the store lock while committing one namespace snapshot.
pub fn run_captured(context: &Context, args: &SyncArgs) -> Result<serde_json::Value> {
    if args.all_branches {
        return Err(EngineError::invalid(
            "--all-branches tidak didukung oleh indeks lexical",
        ));
    }
    let _lock = cigar_storage::StoreLock::acquire_with_wait(
        &context.lock_path(),
        std::time::Duration::from_secs(context.wait_secs),
    )?;
    run_locked(context, args)
}

pub(crate) fn run_locked(context: &Context, args: &SyncArgs) -> Result<serde_json::Value> {
    let store = cigar_storage::TargetStore::open(&context.store_path(), true)?;
    let registry = ParserRegistry::with_defaults();
    let source = open_source(context)?;
    let branch = context.resolve_branch()?;
    let mode = if args.dirty {
        SyncMode::Dirty
    } else {
        SyncMode::Commit
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64);
    let outcome = sync_target(
        &store,
        source.as_ref(),
        &registry,
        &context.config,
        context.namespace.as_str(),
        mode,
        args.full,
        now,
        !args.no_untracked,
        context.strict,
    )?;
    if mode == SyncMode::Commit {
        let head = outcome.head.as_deref().map(CommitId::parse).transpose()?;
        update_refs(context, &branch, head.as_ref())?;
        atomic_write(
            &context.cig_dir.join("HEAD"),
            outcome.head.as_deref().unwrap_or("").as_bytes(),
        )?;
    }
    Ok(serde_json::json!({
        "schema_version": cigar_core::SCHEMA_VERSION,
        "namespace": context.namespace.to_string(),
        "mode": mode.to_string(),
        "branches": [{
            "branch": branch.to_string(),
            "head": outcome.head,
            "files_written": outcome.files_written,
            "files_deleted": outcome.files_deleted,
            "symbols_written": outcome.symbols_written,
        }],
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

    let branch = &value["branches"][0];
    printer.line(&format!(
        "Sync {} - {} file ditulis - {} dihapus - {} simbol",
        branch["branch"].as_str().unwrap_or("?"),
        branch["files_written"].as_u64().unwrap_or(0),
        branch["files_deleted"].as_u64().unwrap_or(0),
        branch["symbols_written"].as_u64().unwrap_or(0)
    ));
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
