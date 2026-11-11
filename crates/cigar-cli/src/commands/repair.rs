// SPDX-License-Identifier: MPL-2.0

//! Verify the lexical index and report unsupported legacy repairs.
use crate::cli::RepairArgs;
use crate::context::Context;
use crate::output::Printer;
use cigar_core::error::{EngineError, Result};

pub fn run(context: &Context, args: &RepairArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    if args.rebuild_vectors || args.drop_orphans {
        return Err(EngineError::invalid(
            "perbaikan vektor tidak tersedia untuk indeks lexical",
        ));
    }
    if args.head {
        return Err(EngineError::invalid(
            "repair --head tidak tersedia untuk indeks lexical",
        ));
    }
    let store = cigar_storage::TargetStore::open(&context.store_path(), false)?;
    let problems = store.health_check()?;
    if printer.json() {
        printer.json_document(&serde_json::json!({"schema_version": cigar_core::SCHEMA_VERSION, "healthy": problems.is_empty(), "problems": problems}));
    } else if problems.is_empty() {
        printer.line("Indeks konsisten; tidak ada yang perlu diperbaiki.");
    } else {
        for problem in &problems {
            printer.warn(problem);
        }
    }
    Ok(if problems.is_empty() {
        0
    } else {
        cigar_core::error::exit::MISMATCH
    })
}
