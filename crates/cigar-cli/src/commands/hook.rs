//! `cigar hook install|uninstall` (docs/11 sections 6-7).
//!
//! Hooks are installed as marker-delimited blocks so an existing hook script is
//! never destroyed. Installation is idempotent and uninstall removes exactly the
//! block CIGAR added.

use std::path::{Path, PathBuf};

use cigar_core::error::{EngineError, Result};

use crate::cli::{HookArgs, HookCommand};
use crate::context::Context;
use crate::output::Printer;

/// Marker that delimits CIGAR's block in a hook file.
const BEGIN_MARKER: &str = "# >>> cigar >>>";
const END_MARKER: &str = "# <<< cigar <<<";

/// Hooks CIGAR manages.
const HOOKS: &[(&str, &str)] = &[
    ("post-commit", "sync_branch"),
    ("post-merge", "sync_branch"),
    ("post-checkout", "sync_branch"),
];

/// Runs `cigar hook ...`.
pub fn run(context: &Context, args: &HookArgs, printer: &Printer) -> Result<i32> {
    match &args.command {
        HookCommand::Install { bin, dry_run, log } => {
            install(context, printer, bin.clone(), *dry_run, log.clone())
        }
        HookCommand::Uninstall => uninstall(context, printer),
    }
}

fn install(
    context: &Context,
    printer: &Printer,
    bin: Option<PathBuf>,
    dry_run: bool,
    log: Option<PathBuf>,
) -> Result<i32> {
    let git_dir = hooks_dir(context)?;
    let binary = bin.unwrap_or_else(default_binary);
    let mut planned = Vec::new();

    for (hook, _kind) in HOOKS {
        let path = git_dir.join(hook);
        let block = hook_block(hook, &binary, context, log.as_deref());
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = replace_block(&existing, &block);
        planned.push((path, existing, updated));
    }

    if dry_run {
        for (path, existing, updated) in &planned {
            if existing == updated {
                printer.line(&format!("{}: sudah terpasang", path.display()));
            } else {
                printer.line(&format!("{}: akan diperbarui", path.display()));
            }
        }
        return Ok(0);
    }

    let mut changed = 0;
    for (path, existing, updated) in planned {
        if existing == updated {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
        }
        std::fs::write(&path, updated).map_err(|err| EngineError::io(&path, err))?;
        make_executable(&path)?;
        changed += 1;
        printer.line(&format!("terpasang: {}", path.display()));
    }
    if changed == 0 {
        printer.line("Hook sudah terpasang.");
    }
    Ok(0)
}

fn uninstall(context: &Context, printer: &Printer) -> Result<i32> {
    let git_dir = hooks_dir(context)?;
    let mut removed = 0;
    for (hook, _kind) in HOOKS {
        let path = git_dir.join(hook);
        let Ok(existing) = std::fs::read_to_string(&path) else {
            continue;
        };
        let updated = strip_block(&existing);
        if updated == existing {
            continue;
        }
        if updated.trim().is_empty() || is_shebang_only(&updated) {
            std::fs::remove_file(&path).map_err(|err| EngineError::io(&path, err))?;
        } else {
            std::fs::write(&path, updated).map_err(|err| EngineError::io(&path, err))?;
            make_executable(&path)?;
        }
        removed += 1;
        printer.line(&format!("dihapus: {}", path.display()));
    }
    if removed == 0 {
        printer.line("Tidak ada hook CIGAR yang terpasang.");
    }
    Ok(0)
}

fn is_shebang_only(script: &str) -> bool {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .all(|line| line.starts_with("#!"))
}

fn hook_block(hook: &str, binary: &Path, context: &Context, log: Option<&Path>) -> String {
    let root = shell_quote(&context.root.display().to_string());
    let bin = shell_quote(&binary.display().to_string());
    let cig = shell_quote(&context.cig_dir.display().to_string());
    let redirect = log.map_or_else(
        || "2>/dev/null || true".to_owned(),
        |path| {
            format!(
                ">>{} 2>&1 || true",
                shell_quote(&path.display().to_string())
            )
        },
    );
    let mode = if hook == "post-checkout" {
        // post-checkout fires on file checkouts too; only branch switches matter.
        "if [ \"$3\" = \"1\" ]; then\n    \"$CIGAR_BIN\" sync --cwd \"$CIGAR_ROOT\" --cig-dir \"$CIGAR_DIR\" \"$CIGAR_WAIT\" >/dev/null MODE || true\nfi"
    } else {
        "\"$CIGAR_BIN\" sync --cwd \"$CIGAR_ROOT\" --cig-dir \"$CIGAR_DIR\" \"$CIGAR_WAIT\" >/dev/null MODE || true"
    };
    let wait = if context.wait_secs > 0 {
        format!("--wait {}", context.wait_secs)
    } else {
        String::new()
    };
    let mode = mode.replace("MODE", &redirect).replace("WAIT", &wait);
    format!(
        "{BEGIN_MARKER}\n# Managed by CIGAR; edits inside this block are overwritten.\nif command -v \"{bin}\" >/dev/null 2>&1; then\n    CIGAR_BIN={bin}\n    CIGAR_ROOT={root}\n    CIGAR_DIR={cig}\n    CIGAR_WAIT=\"{wait}\"\n    {mode}\nfi\n{END_MARKER}\n"
    )
}

fn replace_block(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        let mut out = String::new();
        out.push_str(&existing[..start]);
        out.push_str(block);
        out.push_str(&existing[end + END_MARKER.len()..]);
        return normalize(&out);
    }
    let mut out = String::new();
    if existing.trim().is_empty() {
        out.push_str("#!/bin/sh\n");
    } else {
        out.push_str(existing);
        if !existing.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str(block);
    normalize(&out)
}

fn strip_block(existing: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(BEGIN_MARKER), existing.find(END_MARKER)) {
        let mut out = String::new();
        out.push_str(&existing[..start]);
        out.push_str(&existing[end + END_MARKER.len()..]);
        return normalize(&out);
    }
    existing.to_owned()
}

fn normalize(text: &str) -> String {
    // Collapse the blank lines left behind by block removal, but keep one
    // trailing newline so the script stays POSIX-clean.
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    let mut out = String::new();
    let mut blank = 0;
    for line in lines {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '\\'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn hooks_dir(context: &Context) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "hooks"])
        .current_dir(&context.root)
        .output()
        .map_err(|err| EngineError::io(&context.root, err))?;
    if !output.status.success() {
        return Err(EngineError::Git {
            message: format!(
                "{} bukan repository Git; hook tidak didukung",
                context.root.display()
            ),
        });
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if raw.is_empty() {
        return Err(EngineError::Git {
            message: "tidak bisa menentukan direktori hook".into(),
        });
    }
    let path = PathBuf::from(raw);
    Ok(if path.is_absolute() {
        path
    } else {
        context.root.join(path)
    })
}

fn default_binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cigar"))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|err| EngineError::io(path, err))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).map_err(|err| EngineError::io(path, err))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    // Git for Windows runs hooks through sh; the execute bit is irrelevant.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_preserves_user_content() {
        let existing = "#!/bin/sh\necho user\n";
        let block = format!("{BEGIN_MARKER}\ncigar\n{END_MARKER}\n");
        let updated = replace_block(existing, &block);
        assert!(updated.contains("echo user"));
        assert!(updated.contains(BEGIN_MARKER));
        // Idempotent.
        assert_eq!(replace_block(&updated, &block), updated);
    }

    #[test]
    fn strip_removes_only_cigar_block() {
        let existing = "#!/bin/sh\necho user\n";
        let block = format!("{BEGIN_MARKER}\ncigar\n{END_MARKER}\n");
        let updated = replace_block(existing, &block);
        assert_eq!(strip_block(&updated), "#!/bin/sh\necho user\n");
    }

    #[test]
    fn shell_quote_escapes_spaces_and_quotes() {
        assert_eq!(shell_quote("/a/b"), "/a/b");
        assert_eq!(shell_quote("/a b"), "'/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shebang_only_is_detectable() {
        assert!(is_shebang_only("#!/bin/sh\n"));
        assert!(!is_shebang_only("#!/bin/sh\necho hi\n"));
    }
}
