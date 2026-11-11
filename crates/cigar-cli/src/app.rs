// SPDX-License-Identifier: MPL-2.0

//! Top-level dispatch. Every subcommand is implemented; there is no fallback.

use cigar_core::error::Result;

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
        Command::Branch(args) => commands::branch::run_branch(&context, args, &printer),
        Command::Gc(args) => commands::branch::run_gc(&context, args, &printer),
        Command::Hook(args) => commands::hook::run(&context, args, &printer),
        Command::Repair(args) => commands::repair::run(&context, args, &printer),
        Command::Mcp(args) => {
            let options = crate::mcp::McpOptions {
                allow_write: args.allow_write,
            };
            crate::mcp::run(&context, &options)
        }
        Command::Version(args) => commands::version::run(&context, args, &printer),
    }
}
