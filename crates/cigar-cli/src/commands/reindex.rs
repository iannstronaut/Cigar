// SPDX-License-Identifier: MPL-2.0

//! `cigar reindex` (docs/10 section 13, docs/21 section 7).
//!
//! Rebuilds the current namespace in one atomic sync.

use std::io::{BufRead, IsTerminal, Write};

use cigar_core::error::{EngineError, Result};

use crate::cli::{ReindexArgs, SyncArgs};
use crate::commands::sync::run_locked;
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

    if !args.no_backup {
        let store = cigar_storage::TargetStore::open(&context.store_path(), false)?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis() as i64);
        let path = store.backup(timestamp)?;
        printer.info(&format!("backup: {}", path.display()));
    }
    let value = run_locked(
        context,
        &SyncArgs {
            dirty: args.dirty,
            full: true,
            no_untracked: false,
            jobs: None,
            all_branches: false,
        },
    )?;
    let outcome = &value["branches"][0];

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": cigar_core::SCHEMA_VERSION,
            "namespace": context.namespace.to_string(),
            "branch": outcome["branch"],
            "files": outcome["files_written"],
            "symbols": outcome["symbols_written"],
            "backup": !args.no_backup,
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    printer.line(&format!(
        "Reindex selesai · {} file · {} simbol",
        outcome["files_written"].as_u64().unwrap_or(0),
        outcome["symbols_written"].as_u64().unwrap_or(0)
    ));
    Ok(0)
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
