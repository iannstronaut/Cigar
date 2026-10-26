//! `cigar inspect` (docs/10 section 8).

use cigar_core::domain::{BranchName, OccurrenceRef, SymbolOccurrence};
use cigar_core::error::{EngineError, Result};
use cigar_core::query::{fence_for, RelatedView, Relations};
use cigar_core::traits::ContextStore;

use crate::cli::InspectArgs;
use crate::commands::query::resolve_indexed_branch;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar inspect`.
pub fn run(context: &Context, args: &InspectArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let store =
        cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)?;
    let branch = resolve_indexed_branch(context, &store)?;
    let depth = args.depth.unwrap_or(context.config.query.depth).min(3);

    let candidates = resolve_symbol(&store, context, &branch, args)?;
    match candidates.len() {
        0 => Err(EngineError::NotFound {
            entity: "simbol".into(),
            id: args.symbol.clone(),
        }),
        1 => {
            let occurrence = &candidates[0];
            let reference = OccurrenceRef {
                branch: branch.clone(),
                file_path: occurrence.file_path.clone(),
                symbol_hash: occurrence.symbol.symbol_hash.clone(),
                start_line: occurrence.start_line,
            };
            let neighborhood =
                store.get_neighbors(&context.namespace, &branch, &reference, depth)?;
            let relations = relations_of(&store, context, &neighborhood)?;

            if printer.json() {
                let payload = serde_json::json!({
                    "schema_version": cigar_core::SCHEMA_VERSION,
                    "namespace": context.namespace.to_string(),
                    "branch": branch.to_string(),
                    "symbol": symbol_json(occurrence),
                    "relations": relations_json(&relations),
                });
                printer.json_document(&payload);
                return Ok(0);
            }

            printer.line(&format!(
                "{} · {} · {}",
                occurrence.symbol.qualified_name,
                occurrence.symbol.kind,
                occurrence.symbol.language
            ));
            printer.field(
                "lokasi:",
                &format!(
                    "{}:{}-{}",
                    occurrence.file_path, occurrence.start_line, occurrence.end_line
                ),
            );
            printer.field("signature:", &occurrence.symbol.signature);
            if let Some(doc) = &occurrence.symbol.docstring {
                printer.field("doc:", &doc.replace('\n', " "));
            }
            printer.blank();
            let fence = fence_for(&occurrence.symbol.code_snippet);
            printer.line(&format!("{fence}rust"));
            print!("{}", occurrence.symbol.code_snippet);
            if !occurrence.symbol.code_snippet.ends_with('\n') {
                printer.line("");
            }
            printer.line(&fence);
            render_relations(printer, &relations);
            Ok(0)
        }
        // Ambiguity is an error with candidates listed (docs/05 section 9).
        count => {
            if printer.json() {
                let payload = serde_json::json!({
                    "schema_version": cigar_core::SCHEMA_VERSION,
                    "ambiguous": true,
                    "count": count,
                    "candidates": candidates.iter().map(symbol_json).collect::<Vec<_>>(),
                });
                printer.json_document(&payload);
            } else {
                printer.warn(&format!("nama ambigu: {count} kandidat"));
                for candidate in &candidates {
                    printer.line(&format!(
                        "  {} · {}:{}",
                        candidate.symbol.qualified_name, candidate.file_path, candidate.start_line
                    ));
                }
            }
            Ok(1)
        }
    }
}

/// Resolves a user-supplied symbol reference: exact hash, qualified name, name,
/// then case-insensitive suffix.
fn resolve_symbol(
    store: &dyn ContextStore,
    context: &Context,
    branch: &BranchName,
    args: &InspectArgs,
) -> Result<Vec<SymbolOccurrence>> {
    // A 64-char lowercase hex string is a symbol hash.
    if args.symbol.len() == 64 && args.symbol.bytes().all(|b| b.is_ascii_hexdigit()) {
        if let Ok(hash) = cigar_core::domain::SymbolHash::parse(args.symbol.clone()) {
            let manifest = store.get_branch_manifest(&context.namespace, branch)?;
            for file in manifest {
                let symbols = store.blob_symbols(&file.blob_hash, file.language)?;
                for row in symbols {
                    if row.symbol_hash == hash {
                        let reference = OccurrenceRef {
                            branch: branch.clone(),
                            file_path: file.file_path.clone(),
                            symbol_hash: hash.clone(),
                            start_line: row.start_line,
                        };
                        if let Some(occurrence) =
                            store.get_occurrence(&context.namespace, &reference)?
                        {
                            return Ok(vec![occurrence]);
                        }
                    }
                }
            }
            return Ok(Vec::new());
        }
    }

    let mut candidates = store.find_symbols(&context.namespace, branch, &args.symbol, 50)?;
    if let Some(path) = &args.file {
        candidates.retain(|candidate| matches_glob(path, candidate.file_path.as_str()));
    }
    Ok(candidates)
}

fn matches_glob(pattern: &str, path: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') {
        globset::Glob::new(pattern)
            .ok()
            .map(|glob| glob.compile_matcher().is_match(path))
            .unwrap_or(false)
    } else {
        path == pattern || path.ends_with(&format!("/{pattern}"))
    }
}

fn relations_of(
    store: &dyn ContextStore,
    context: &Context,
    neighborhood: &cigar_core::domain::GraphNeighborhood,
) -> Result<Relations> {
    let mut relations = Relations {
        truncated: neighborhood.truncated,
        ..Relations::default()
    };
    let push = |target: &mut Vec<RelatedView>,
                list: &[cigar_core::domain::RelatedOccurrence],
                relation: &str|
     -> Result<()> {
        for item in list {
            let Some(occurrence) = store.get_occurrence(&context.namespace, &item.occurrence)?
            else {
                continue;
            };
            target.push(RelatedView {
                symbol_hash: occurrence.symbol.symbol_hash.to_string(),
                qualified_name: occurrence.symbol.qualified_name,
                kind: occurrence.symbol.kind.to_string(),
                signature: occurrence.symbol.signature,
                file: occurrence.file_path.to_string(),
                start_line: occurrence.start_line,
                relation: relation.to_owned(),
            });
        }
        Ok(())
    };
    push(&mut relations.callers, &neighborhood.callers, "CALLS")?;
    push(&mut relations.callees, &neighborhood.callees, "CALLS")?;
    push(
        &mut relations.types_used,
        &neighborhood.types_used,
        "USES_TYPE",
    )?;
    push(
        &mut relations.implements,
        &neighborhood.implements,
        "IMPLEMENTS",
    )?;
    push(
        &mut relations.implemented_by,
        &neighborhood.implemented_by,
        "IMPLEMENTS",
    )?;
    Ok(relations)
}

fn render_relations(printer: &Printer, relations: &Relations) {
    printer.blank();
    for (label, list) in [
        ("callers", &relations.callers),
        ("callees", &relations.callees),
        ("types_used", &relations.types_used),
        ("implements", &relations.implements),
        ("implemented_by", &relations.implemented_by),
    ] {
        if list.is_empty() {
            printer.line(&format!("{label}: (none)"));
            continue;
        }
        printer.line(&format!("{label} ({}):", list.len()));
        for view in list {
            printer.line(&format!(
                "  - {}:{} · {} · {}",
                view.file, view.start_line, view.qualified_name, view.kind
            ));
        }
    }
    if relations.truncated {
        printer.warn("relasi terpotong pada batas max_neighbors");
    }
}

fn symbol_json(occurrence: &SymbolOccurrence) -> serde_json::Value {
    serde_json::json!({
        "symbol_hash": occurrence.symbol.symbol_hash.to_string(),
        "name": occurrence.symbol.name,
        "qualified_name": occurrence.symbol.qualified_name,
        "kind": occurrence.symbol.kind.to_string(),
        "language": occurrence.symbol.language.to_string(),
        "signature": occurrence.symbol.signature,
        "doc": occurrence.symbol.docstring,
        "file": occurrence.file_path.to_string(),
        "start_line": occurrence.start_line,
        "end_line": occurrence.end_line,
        "code": occurrence.symbol.code_snippet,
        "embedding_key": occurrence.embedding_key.to_string(),
    })
}

fn relations_json(relations: &Relations) -> serde_json::Value {
    let list = |items: &[RelatedView]| {
        items
            .iter()
            .map(|view| {
                serde_json::json!({
                    "symbol_hash": view.symbol_hash,
                    "qualified_name": view.qualified_name,
                    "kind": view.kind,
                    "signature": view.signature,
                    "file": view.file,
                    "start_line": view.start_line,
                    "relation": view.relation,
                })
            })
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "callers": list(&relations.callers),
        "callees": list(&relations.callees),
        "types_used": list(&relations.types_used),
        "implements": list(&relations.implements),
        "implemented_by": list(&relations.implemented_by),
        "truncated": relations.truncated,
    })
}
