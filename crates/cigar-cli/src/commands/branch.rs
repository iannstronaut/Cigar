//! `cigar branch` and `cigar gc` (docs/10 sections 10-11).

use cigar_core::domain::{BranchName, GcOptions};
use cigar_core::error::Result;
use cigar_core::traits::ContextStore;

use crate::cli::{BranchArgs, BranchCommand, GcArgs};
use crate::context::Context;
use crate::output::{human_bytes, thousands, Printer};

/// Runs `cigar branch ...`.
pub fn run_branch(context: &Context, args: &BranchArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    // Prune mutates; list only reads. Selecting the mode avoids a read-only
    // handle failing mid-prune.
    let store = match &args.command {
        BranchCommand::Prune { .. } => cigar_storage::open_store_for_write(
            &context.store_path(),
            context.config.embedding.dimensions,
        )?,
        BranchCommand::List => {
            cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)?
        }
    };

    match &args.command {
        BranchCommand::List => {
            let branches = store.list_branches(&context.namespace)?;
            if printer.json() {
                let payload = serde_json::json!({
                    "schema_version": cigar_core::SCHEMA_VERSION,
                    "namespace": context.namespace.to_string(),
                    "branches": branches.iter().map(|state| serde_json::json!({
                        "branch": state.branch.to_string(),
                        "commit_hash": state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
                        "status": state.status.to_string(),
                        "mode": state.last_sync_mode.to_string(),
                        "updated_at_ms": state.updated_at_ms,
                    })).collect::<Vec<_>>(),
                });
                printer.json_document(&payload);
                return Ok(0);
            }
            if branches.is_empty() {
                printer.line("Belum ada branch terindeks.");
                return Ok(0);
            }
            printer.line(&format!(
                "{:<30} {:<10} {:<8} {}",
                "BRANCH", "STATUS", "MODE", "COMMIT"
            ));
            for state in branches {
                printer.line(&format!(
                    "{:<30} {:<10} {:<8} {}",
                    state.branch.as_str(),
                    state.status.as_str(),
                    state.last_sync_mode.as_str(),
                    state
                        .commit_hash
                        .as_ref()
                        .map_or("-", |commit| commit.short())
                ));
            }
            Ok(0)
        }
        BranchCommand::Prune { dry_run } => prune(context, &store, *dry_run, printer),
    }
}

fn prune(
    context: &Context,
    store: &cigar_storage::SqliteStore,
    dry_run: bool,
    printer: &Printer,
) -> Result<i32> {
    let indexed = store.list_branches(&context.namespace)?;
    let live = live_branches(context)?;
    let stale: Vec<BranchName> = indexed
        .into_iter()
        .map(|state| state.branch)
        .filter(|branch| !branch.is_detached() && !live.contains(branch))
        .collect();

    if stale.is_empty() {
        printer.line("Tidak ada branch basi.");
        return Ok(0);
    }

    if dry_run {
        for branch in &stale {
            printer.line(&format!("(dry-run) akan dihapus: {branch}"));
        }
        return Ok(0);
    }

    for branch in &stale {
        store.set_branch_state(
            &context.namespace,
            &cigar_core::domain::BranchState {
                namespace: context.namespace.clone(),
                branch: branch.clone(),
                commit_hash: None,
                last_sync_mode: cigar_core::domain::SyncMode::Commit,
                status: cigar_core::domain::SyncStatus::Ready,
                worktree_fingerprint: None,
                config_fingerprint: context.config.fingerprint()?,
                updated_at_ms: 0,
            },
        )?;
        clear_branch(store, context, branch)?;
        // Remove the row itself last: children cascade from branch_refs.
        let batch = cigar_core::domain::BranchBatch {
            files: Vec::new(),
            removed_files: Vec::new(),
            state: None,
        };
        store.apply_branch_batch(&context.namespace, branch, batch)?;
        remove_branch_row(store, context, branch)?;
        printer.line(&format!("dihapus: {branch}"));
    }
    Ok(0)
}

fn clear_branch(
    store: &cigar_storage::SqliteStore,
    context: &Context,
    branch: &BranchName,
) -> Result<()> {
    let manifest = store.get_branch_manifest(&context.namespace, branch)?;
    let batch = cigar_core::domain::BranchBatch {
        files: Vec::new(),
        removed_files: manifest.into_iter().map(|file| file.file_path).collect(),
        state: None,
    };
    store.apply_branch_batch(&context.namespace, branch, batch)?;
    Ok(())
}

fn remove_branch_row(
    store: &cigar_storage::SqliteStore,
    context: &Context,
    branch: &BranchName,
) -> Result<()> {
    store.remove_branch(&context.namespace, branch)
}

/// Branch names that still exist in Git.
fn live_branches(context: &Context) -> Result<Vec<BranchName>> {
    let output = std::process::Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(&context.root)
        .output();
    let Ok(output) = output else {
        return Ok(Vec::new());
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Ok(branch) = BranchName::parse(line.trim().to_owned()) {
            out.push(branch);
        }
    }
    Ok(out)
}

/// Runs `cigar gc`.
pub fn run_gc(context: &Context, args: &GcArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let _lock = cigar_storage::StoreLock::acquire_with_wait(
        &context.lock_path(),
        std::time::Duration::from_secs(context.wait_secs),
    )?;
    let store = cigar_storage::open_store_for_write(
        &context.store_path(),
        context.config.embedding.dimensions,
    )?;

    let report = store.gc(
        &context.namespace,
        GcOptions {
            dry_run: args.dry_run,
            prune_branches: args.prune_branches,
            vacuum: args.vacuum,
        },
    )?;

    if printer.json() {
        printer.json_document(&serde_json::json!({
            "schema_version": cigar_core::SCHEMA_VERSION,
            "dry_run": args.dry_run,
            "branches_pruned": report.branches_pruned,
            "vectors_deleted": report.vectors_deleted,
            "symbol_defs_deleted": report.symbol_defs_deleted,
            "symbol_edges_deleted": report.symbol_edges_deleted,
            "file_blobs_deleted": report.file_blobs_deleted,
            "blob_symbols_deleted": report.blob_symbols_deleted,
            "bytes_before": report.bytes_before,
            "bytes_after": report.bytes_after,
        }));
        return Ok(0);
    }

    let prefix = if args.dry_run { "(dry-run) " } else { "" };
    printer.line(&format!(
        "{prefix}GC selesai - {} vektor - {} definisi - {} edge - {} blob",
        thousands(report.vectors_deleted),
        thousands(report.symbol_defs_deleted),
        thousands(report.symbol_edges_deleted),
        thousands(report.file_blobs_deleted)
    ));
    printer.field(
        "ukuran:",
        &format!(
            "{} -> {}",
            human_bytes(report.bytes_before),
            human_bytes(report.bytes_after)
        ),
    );
    Ok(0)
}
