//! Top-level dispatch.

use cigar_core::error::{EngineError, Result};

use crate::cli::{Cli, Command};
use crate::commands;
use crate::context::Context;
use crate::output::Printer;

/// Executes one CLI invocation.
pub fn run(cli: Cli) -> Result<i32> {
    let context = Context::discover(&cli.global)?;
    let printer = Printer::new(cli.global.json, cli.global.quiet);

    match &cli.command {
        Command::Init(args) => commands::init::run(&context, args, &printer),
        Command::Status(args) => commands::status::run(&context, args, &printer),
        Command::Sync(args) => commands::sync::run(&context, args, &printer),
        Command::Model(args) => commands::model::run(&context, args, &printer),
        Command::Reindex(args) => commands::reindex::run(&context, args, &printer),
        Command::Query(args) => commands::query::run(&context, args, &printer),
        Command::Inspect(args) => commands::inspect::run(&context, args, &printer),
        Command::Version(args) => commands::version::run(&context, args, &printer),
        other => Err(EngineError::invalid(format!(
            "perintah {} belum diimplementasikan pada fase ini",
            command_name(other)
        ))),
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Init(_) => "init",
        Command::Sync(_) => "sync",
        Command::Status(_) => "status",
        Command::Query(_) => "query",
        Command::Inspect(_) => "inspect",
        Command::Reindex(_) => "reindex",
        Command::Branch(_) => "branch",
        Command::Gc(_) => "gc",
        Command::Hook(_) => "hook",
        Command::Mcp(_) => "mcp",
        Command::Model(_) => "model",
        Command::Repair(_) => "repair",
        Command::Version(_) => "version",
    }
}
