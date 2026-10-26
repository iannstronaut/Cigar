//! `cigar version` (docs/10 ?14).

use cigar_core::error::Result;

use crate::cli::VersionArgs;
use crate::commands::status::format_timestamp;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar version`.
pub fn run(_context: &Context, args: &VersionArgs, printer: &Printer) -> Result<i32> {
    let sqlite = sqlite_version();
    let target = target_triple();
    if args.json || printer.json() {
        let payload = serde_json::json!({
            "cigar": cigar_core::ENGINE_VERSION,
            "schema_version": cigar_core::SCHEMA_VERSION,
            "content_version": cigar_core::CONTENT_VERSION,
            "template_version": cigar_core::TEMPLATE_VERSION,
            "rustc": rustc_version(),
            "sqlite": sqlite,
            "sqlite_vec": sqlite_vec_version(),
            "target": target,
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    printer.line(&format!("cigar {}", cigar_core::ENGINE_VERSION));
    printer.line(&format!("schema: {}", cigar_core::SCHEMA_VERSION));
    printer.line(&format!("rustc: {}", rustc_version()));
    printer.line(&format!("sqlite: {sqlite}"));
    printer.line(&format!("sqlite-vec: {}", sqlite_vec_version()));
    printer.line(&format!("target: {target}"));
    let _ = format_timestamp(0);
    Ok(0)
}

fn sqlite_version() -> String {
    cigar_storage::register_sqlite_vec();
    rusqlite::version().to_owned()
}

fn sqlite_vec_version() -> String {
    rusqlite::Connection::open_in_memory()
        .ok()
        .and_then(|connection| {
            connection
                .query_row("SELECT vec_version()", [], |row| row.get::<_, String>(0))
                .ok()
        })
        .unwrap_or_else(|| "unavailable".into())
}

fn rustc_version() -> &'static str {
    option_env!("RUSTC_VERSION")
        .or(option_env!("CARGO_PKG_RUST_VERSION"))
        .unwrap_or("unknown")
}

fn target_triple() -> &'static str {
    option_env!("TARGET").unwrap_or(std::env::consts::ARCH)
}
