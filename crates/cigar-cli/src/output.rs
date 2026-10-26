//! Output formatting helpers. stdout is data; stderr is diagnostics.

use std::io::Write;

use serde::Serialize;

/// Output sink honoring `--json` and `--quiet`.
pub struct Printer {
    json: bool,
    quiet: bool,
}

impl Printer {
    #[must_use]
    pub fn new(json: bool, quiet: bool) -> Self {
        Self { json, quiet }
    }

    #[must_use]
    pub fn json(&self) -> bool {
        self.json
    }

    /// Prints a line of data to stdout unless quiet.
    pub fn line(&self, text: &str) {
        if self.quiet {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        // Broken pipe is not an error worth panicking over (docs/10 ?18).
        let _ = writeln!(stdout, "{text}");
    }

    /// Prints a blank separator.
    pub fn blank(&self) {
        if self.quiet {
            return;
        }
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout);
    }

    /// Prints a JSON document. When `--json` is absent this is a no-op, so
    /// callers can branch explicitly on `self.json()`.
    pub fn json_document<T: Serialize>(&self, value: &T) {
        if !self.json || self.quiet {
            return;
        }
        match serde_json::to_string_pretty(value) {
            Ok(text) => self.line(&text),
            Err(err) => eprintln!("error: gagal menserialisasi JSON: {err}"),
        }
    }

    /// Prints a warning to stderr.
    pub fn warn(&self, message: &str) {
        eprintln!("warning: {message}");
    }

    /// Prints an informational message to stderr unless quiet.
    pub fn info(&self, message: &str) {
        if self.quiet {
            return;
        }
        eprintln!("{message}");
    }

    /// Prints a label/value pair aligned for humans.
    pub fn field(&self, label: &str, value: &str) {
        if self.quiet {
            return;
        }
        self.line(&format!("{label:<11}{value}"));
    }
}

/// Formats a byte count as a compact human-readable size.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats an integer with thousands separators.
#[must_use]
pub fn thousands(value: u64) -> String {
    let text = value.to_string();
    let mut out = String::with_capacity(text.len() + text.len() / 3);
    for (index, ch) in text.chars().enumerate() {
        if index > 0 && (text.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }

    #[test]
    fn thousands_groups() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1234), "1,234");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
