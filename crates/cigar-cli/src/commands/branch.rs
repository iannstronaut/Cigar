// SPDX-License-Identifier: MPL-2.0

//! Branch snapshots and garbage collection for the lexical index.
use crate::cli::{BranchArgs, BranchCommand, GcArgs};
use crate::context::Context;
use crate::output::Printer;
use cigar_core::error::{EngineError, Result};

pub fn run_branch(context: &Context, args: &BranchArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let store = cigar_storage::TargetStore::open(&context.store_path(), false)?;
    match &args.command {
        BranchCommand::Prune { .. } => Err(EngineError::invalid(
            "branch prune tidak tersedia: indeks lexical menyimpan satu snapshot per namespace",
        )),
        BranchCommand::List => {
            let head = store.sync_head(context.namespace.as_str())?;
            if printer.json() {
                printer.json_document(&serde_json::json!({"schema_version": cigar_core::SCHEMA_VERSION, "namespace": context.namespace.as_str(), "head": head}));
            } else {
                printer.line(&format!(
                    "Snapshot HEAD: {}",
                    head.as_deref().unwrap_or("(belum sync)")
                ));
            }
            Ok(0)
        }
    }
}

pub fn run_gc(context: &Context, _args: &GcArgs, _printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    Err(EngineError::invalid(
        "gc tidak tersedia untuk indeks lexical; penghapusan terjadi atomik saat sync",
    ))
}
