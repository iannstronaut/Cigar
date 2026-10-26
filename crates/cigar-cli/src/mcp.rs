//! Minimal MCP stdio server (docs/12).
//!
//! Implements JSON-RPC 2.0 over newline-delimited JSON on stdin/stdout, the
//! framing MCP uses for stdio transports. stdout carries *only* protocol
//! messages; diagnostics go to stderr.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use cigar_core::domain::{BranchName, SymbolKind};
use cigar_core::error::{EngineError, Result};
use cigar_core::query::{render_text, QueryEngine, QueryOptions};
use cigar_core::traits::ContextStore;

use crate::commands::query::{index_info, resolve_indexed_branch};
use crate::context::Context;
use crate::embedding::build_embedder;

/// Supported protocol versions, newest first.
const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];
const SERVER_NAME: &str = "cigar";

/// Options for one server run.
pub struct McpOptions {
    pub allow_write: bool,
}

/// Runs the server until stdin closes.
pub fn run(context: &Context, options: &McpOptions) -> Result<i32> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut initialized = false;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("warning: mcp: gagal membaca stdin: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                respond(
                    &mut stdout,
                    &rpc_error(Value::Null, -32700, &error.to_string()),
                )?;
                continue;
            }
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            if id.is_some() {
                respond(
                    &mut stdout,
                    &rpc_error(id.unwrap_or(Value::Null), -32600, "method wajib ada"),
                )?;
            }
            continue;
        };

        // Notifications never get a response (docs/12 section 7).
        let is_notification = id.is_none();
        match method {
            "initialize" => {
                initialized = true;
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let reply = initialize_result(&params, options);
                if !is_notification {
                    respond(&mut stdout, &rpc_ok(id.unwrap_or(Value::Null), reply))?;
                }
            }
            "notifications/initialized" | "notifications/cancelled" => {}
            "ping" => {
                if !is_notification {
                    respond(&mut stdout, &rpc_ok(id.unwrap_or(Value::Null), json!({})))?;
                }
            }
            "tools/list" => {
                if !initialized {
                    if !is_notification {
                        respond(
                            &mut stdout,
                            &rpc_error(
                                id.unwrap_or(Value::Null),
                                -32002,
                                "server belum diinisialisasi",
                            ),
                        )?;
                    }
                    continue;
                }
                let tools = if options.allow_write {
                    json!([query_tool(), inspect_tool(), status_tool(), sync_tool()])
                } else {
                    json!([query_tool(), inspect_tool(), status_tool()])
                };
                if !is_notification {
                    respond(
                        &mut stdout,
                        &rpc_ok(id.unwrap_or(Value::Null), json!({ "tools": tools })),
                    )?;
                }
            }
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let reply = call_tool(context, options, &params);
                if !is_notification {
                    match reply {
                        Ok(result) => {
                            respond(&mut stdout, &rpc_ok(id.unwrap_or(Value::Null), result))?;
                        }
                        Err(error) if is_protocol_error(&error) => {
                            respond(
                                &mut stdout,
                                &rpc_error(id.unwrap_or(Value::Null), -32602, &error.to_string()),
                            )?;
                        }
                        Err(error) => {
                            // Domain failures are tool errors, not RPC errors.
                            respond(
                                &mut stdout,
                                &rpc_ok(id.unwrap_or(Value::Null), tool_error(&error)),
                            )?;
                        }
                    }
                }
            }
            "resources/list" | "prompts/list" | "logging/setLevel" | "completion/complete" => {
                if !is_notification {
                    respond(
                        &mut stdout,
                        &rpc_error(
                            id.unwrap_or(Value::Null),
                            -32601,
                            &format!("method tidak didukung: {method}"),
                        ),
                    )?;
                }
            }
            _ => {
                if !is_notification {
                    respond(
                        &mut stdout,
                        &rpc_error(
                            id.unwrap_or(Value::Null),
                            -32601,
                            &format!("method tidak dikenal: {method}"),
                        ),
                    )?;
                }
            }
        }
    }
    Ok(0)
}

fn is_protocol_error(error: &EngineError) -> bool {
    matches!(error, EngineError::InvalidInput { .. })
}

fn initialize_result(params: &Value, options: &McpOptions) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = if SUPPORTED_VERSIONS.contains(&requested) {
        requested
    } else {
        SUPPORTED_VERSIONS[0]
    };
    let mut capabilities = json!({ "tools": {} });
    if options.allow_write {
        capabilities["tools"] = json!({ "listChanged": false });
    }
    json!({
        "protocolVersion": version,
        "capabilities": capabilities,
        "serverInfo": {
            "name": SERVER_NAME,
            "version": cigar_core::ENGINE_VERSION,
        },
        "instructions": "CIGAR menyediakan pencarian konteks kode berbasis vektor dan graph. \
                         Gunakan cigar_query untuk mencari, cigar_inspect untuk detail simbol, \
                         dan cigar_status untuk memeriksa indeks.",
    })
}

fn query_tool() -> Value {
    json!({
        "name": "cigar_query",
        "description": "Cari konteks simbol yang relevan (vektor + graph).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": 2000 },
                "branch": { "type": "string", "minLength": 1, "maxLength": 255 },
                "top_k": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5 },
                "depth": { "type": "integer", "minimum": 0, "maximum": 2, "default": 1 },
                "max_tokens": { "type": "integer", "minimum": 256, "maximum": 32000, "default": 6000 },
                "path": { "type": "string" },
                "kind": {
                    "type": "array",
                    "items": { "enum": ["function","method","struct","interface","class","trait","enum"] }
                },
                "min_score": { "type": "number", "minimum": -1, "maximum": 1 }
            },
            "required": ["query"],
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    })
}

fn inspect_tool() -> Value {
    json!({
        "name": "cigar_inspect",
        "description": "Detail simbol dan relasinya.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "minLength": 1, "maxLength": 300 },
                "branch": { "type": "string", "minLength": 1, "maxLength": 255 },
                "depth": { "type": "integer", "minimum": 0, "maximum": 3, "default": 1 },
                "file": { "type": "string" }
            },
            "required": ["symbol"],
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    })
}

fn status_tool() -> Value {
    json!({
        "name": "cigar_status",
        "description": "Status indeks untuk satu branch.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "branch": { "type": "string", "minLength": 1, "maxLength": 255 }
            },
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    })
}

fn sync_tool() -> Value {
    json!({
        "name": "cigar_sync",
        "description": "Sinkronkan indeks (memerlukan --allow-write).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "branch": { "type": "string", "minLength": 1, "maxLength": 255 },
                "dirty": { "type": "boolean", "default": false },
                "full": { "type": "boolean", "default": false },
                "all_branches": { "type": "boolean", "default": false }
            },
            "additionalProperties": false
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
    })
}

fn call_tool(context: &Context, options: &McpOptions, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::invalid("params.name wajib ada"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match name {
        "cigar_query" => tool_query(context, &arguments),
        "cigar_inspect" => tool_inspect(context, &arguments),
        "cigar_status" => tool_status(context, &arguments),
        "cigar_sync" if options.allow_write => tool_sync(context, &arguments),
        "cigar_sync" => Err(EngineError::invalid(
            "cigar_sync tidak tersedia tanpa --allow-write",
        )),
        other => Err(EngineError::invalid(format!("tool tidak dikenal: {other}"))),
    }
}

fn tool_query(context: &Context, arguments: &Value) -> Result<Value> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::invalid("query wajib berupa string"))?;
    let store = open_store(context)?;
    let branch = resolve_branch_arg(context, &store, arguments)?;

    let kinds = arguments
        .get("kind")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|value| value.parse::<SymbolKind>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let path = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(|value| vec![value.to_owned()])
        .unwrap_or_default();

    let options = QueryOptions {
        top_k: clamp_usize(arguments, "top_k", context.config.query.top_k, 1, 50),
        depth: clamp_u8(arguments, "depth", context.config.query.depth, 0, 2),
        max_tokens: clamp_usize(
            arguments,
            "max_tokens",
            context.config.query.max_tokens,
            256,
            32_000,
        ),
        max_neighbors: context.config.query.max_neighbors,
        path_globs: path,
        kinds,
        min_score: arguments
            .get("min_score")
            .and_then(Value::as_f64)
            .map(|value| value as f32),
    };

    let index = index_info(context, &store, &branch)?;
    let embedder = build_embedder(&context.config, context.offline)?;
    let engine = QueryEngine::new(&store, embedder.as_ref());
    let response = engine.run(&context.namespace, &branch, query, &options, index)?;
    Ok(tool_success(
        &render_text(&response, options.max_tokens),
        response.to_json(),
    ))
}

fn tool_inspect(context: &Context, arguments: &Value) -> Result<Value> {
    let symbol = arguments
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::invalid("symbol wajib berupa string"))?;
    let store = open_store(context)?;
    let branch = resolve_branch_arg(context, &store, arguments)?;
    let depth = clamp_u8(arguments, "depth", 1, 0, 3);
    let file = arguments.get("file").and_then(Value::as_str);

    let candidates = store.find_symbols(&context.namespace, &branch, symbol, 50)?;
    let candidates = match file {
        Some(pattern) => candidates
            .into_iter()
            .filter(|candidate| candidate.file_path.as_str() == pattern)
            .collect::<Vec<_>>(),
        None => candidates,
    };

    match candidates.len() {
        0 => Err(EngineError::NotFound {
            entity: "simbol".into(),
            id: symbol.to_owned(),
        }),
        1 => {
            let occurrence = &candidates[0];
            let reference = cigar_core::domain::OccurrenceRef {
                branch: branch.clone(),
                file_path: occurrence.file_path.clone(),
                symbol_hash: occurrence.symbol.symbol_hash.clone(),
                start_line: occurrence.start_line,
            };
            let neighborhood =
                store.get_neighbors(&context.namespace, &branch, &reference, depth)?;
            let payload = serde_json::json!({
                "schema_version": cigar_core::SCHEMA_VERSION,
                "symbol": {
                    "symbol_hash": occurrence.symbol.symbol_hash.to_string(),
                    "qualified_name": occurrence.symbol.qualified_name,
                    "kind": occurrence.symbol.kind.to_string(),
                    "signature": occurrence.symbol.signature,
                    "doc": occurrence.symbol.docstring,
                    "file": occurrence.file_path.to_string(),
                    "start_line": occurrence.start_line,
                    "end_line": occurrence.end_line,
                    "code": occurrence.symbol.code_snippet,
                },
                "relations": neighborhood_json(&neighborhood),
            });
            let text = format!(
                "{} · {} · {}:{}-{}\n{}\n",
                occurrence.symbol.qualified_name,
                occurrence.symbol.kind,
                occurrence.file_path,
                occurrence.start_line,
                occurrence.end_line,
                occurrence.symbol.signature
            );
            Ok(tool_success(&text, payload))
        }
        // Ambiguity is a tool error with candidates listed, never a silent pick.
        count => Err(EngineError::AmbiguousSymbol {
            name: symbol.to_owned(),
            count,
        }),
    }
}

fn tool_status(context: &Context, arguments: &Value) -> Result<Value> {
    let store = open_store(context)?;
    let branch = resolve_branch_arg(context, &store, arguments)?;
    let state = store.get_branch_state(&context.namespace, &branch)?;
    let stats = store.stats(&context.namespace, &branch)?;
    let payload = serde_json::json!({
        "schema_version": cigar_core::SCHEMA_VERSION,
        "namespace": context.namespace.to_string(),
        "branch": branch.to_string(),
        "state": state.as_ref().map(|state| serde_json::json!({
            "commit_hash": state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
            "mode": state.last_sync_mode.to_string(),
            "status": state.status.to_string(),
            "updated_at_ms": state.updated_at_ms,
        })),
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
            "dimensions": context.config.embedding.dimensions,
        },
    });
    let text = format!(
        "branch={} status={} files={} symbols={} vectors={}",
        branch,
        state
            .as_ref()
            .map_or("unknown", |state| state.status.as_str()),
        stats.files,
        stats.symbols,
        stats.vectors
    );
    Ok(tool_success(&text, payload))
}

fn tool_sync(context: &Context, arguments: &Value) -> Result<Value> {
    let dirty = arguments
        .get("dirty")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full = arguments
        .get("full")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let branch_arg = arguments.get("branch").and_then(Value::as_str);

    // Reuse the CLI sync path so locking and status rules stay identical.
    let args = crate::cli::SyncArgs {
        dirty,
        full,
        no_untracked: false,
        jobs: None,
        all_branches: false,
    };
    let mut scoped = context.clone_for(&context.root);
    scoped.branch_override = match branch_arg {
        Some(value) => Some(BranchName::parse(value.to_owned())?),
        None => context.branch_override.clone(),
    };
    let value = crate::commands::sync::run_captured(&scoped, &args)?;
    let symbols = value["branches"]
        .as_array()
        .and_then(|list| list.first())
        .and_then(|first| first["symbols_added"].as_u64())
        .unwrap_or(0);
    let files = value["branches"].as_array().map_or(0, |list| list.len());
    Ok(tool_success(
        &format!("sync selesai: {files} branch, {symbols} simbol"),
        value,
    ))
}
fn neighborhood_json(neighborhood: &cigar_core::domain::GraphNeighborhood) -> Value {
    let list = |items: &[cigar_core::domain::RelatedOccurrence]| {
        items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "file": item.occurrence.file_path.to_string(),
                    "start_line": item.occurrence.start_line,
                    "symbol_hash": item.occurrence.symbol_hash.to_string(),
                    "relation": item.relation.to_string(),
                    "depth": item.depth,
                })
            })
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "callers": list(&neighborhood.callers),
        "callees": list(&neighborhood.callees),
        "types_used": list(&neighborhood.types_used),
        "implements": list(&neighborhood.implements),
        "implemented_by": list(&neighborhood.implemented_by),
        "truncated": neighborhood.truncated,
    })
}

fn open_store(context: &Context) -> Result<cigar_storage::SqliteStore> {
    context.require_initialized()?;
    cigar_storage::open_store(&context.store_path(), context.config.embedding.dimensions)
}

fn resolve_branch_arg(
    context: &Context,
    store: &dyn ContextStore,
    arguments: &Value,
) -> Result<BranchName> {
    match arguments.get("branch").and_then(Value::as_str) {
        Some(name) => {
            let branch = BranchName::parse(name.to_owned())?;
            if store
                .get_branch_state(&context.namespace, &branch)?
                .is_none()
            {
                return Err(EngineError::NotFound {
                    entity: "branch".into(),
                    id: format!("BRANCH_NOT_FOUND: {branch}"),
                });
            }
            Ok(branch)
        }
        None => resolve_indexed_branch(context, store),
    }
}

fn clamp_usize(arguments: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map_or(default, |value| (value as usize).clamp(min, max))
}

fn clamp_u8(arguments: &Value, key: &str, default: u8, min: u8, max: u8) -> u8 {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map_or(default, |value| (value as u8).clamp(min, max))
}

fn tool_success(text: &str, structured: Value) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(error: &EngineError) -> Value {
    let code = error_code(error);
    json!({
        "content": [ { "type": "text", "text": format!("[{code}] {error}") } ],
        "isError": true,
    })
}

/// Stable error codes so agents can react without parsing prose.
#[must_use]
pub fn error_code(error: &EngineError) -> &'static str {
    match error {
        EngineError::NotInitialized { .. } => "NOT_INITIALIZED",
        EngineError::Config { .. } => "INVALID_CONFIG",
        EngineError::InvalidInput { .. } => "INVALID_INPUT",
        EngineError::Git { .. } => "GIT_ERROR",
        EngineError::Parse { .. } => "PARSE_ERROR",
        EngineError::ModelUnavailable { .. } => "MODEL_UNAVAILABLE",
        EngineError::Embedding { .. } => "EMBEDDING_ERROR",
        EngineError::DimensionMismatch { .. } => "DIMENSION_MISMATCH",
        EngineError::Storage { .. } => "STORAGE_ERROR",
        EngineError::LockBusy { .. } => "LOCK_BUSY",
        EngineError::SchemaTooNew { .. } => "SCHEMA_TOO_NEW",
        EngineError::SchemaMismatch { .. } => "SCHEMA_MISMATCH",
        EngineError::ConfigMismatch { .. } => "CONFIG_MISMATCH",
        EngineError::IdCollision { .. } => "ID_COLLISION",
        EngineError::NotFound { .. } => "NOT_FOUND",
        EngineError::AmbiguousSymbol { .. } => "AMBIGUOUS_SYMBOL",
        EngineError::Io { .. } => "IO_ERROR",
        EngineError::Cancelled => "CANCELLED",
        EngineError::Internal { .. } => "INTERNAL_ERROR",
        _ => "UNKNOWN",
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn respond(stdout: &mut std::io::Stdout, value: &Value) -> Result<()> {
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value).map_err(|err| EngineError::Internal {
        message: format!("gagal menulis JSON-RPC: {err}"),
    })?;
    lock.write_all(b"\n")
        .map_err(|err| EngineError::io("<stdout>", err))?;
    lock.flush().map_err(|err| EngineError::io("<stdout>", err))
}
