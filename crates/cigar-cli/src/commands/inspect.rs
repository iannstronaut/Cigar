// SPDX-License-Identifier: MPL-2.0

//! Inspect a symbol in the lexical index.

use crate::cli::InspectArgs;
use crate::context::Context;
use crate::output::Printer;
use cigar_core::error::{EngineError, Result};
use cigar_storage::{TargetHit, TargetStore};

pub fn symbol_json(hit: &TargetHit) -> serde_json::Value {
    serde_json::json!({"symbol_id": hit.symbol_id, "name": hit.name,
        "qualified_name": hit.qualified_name, "kind": hit.kind, "signature": hit.signature,
        "doc": hit.docstring, "file": hit.file_path, "start_line": hit.start_line,
        "end_line": hit.end_line, "code": hit.code_snippet})
}

pub fn inspect(
    store: &TargetStore,
    namespace: &str,
    name: &str,
    file: Option<&str>,
    depth: u8,
) -> Result<serde_json::Value> {
    let mut candidates = store.find_symbols(namespace, name)?;
    if let Some(pattern) = file {
        let matcher = globset::Glob::new(pattern)
            .map_err(|e| EngineError::invalid(e.to_string()))?
            .compile_matcher();
        candidates.retain(|hit| matcher.is_match(&hit.file_path));
    }
    if candidates.is_empty() {
        return Err(EngineError::NotFound {
            entity: "simbol".into(),
            id: name.into(),
        });
    }
    if candidates.len() > 1 {
        return Err(EngineError::AmbiguousSymbol {
            name: name.into(),
            count: candidates.len(),
        });
    }
    let hit = &candidates[0];
    let mut relations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![hit.symbol_id.clone()];
    seen.insert(hit.symbol_id.clone());
    for level in 1..=depth.min(3) {
        let mut next = Vec::new();
        for id in frontier {
            for neighbor in store.neighbors(namespace, &id)? {
                if seen.insert(neighbor.symbol.symbol_id.clone()) {
                    next.push(neighbor.symbol.symbol_id.clone());
                }
                relations.push(serde_json::json!({"symbol": symbol_json(&neighbor.symbol), "relation": neighbor.relation, "incoming": neighbor.incoming, "depth": level}));
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    Ok(
        serde_json::json!({"schema_version": cigar_core::SCHEMA_VERSION, "namespace": namespace, "symbol": symbol_json(hit), "relations": relations}),
    )
}

pub fn run(context: &Context, args: &InspectArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    context.resolve_branch()?;
    let store = TargetStore::open(&context.store_path(), false)?;
    let result = inspect(
        &store,
        context.namespace.as_str(),
        &args.symbol,
        args.file.as_deref(),
        args.depth.unwrap_or(context.config.query.depth),
    )?;
    if printer.json() {
        printer.json_document(&result);
    } else {
        let symbol = &result["symbol"];
        printer.line(&format!(
            "{} · {} · {}:{}-{}",
            symbol["qualified_name"].as_str().unwrap_or(""),
            symbol["kind"].as_str().unwrap_or(""),
            symbol["file"].as_str().unwrap_or(""),
            symbol["start_line"],
            symbol["end_line"]
        ));
        printer.field("signature:", symbol["signature"].as_str().unwrap_or(""));
        if let Some(doc) = symbol["doc"].as_str() {
            printer.field("doc:", doc);
        }
        printer.line(symbol["code"].as_str().unwrap_or(""));
        for relation in result["relations"].as_array().into_iter().flatten() {
            printer.line(&format!(
                "  {} {}",
                relation["relation"].as_str().unwrap_or(""),
                relation["symbol"]["qualified_name"].as_str().unwrap_or("")
            ));
        }
    }
    Ok(0)
}
