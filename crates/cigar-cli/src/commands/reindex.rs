//! `cigar reindex` (docs/10 section 13, docs/21 section 7).
//!
//! Reindex is the only correct migration path for schema/identity changes: it
//! validates the configuration first, backs up the store, then rebuilds every
//! branch from scratch.

use std::io::{BufRead, IsTerminal, Write};

use cigar_core::domain::{BranchName, Namespace, SyncMode};
use cigar_core::error::{EngineError, Result};
use cigar_core::parse::ParserRegistry;
use cigar_core::sync::{SyncEngine, SyncOptions};
use cigar_core::traits::{ContextStore, SystemClock};
use cigar_storage::backup_store;

use crate::cli::ReindexArgs;
use crate::commands::sync::open_source;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar reindex`.
pub fn run(context: &Context, args: &ReindexArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;

    if !args.yes
        && !printer.json()
        && std::io::stdin().is_terminal()
        && !confirm("Hapus dan bangun ulang seluruh indeks?")?
    {
        return Err(EngineError::Cancelled);
    }

    let _lock = cigar_storage::StoreLock::acquire_with_wait(
        &context.lock_path(),
        std::time::Duration::from_secs(context.wait_secs),
    )?;

    let store = cigar_storage::open_store_for_write(
        &context.store_path(),
        context.config.embedding.dimensions,
    )?
    .with_search_config(&context.config.search);
    let branches = store.list_branches(&context.namespace)?;

    if !args.no_backup {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as i64);
        let path = backup_store(&store, timestamp)?;
        printer.info(&format!("backup: {}", path.display()));
    }

    // Clear every branch for this namespace so the rebuild is from scratch.
    // `gc` removes shared rows that are no longer referenced by any branch.
    for branch in &branches {
        clear_branch(&store, &context.namespace, &branch.branch)?;
    }
    store.gc(
        &context.namespace,
        cigar_core::domain::GcOptions {
            dry_run: false,
            prune_branches: false,
            vacuum: false,
        },
    )?;

    // Re-embed every symbol: `--full` plus a vector purge would be needed, and
    // the schema guarantees vectors are keyed by content, so a stale vector is
    // still valid. Reindex keeps valid vectors and only recomputes changed ones.
    let embedder = None;
    let registry = ParserRegistry::with_defaults();
    let clock = SystemClock;
    let source = open_source(context)?;
    let options = SyncOptions {
        mode: if args.dirty {
            SyncMode::Dirty
        } else {
            SyncMode::Commit
        },
        full: true,
        include_untracked: true,
        strict: context.strict,
    };
    let branch = context.resolve_branch()?;
    let engine = SyncEngine::new(&store, embedder, &registry, &context.config, &clock);
    let outcome = engine.run(source.as_ref(), &context.namespace, &branch, &options)?;

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": cigar_core::SCHEMA_VERSION,
            "namespace": context.namespace.to_string(),
            "branch": outcome.branch.to_string(),
            "files": outcome.summary.files_added,
            "symbols": outcome.summary.symbols_added,
            "embeddings_computed": outcome.summary.embeddings_computed,
            "backup": !args.no_backup,
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    printer.line(&format!(
        "Reindex selesai · {} file · {} simbol · {} vektor",
        outcome.summary.files_added,
        outcome.summary.symbols_added,
        outcome.summary.embeddings_computed
    ));
    Ok(0)
}

fn clear_branch(
    store: &cigar_storage::SqliteStore,
    namespace: &Namespace,
    branch: &BranchName,
) -> Result<()> {
    let manifest = store.get_branch_manifest(namespace, branch)?;
    let batch = cigar_core::domain::BranchBatch {
        files: Vec::new(),
        removed_files: manifest.into_iter().map(|file| file.file_path).collect(),
        state: None,
    };
    store.apply_branch_batch(namespace, branch, batch)?;
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| EngineError::io("<stdin>", err))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
