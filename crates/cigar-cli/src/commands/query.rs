//! `cigar query` (docs/10 section 7).

use cigar_core::domain::{BranchName, SymbolKind};
use cigar_core::error::{EngineError, Result};
use cigar_core::query::{render_text, IndexInfo, QueryEngine, QueryOptions, QueryResponse};
use cigar_core::traits::ContextStore;

use crate::cli::QueryArgs;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar query`.
pub fn run(context: &Context, args: &QueryArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    let store =
        cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)?
            .with_search_config(&context.config.search);
    let branch = resolve_indexed_branch(context, &store)?;

    let kinds = match &args.kind {
        Some(value) => value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<SymbolKind>())
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };

    let options = QueryOptions {
        top_k: args.top_k.unwrap_or(context.config.query.top_k),
        depth: args.depth.unwrap_or(context.config.query.depth),
        max_tokens: args.max_tokens.unwrap_or(context.config.query.max_tokens),
        max_neighbors: args
            .max_neighbors
            .unwrap_or(context.config.query.max_neighbors),
        path_globs: args.path.clone(),
        kinds,
        min_score: args.min_score,
    };

    let index = index_info(context, &store, &branch)?;
    let engine = QueryEngine::new(&store, None);
    let response = engine.run(&context.namespace, &branch, &args.query, &options, index)?;

    emit(printer, &response, options.max_tokens)
}

/// Emits a response in the requested format.
pub fn emit(printer: &Printer, response: &QueryResponse, max_tokens: usize) -> Result<i32> {
    if printer.json() {
        printer.json_document(&response.to_json());
        return Ok(0);
    }
    let text = render_text(response, max_tokens);
    print!("{text}");
    Ok(0)
}

/// Resolves the branch to query, failing clearly when it is not indexed.
pub fn resolve_indexed_branch(context: &Context, store: &dyn ContextStore) -> Result<BranchName> {
    let branch = context.resolve_branch()?;
    if store
        .get_branch_state(&context.namespace, &branch)?
        .is_none()
    {
        return Err(EngineError::NotFound {
            entity: "branch".into(),
            id: format!(
                "{} (namespace {}); jalankan `cigar sync --branch {}`",
                branch, context.namespace, branch
            ),
        });
    }
    Ok(branch)
}

/// Builds the index metadata block, including staleness.
pub fn index_info(
    context: &Context,
    store: &dyn ContextStore,
    branch: &BranchName,
) -> Result<IndexInfo> {
    let state = store.get_branch_state(&context.namespace, branch)?;
    if let Some(state) = &state {
        let actual = context.config.fingerprint()?;
        if state.config_fingerprint != actual {
            return Err(EngineError::ConfigMismatch {
                field: "config_fingerprint".into(),
                expected: state.config_fingerprint.to_string(),
                actual: actual.to_string(),
            });
        }
    }
    let git_head = git_head(context);
    let indexed_head = state
        .as_ref()
        .and_then(|state| state.commit_hash.as_ref())
        .map(|commit| commit.as_str().to_owned());
    let stale = match (&git_head, &indexed_head) {
        (Some(git), Some(indexed)) => git != indexed,
        (Some(_), None) => true,
        _ => false,
    };
    Ok(IndexInfo {
        namespace: context.namespace.to_string(),
        branch: branch.to_string(),
        head: indexed_head.map(|head| head[..head.len().min(7)].to_owned()),
        mode: state.as_ref().map_or_else(
            || "commit".to_owned(),
            |state| state.last_sync_mode.to_string(),
        ),
        status: state
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |state| state.status.to_string()),
        model: context.config.embedding.model.clone(),
        stale,
    })
}

/// Reads the current Git HEAD commit, for staleness reporting only.
fn git_head(context: &Context) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&context.root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!head.is_empty()).then_some(head)
}
