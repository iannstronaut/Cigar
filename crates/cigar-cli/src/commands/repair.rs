//! `cigar repair` (docs/10 section 13, docs/16).
//!
//! Repair is the explicit recovery path: it never runs silently and never
//! deletes branch manifests unless `--head` or `--drop-orphans` was requested.

use cigar_core::error::{EngineError, Result};
use cigar_core::traits::ContextStore;

use crate::cli::RepairArgs;
use crate::commands::init::atomic_write;
use crate::context::Context;
use crate::output::{thousands, Printer};

/// Runs `cigar repair`.
pub fn run(context: &Context, args: &RepairArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;

    let is_read_only = args.verify || (!args.drop_orphans && !args.rebuild_vectors && !args.head);
    let store = if is_read_only {
        cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)?
    } else {
        cigar_storage::open_store_for_write(
            &context.store_path(),
            context.config.embedding.dimensions,
        )?
    };

    let mut report = RepairReport::default();
    let health = store.health_check()?;
    report.health = Some(health.clone());

    if health.ok && is_read_only {
        if printer.json() {
            printer.json_document(&serde_json::json!({
                "schema_version": cigar_core::SCHEMA_VERSION,
                "healthy": true,
                "actions": [],
            }));
            return Ok(0);
        }
        printer.line("Indeks konsisten; tidak ada yang perlu diperbaiki.");
        return Ok(0);
    }

    if args.drop_orphans {
        let gc = store.gc(
            &context.namespace,
            cigar_core::domain::GcOptions {
                dry_run: false,
                prune_branches: false,
                vacuum: true,
            },
        )?;
        report.vectors_dropped = gc.vectors_deleted;
        report.defs_dropped = gc.symbol_defs_deleted;
        report.blobs_dropped = gc.file_blobs_deleted;
    }

    if args.head {
        rewrite_refs(context, &store, &mut report)?;
    }

    if args.rebuild_vectors {
        // Rebuilding vectors means syncing; delegate so hashing, parsing, and
        // keys stay identical to a normal sync.
        let sync_args = crate::cli::SyncArgs {
            dirty: false,
            full: true,
            no_untracked: false,
            jobs: None,
            all_branches: false,
        };
        let value = crate::commands::sync::run_captured(context, &sync_args)?;
        report.vectors_rebuilt = value["branches"]
            .as_array()
            .and_then(|list| list.first())
            .and_then(|first| first["embeddings_computed"].as_u64())
            .unwrap_or(0);
    }

    let final_health = store.health_check()?;

    if printer.json() {
        printer.json_document(&serde_json::json!({
            "schema_version": cigar_core::SCHEMA_VERSION,
            "healthy_before": report.health.as_ref().map(|h| h.ok),
            "healthy_after": final_health.ok,
            "vectors_dropped": report.vectors_dropped,
            "defs_dropped": report.defs_dropped,
            "blobs_dropped": report.blobs_dropped,
            "vectors_rebuilt": report.vectors_rebuilt,
            "refs_written": report.refs_written,
            "health": {
                "foreign_key_violations": final_health.foreign_key_violations,
                "quick_check_ok": final_health.quick_check_ok,
                "orphan_vectors": final_health.orphan_vectors,
                "symbols_without_vectors": final_health.symbols_without_vectors,
                "dangling_edges": final_health.dangling_edges,
                "files_without_symbols": final_health.manifest_files_without_symbols,
            },
        }));
        return Ok(if final_health.ok { 0 } else { 6 });
    }

    printer.line(&format!(
        "Repair selesai - {} vektor dibuang - {} definisi - {} blob",
        thousands(report.vectors_dropped),
        thousands(report.defs_dropped),
        thousands(report.blobs_dropped)
    ));
    if report.refs_written > 0 {
        printer.field("ref:", &format!("{} ditulis ulang", report.refs_written));
    }
    if report.vectors_rebuilt > 0 {
        printer.field(
            "vektor:",
            &format!("{} dibangun ulang", report.vectors_rebuilt),
        );
    }
    printer.field(
        "hasil:",
        if final_health.ok {
            "konsisten"
        } else {
            "masih tidak konsisten (exit 6)"
        },
    );
    Ok(if final_health.ok { 0 } else { 6 })
}

#[derive(Default)]
struct RepairReport {
    health: Option<cigar_core::domain::StoreHealth>,
    vectors_dropped: u64,
    defs_dropped: u64,
    blobs_dropped: u64,
    vectors_rebuilt: u64,
    refs_written: u64,
}

/// Rewrites `.cig/HEAD` and ref files from the database (the database is
/// authoritative; ref files are only a mirror).
fn rewrite_refs(
    context: &Context,
    store: &cigar_storage::SqliteStore,
    report: &mut RepairReport,
) -> Result<()> {
    let branches = store.list_branches(&context.namespace)?;
    if branches.is_empty() {
        return Err(EngineError::storage(
            "tidak ada branch di database; jalankan `cigar reindex`",
        ));
    }
    for state in &branches {
        let path = crate::commands::init::refs_path(&context.cig_dir, &state.branch);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
        }
        atomic_write(
            &path,
            format!(
                "{}\n",
                state
                    .commit_hash
                    .as_ref()
                    .map_or("", |commit| commit.as_str())
            )
            .as_bytes(),
        )?;
        report.refs_written += 1;
    }

    let current = context.resolve_branch()?;
    let head = branches
        .iter()
        .find(|state| state.branch == current)
        .and_then(|state| state.commit_hash.as_ref())
        .map_or("", |commit| commit.as_str());
    atomic_write(&context.cig_dir.join("HEAD"), head.as_bytes())?;
    Ok(())
}
