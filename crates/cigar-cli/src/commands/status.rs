// SPDX-License-Identifier: MPL-2.0

//! `cigar status` (docs/10 ?6).

use cigar_core::error::{EngineError, Result};
pub fn run(context: &Context, args: &StatusArgs, printer: &Printer) -> Result<i32> {
    context.require_initialized()?;
    if args.all_branches {
        return Err(EngineError::invalid(
            "--all-branches tidak didukung oleh indeks lexical",
        ));
    }
    let store = cigar_storage::TargetStore::open(&context.store_path(), false)?;
    let stats = store.stats(context.namespace.as_str())?;
    let head = store.sync_head(context.namespace.as_str())?;
    let problems = if args.verify {
        store.health_check()?
    } else {
        Vec::new()
    };
    let branch = context.resolve_branch()?;
    if printer.json() {
        printer.json_document(&serde_json::json!({
            "schema_version": SCHEMA_VERSION, "engine_version": ENGINE_VERSION,
            "root": context.root, "cig_dir": context.cig_dir,
            "namespace": context.namespace.as_str(), "branch": branch.as_str(),
            "head": head, "search": { "mode": context.config.search.mode },
            "stats": { "files": stats.files, "symbols": stats.symbols, "edges": stats.edges },
            "health": if args.verify { Some(&problems) } else { None },
        }));
    } else {
        printer.line(&format!("CIGAR - {}", context.root_display()));
        printer.field("namespace:", context.namespace.as_str());
        printer.field("branch:", branch.as_str());
        printer.field("head:", head.as_deref().unwrap_or("(not synced)"));
        printer.field("search:", &context.config.search.mode);
        printer.field("files:", &thousands(stats.files));
        printer.field("symbols:", &thousands(stats.symbols));
        printer.field("edges:", &thousands(stats.edges));
        if args.verify {
            if problems.is_empty() {
                printer.line("Verification: ok");
            }
            for problem in &problems {
                printer.warn(problem);
            }
        }
    }
    Ok(if problems.is_empty() {
        0
    } else {
        cigar_core::error::exit::MISMATCH
    })
}
use cigar_core::{ENGINE_VERSION, SCHEMA_VERSION};

use crate::cli::StatusArgs;
use crate::context::Context;
use crate::output::{thousands, Printer};

/// Formats a millisecond epoch as UTC `YYYY-MM-DD HH:MM:SS`.
#[must_use]
pub fn format_timestamp(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let seconds = ms / 1_000;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_utc() {
        assert_eq!(format_timestamp(0), "-");
        assert_eq!(
            format_timestamp(1_700_000_000_000),
            "2023-11-14 22:13:20 UTC"
        );
    }
}
