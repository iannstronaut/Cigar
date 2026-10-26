//! `cigar status` (docs/10 ?6).

use cigar_core::error::Result;
use cigar_core::traits::ContextStore;
use cigar_core::{ENGINE_VERSION, SCHEMA_VERSION};

use crate::cli::StatusArgs;
use crate::context::Context;
use crate::output::{human_bytes, thousands, Printer};

/// Runs `cigar status`.
pub fn run(context: &Context, args: &StatusArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let store =
        cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)?;

    let branch = context.resolve_branch()?;
    let state = store.get_branch_state(&context.namespace, &branch)?;
    let stats = store.stats(&context.namespace, &branch)?;
    let health = if args.verify {
        Some(store.health_check()?)
    } else {
        None
    };

    let all_branches = if args.all_branches {
        Some(store.list_branches(&context.namespace)?)
    } else {
        None
    };

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "engine_version": ENGINE_VERSION,
            "root": context.root,
            "cig_dir": context.cig_dir,
            "namespace": context.namespace.to_string(),
            "branch": branch.to_string(),
            "state": state.as_ref().map(state_json),
            "stats": {
                "files": stats.files,
                "symbols": stats.symbols,
                "edges": stats.edges,
                "vectors": stats.vectors,
                "unresolved_edges": stats.unresolved_edges,
                "db_size_bytes": stats.db_size_bytes,
            },
            "model": {
                "provider": context.config.embedding.provider.to_string(),
                "model": context.config.embedding.model,
                "revision": context.config.embedding.revision,
                "dimensions": context.config.embedding.dimensions,
                "template_version": context.config.embedding.template_version,
            },
            "health": health.as_ref().map(health_json),
            "branches": all_branches.as_ref().map(|list| list.iter().map(branch_json).collect::<Vec<_>>()),
        });
        printer.json_document(&payload);
        return Ok(exit_code(health.as_ref()));
    }

    printer.line(&format!("CIGAR - {}", context.root_display()));
    printer.field("namespace:", context.namespace.as_str());
    printer.field(
        "branch:",
        &format!(
            "{} - {}",
            branch,
            state
                .as_ref()
                .map_or("(not synced)".to_owned(), |state| state
                    .commit_hash
                    .as_ref()
                    .map_or("(none)".to_owned(), |commit| commit.short().to_owned()))
        ),
    );
    printer.field(
        "status:",
        state
            .as_ref()
            .map_or("unknown", |state| state.status.as_str()),
    );
    printer.field(
        "model:",
        &format!(
            "{} - {}d",
            context.config.embedding.model, context.config.embedding.dimensions
        ),
    );
    printer.field("files:", &thousands(stats.files));
    printer.field("symbols:", &thousands(stats.symbols));
    printer.field(
        "edges:",
        &format!(
            "{} (unresolved {})",
            thousands(stats.edges),
            thousands(stats.unresolved_edges)
        ),
    );
    printer.field("vectors:", &thousands(stats.vectors));
    printer.field(
        "db:",
        &format!(
            "{} - {}",
            human_bytes(stats.db_size_bytes),
            context.store_path().display()
        ),
    );
    if let Some(state) = &state {
        printer.field(
            "last sync:",
            &format!(
                "{} - mode {}",
                format_timestamp(state.updated_at_ms),
                state.last_sync_mode
            ),
        );
    }

    if let Some(branches) = &all_branches {
        printer.blank();
        printer.line("Branches:");
        for branch_state in branches {
            let commit = branch_state
                .commit_hash
                .as_ref()
                .map_or("(none)".to_owned(), |commit| commit.short().to_owned());
            printer.line(&format!(
                "  {:<28} {:<10} {}",
                branch_state.branch.as_str(),
                branch_state.status.as_str(),
                commit
            ));
        }
    }

    if let Some(health) = &health {
        printer.blank();
        printer.line("Verification:");
        printer.line(&format!(
            "  foreign_key_check: {}",
            if health.foreign_key_violations == 0 {
                "ok".to_owned()
            } else {
                format!("{} violation", health.foreign_key_violations)
            }
        ));
        printer.line(&format!(
            "  quick_check: {}",
            if health.quick_check_ok {
                "ok"
            } else {
                "FAILED"
            }
        ));
        printer.line(&format!("  orphan vectors: {}", health.orphan_vectors));
        printer.line(&format!(
            "  symbols without vectors: {}",
            health.symbols_without_vectors
        ));
        printer.line(&format!("  dangling edges: {}", health.dangling_edges));
        printer.line(&format!(
            "  files without symbols: {}",
            health.manifest_files_without_symbols
        ));
        for message in &health.messages {
            printer.warn(message);
        }
        if !health.ok {
            printer.line("hasil: INCONSISTENT (exit 6)");
        }
    }

    Ok(exit_code(health.as_ref()))
}

fn exit_code(health: Option<&cigar_core::domain::StoreHealth>) -> i32 {
    match health {
        Some(health) if !health.ok => cigar_core::error::exit::MISMATCH,
        _ => 0,
    }
}

fn state_json(state: &cigar_core::domain::BranchState) -> serde_json::Value {
    serde_json::json!({
        "namespace": state.namespace.to_string(),
        "branch": state.branch.to_string(),
        "commit_hash": state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
        "last_sync_mode": state.last_sync_mode.to_string(),
        "status": state.status.to_string(),
        "worktree_fingerprint": state.worktree_fingerprint.as_ref().map(|d| d.as_str().to_owned()),
        "config_fingerprint": state.config_fingerprint.as_str(),
        "updated_at_ms": state.updated_at_ms,
    })
}

fn branch_json(state: &cigar_core::domain::BranchState) -> serde_json::Value {
    serde_json::json!({
        "branch": state.branch.to_string(),
        "commit_hash": state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
        "status": state.status.to_string(),
        "mode": state.last_sync_mode.to_string(),
        "updated_at_ms": state.updated_at_ms,
    })
}

fn health_json(health: &cigar_core::domain::StoreHealth) -> serde_json::Value {
    serde_json::json!({
        "ok": health.ok,
        "foreign_key_violations": health.foreign_key_violations,
        "quick_check_ok": health.quick_check_ok,
        "orphan_vectors": health.orphan_vectors,
        "symbols_without_vectors": health.symbols_without_vectors,
        "dangling_edges": health.dangling_edges,
        "files_without_symbols": health.manifest_files_without_symbols,
        "messages": health.messages,
    })
}

/// Formats a millisecond epoch as UTC `YYYY-MM-DD HH:MM:SS`.
#[must_use]
pub fn format_timestamp(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let seconds = ms / 1_000;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_utc() {
        assert_eq!(format_timestamp(0), "-");
        assert_eq!(
            format_timestamp(1_700_000_000_000),
            "2023-11-14 22:13:20 UTC"
        );
    }
}
