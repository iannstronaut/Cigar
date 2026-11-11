// SPDX-License-Identifier: MPL-2.0

//! `cigar init` (docs/10 ?4).

use std::fs;
use std::path::{Path, PathBuf};

use cigar_core::config::{render_default_config, Config};
use cigar_core::domain::GitMode;
use cigar_core::error::{EngineError, Result};
use cigar_core::SCHEMA_VERSION;
use cigar_storage::{StoreLock, TargetStore};

use crate::cli::InitArgs;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar init`.
pub fn run(context: &Context, args: &InitArgs, printer: &Printer) -> Result<i32> {
    let git_mode: GitMode = args.git_mode.parse()?;
    if args.force {
        // `--force` never deletes data; it only revalidates and repairs metadata.
    }

    let existed = context.cig_dir.exists();
    create_state_dir(&context.cig_dir)?;

    let mut config = context.config.clone();
    config.git.mode = git_mode;
    if let Some(model) = &args.model {
        config.embedding.model = model.clone();
    }
    if let Some(dimensions) = args.dimensions {
        config.embedding.dimensions = dimensions;
    }
    if let Some(dir) = &args.model_dir {
        config.embedding.model_dir = dir.clone();
    }
    if let Some(provider) = &args.embedding {
        config.embedding.provider = provider.parse()?;
    }
    config.storage.path = context.store_path();
    config.validate()?;

    write_config_atomic(&context.config_path, &config)?;

    let fingerprint = config.fingerprint()?;
    let _store = TargetStore::open(&context.store_path(), true)?;

    let branch = context.resolve_branch()?;
    let branch_name = branch.clone();
    write_head_ref(&context.cig_dir)?;
    write_branch_marker(&context.cig_dir, &branch_name, None)?;

    exclude_state_dir(&context.root, git_mode)?;

    let _lock = StoreLock::acquire(&context.lock_path())?;
    drop(_lock);

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "root": context.root,
            "cig_dir": context.cig_dir,
            "already_initialized": existed,
            "git_mode": git_mode.to_string(),
            "branch": branch_name.to_string(),
            "search": { "mode": config.search.mode },
            "namespace": config.engine.namespace.to_string(),
            "config_fingerprint": fingerprint.to_string(),
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    if existed {
        printer.line(&format!(
            "CIGAR sudah diinisialisasi - {}",
            context.cig_dir.display()
        ));
    } else {
        printer.line(&format!(
            "CIGAR diinisialisasi - {}",
            context.cig_dir.display()
        ));
    }
    printer.field("namespace:", config.engine.namespace.as_str());
    printer.field("git:", git_mode.as_str());
    printer.field("branch:", branch_name.as_str());
    printer.field("search:", &config.search.mode);
    printer.field("store:", &context.store_path().display().to_string());
    printer.blank();
    printer.line("Langkah berikutnya: `cigar sync`");
    Ok(0)
}

fn create_state_dir(cig_dir: &Path) -> Result<()> {
    fs::create_dir_all(cig_dir).map_err(|err| EngineError::io(cig_dir, err))?;
    restrict_permissions(cig_dir)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(0o700);
    fs::set_permissions(dir, permissions).map_err(|err| EngineError::io(dir, err))
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) -> Result<()> {
    // Windows ACLs are inherited from the parent; `.cig` stays outside the
    // repository manifest either way.
    Ok(())
}

fn write_config_atomic(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    if path.exists() {
        return Ok(());
    }
    let text = render_default_config(config);
    let temp = path.with_extension("toml.tmp");
    fs::write(&temp, text.as_bytes()).map_err(|err| EngineError::io(&temp, err))?;
    fs::rename(&temp, path).map_err(|err| EngineError::io(path, err))?;
    Ok(())
}

fn write_head_ref(cig_dir: &Path) -> Result<()> {
    let path = cig_dir.join("HEAD");
    if path.exists() {
        return Ok(());
    }
    atomic_write(&path, b"")
}

fn write_branch_marker(
    cig_dir: &Path,
    branch: &cigar_core::domain::BranchName,
    commit: Option<&str>,
) -> Result<()> {
    let path = refs_path(cig_dir, branch);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    atomic_write(&path, format!("{}\n", commit.unwrap_or("")).as_bytes())
}

/// Mirror path for an indexed branch.
#[must_use]
pub fn refs_path(cig_dir: &Path, branch: &cigar_core::domain::BranchName) -> PathBuf {
    let mut path = cig_dir.join("refs").join("heads");
    for segment in branch.as_str().split('/') {
        path.push(segment);
    }
    path
}

/// Writes a file by writing a sibling temp file and renaming it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes).map_err(|err| EngineError::io(&temp, err))?;
    fs::rename(&temp, path).map_err(|err| EngineError::io(path, err))?;
    Ok(())
}

fn exclude_state_dir(root: &Path, mode: GitMode) -> Result<()> {
    if mode == GitMode::Bare || mode == GitMode::FsOnly {
        return Ok(());
    }
    let git_dir = find_git_dir(root)?;
    let Some(git_dir) = git_dir else {
        return Ok(());
    };
    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir).map_err(|err| EngineError::io(&info_dir, err))?;
    let exclude = info_dir.join("exclude");
    let pattern = ".cig/";
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# CIGAR state\n");
    text.push_str(pattern);
    text.push('\n');
    fs::write(&exclude, text).map_err(|err| EngineError::io(&exclude, err))
}

fn find_git_dir(root: &Path) -> Result<Option<PathBuf>> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).map_err(|err| EngineError::io(&dot_git, err))?;
        if let Some(rest) = text.trim().strip_prefix("gitdir: ") {
            let path = PathBuf::from(rest.trim());
            let resolved = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            return Ok(Some(resolved));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_init_creates_lexical_store_without_model_files() {
        let root = tempfile::tempdir().unwrap();
        let cig_dir = root.path().join(".cig");
        let mut config = Config::default();
        config.storage.path = cig_dir.join("store.db");
        config.embedding.model_dir = root.path().join("missing-models");
        let context = Context {
            root: root.path().to_path_buf(),
            config_path: cig_dir.join("config.toml"),
            cig_dir: cig_dir.clone(),
            namespace: config.engine.namespace.clone(),
            config,
            branch_override: None,
            verbose: false,
            offline: false,
            strict: false,
            wait_secs: 0,
        };
        let args = InitArgs {
            git_mode: "auto".into(),
            embedding: None,
            model: None,
            dimensions: None,
            model_dir: None,
            no_model: false,
            force: false,
        };

        assert_eq!(run(&context, &args, &Printer::new(false, true)).unwrap(), 0);
        assert!(cig_dir.join("store.db").is_file());
        assert!(!root.path().join("missing-models").exists());
        let text = fs::read_to_string(cig_dir.join("config.toml")).unwrap();
        let parsed = Config::from_toml(&text, root.path(), &cig_dir).unwrap();
        assert_eq!(parsed.search.mode, "lexical-graph");
        assert_eq!(parsed.storage.driver, "sqlite");
    }
}
