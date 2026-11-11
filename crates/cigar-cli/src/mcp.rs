// SPDX-License-Identifier: MPL-2.0

//! Minimal MCP stdio server (docs/12).
//!
//! Implements JSON-RPC 2.0 over newline-delimited JSON on stdin/stdout, the
//! framing MCP uses for stdio transports. stdout carries *only* protocol
//! messages; diagnostics go to stderr.

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::commands::{inspect, query};
use crate::context::Context;
use cigar_core::domain::BranchName;
use cigar_core::error::{EngineError, Result};

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
    let text = arguments
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::invalid("query wajib berupa string"))?;
    let store = open_store(context)?;
    validate_branch(context, arguments)?;
    let paths = arguments
        .get("path")
        .and_then(Value::as_str)
        .map(|s| vec![s.to_owned()])
        .unwrap_or_default();
    let kinds = arguments
        .get("kind")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let value = query::query(
        &store,
        context.namespace.as_str(),
        text,
        clamp_usize(arguments, "top_k", context.config.query.top_k, 1, 50),
        &paths,
        &kinds,
        clamp_u8(arguments, "depth", context.config.query.depth, 0, 3),
        context.config.query.max_neighbors,
        clamp_usize(
            arguments,
            "max_tokens",
            context.config.query.max_tokens,
            256,
            32_000,
        ),
        arguments
            .get("min_score")
            .and_then(Value::as_f64)
            .map(|v| v as f32),
        &context.config.search,
    )?;
    Ok(tool_success(&query::render(&value), value))
}

fn tool_inspect(context: &Context, arguments: &Value) -> Result<Value> {
    let name = arguments
        .get("symbol")
        .and_then(Value::as_str)
        .ok_or_else(|| EngineError::invalid("symbol wajib berupa string"))?;
    let store = open_store(context)?;
    validate_branch(context, arguments)?;
    let value = inspect::inspect(
        &store,
        context.namespace.as_str(),
        name,
        arguments.get("file").and_then(Value::as_str),
        clamp_u8(arguments, "depth", 1, 0, 3),
    )?;
    let symbol = &value["symbol"];
    let text = format!(
        "{} · {}:{}\n{}",
        symbol["qualified_name"].as_str().unwrap_or(""),
        symbol["file"].as_str().unwrap_or(""),
        symbol["start_line"],
        symbol["signature"].as_str().unwrap_or("")
    );
    Ok(tool_success(&text, value))
}

fn tool_status(context: &Context, arguments: &Value) -> Result<Value> {
    let store = open_store(context)?;
    validate_branch(context, arguments)?;
    let branch = context.resolve_branch()?;
    let head = store.sync_head(context.namespace.as_str())?;
    let stats = store.stats(context.namespace.as_str())?;
    let value = json!({"schema_version": cigar_core::SCHEMA_VERSION, "namespace": context.namespace.as_str(), "branch": branch.as_str(), "head": head, "stats": {"files": stats.files, "symbols": stats.symbols, "edges": stats.edges}, "search": {"mode": context.config.search.mode}});
    Ok(tool_success(
        &format!(
            "branch={} files={} symbols={}",
            branch, stats.files, stats.symbols
        ),
        value,
    ))
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
        .and_then(|first| first["symbols_written"].as_u64())
        .unwrap_or(0);
    let files = value["branches"].as_array().map_or(0, |list| list.len());
    Ok(tool_success(
        &format!("sync selesai: {files} branch, {symbols} simbol"),
        value,
    ))
}
fn open_store(context: &Context) -> Result<cigar_storage::TargetStore> {
    context.require_initialized()?;
    cigar_storage::TargetStore::open(&context.store_path(), false)
}

fn validate_branch(context: &Context, arguments: &Value) -> Result<()> {
    let current = context.resolve_branch()?;
    if let Some(name) = arguments.get("branch").and_then(Value::as_str) {
        let requested = BranchName::parse(name.to_owned())?;
        if requested != current {
            return Err(EngineError::invalid(
                "indeks lexical hanya mendukung branch checkout saat ini",
            ));
        }
    }
    Ok(())
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
