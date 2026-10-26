//! `cigar` command-line entry point.
//!
//! The CLI is a thin shell: it parses arguments, assembles adapters, and
//! formats output. All domain logic lives in `cigar-core`.

mod app;
mod cli;
mod commands;
mod context;
mod embedding;
mod output;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::Cli::parse_args();
    match app::run(args) {
        Ok(code) => ExitCode::from(code_as_u8(code)),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(code_as_u8(error.exit_code()))
        }
    }
}

fn code_as_u8(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}
