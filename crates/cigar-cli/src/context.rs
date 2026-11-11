//! Runtime environment discovery: repository root, CIGAR directory, config load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cigar_core::config::Config;
use cigar_core::domain::{BranchName, Namespace};
use cigar_core::error::{EngineError, Result};

use crate::cli::GlobalArgs;

/// Resolved paths and configuration for one invocation.
#[derive(Debug)]
pub struct Context {
    pub root: PathBuf,
    pub cig_dir: PathBuf,
    pub config_path: PathBuf,
    pub config: Config,
    pub namespace: Namespace,
    pub branch_override: Option<BranchName>,
    #[allow(dead_code, reason = "verbose logging lands with sync progress")]
    pub verbose: bool,
    pub offline: bool,
    #[allow(dead_code, reason = "strict parse promotion lands with sync")]
    pub strict: bool,
    // Consumed by `sync` (Fase 2): lock wait budget and per-branch mode.
    #[allow(dead_code, reason = "lock wait budget is used by sync in Fase 2")]
    pub wait_secs: u64,
}

impl Context {
    /// Resolves paths without requiring an existing `.cig` directory.
    pub fn discover(global: &GlobalArgs) -> Result<Self> {
        let cwd = match &global.cwd {
            Some(path) => absolute(path),
            None => std::env::current_dir().map_err(|err| EngineError::io(".", err))?,
        };
        let root = find_repo_root(&cwd);
        let cig_dir = match &global.cig_dir {
            Some(path) => absolute_from(path, &cwd),
            None => find_cig_dir(&cwd).unwrap_or_else(|| root.join(".cig")),
        };
        let config_path = match &global.config {
            Some(path) => absolute_from(path, &cwd),
            None => cig_dir.join("config.toml"),
        };

        let config = if config_path.exists() {
            let text = std::fs::read_to_string(&config_path)
                .map_err(|err| EngineError::io(&config_path, err))?;
            Config::from_toml(&text, &root, &cig_dir)?
        } else {
            let mut config = Config::default();
            config.storage.path = cig_dir.join("store.db");
            config.validate()?;
            config
        };

        let env = std::env::vars()
            .filter(|(key, _)| key.starts_with("CIGAR_"))
            .collect::<BTreeMap<_, _>>();
        let mut config = config;
        config.apply_env(&env, &root, &cig_dir)?;

        // CLI overrides win over everything.
        if let Some(ns) = &global.namespace {
            config.engine.namespace = Namespace::parse(ns.clone())?;
        }

        let namespace = config.engine.namespace.clone();
        let branch_override = global
            .branch
            .as_deref()
            .map(BranchName::parse)
            .transpose()?;

        Ok(Self {
            root,
            cig_dir,
            config_path,
            config,
            namespace,
            branch_override,
            verbose: global.verbose,
            offline: global.offline,
            strict: global.strict,
            wait_secs: global.wait,
        })
    }

    /// Clones this context with a different repository root, keeping every
    /// resolved setting.
    #[must_use]
    pub fn clone_for(&self, root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            cig_dir: self.cig_dir.clone(),
            config_path: self.config_path.clone(),
            config: self.config.clone(),
            namespace: self.namespace.clone(),
            branch_override: self.branch_override.clone(),
            verbose: self.verbose,
            offline: self.offline,
            strict: self.strict,
            wait_secs: self.wait_secs,
        }
    }

    /// Fails with `NotInitialized` when the state directory is missing.
    pub fn require_initialized(&self) -> Result<()> {
        if !self.cig_dir.join("store.db").exists() {
            return Err(EngineError::NotInitialized {
                path: self.cig_dir.display().to_string(),
            });
        }
        Ok(())
    }

    /// Store path from resolved config.
    #[must_use]
    pub fn store_path(&self) -> PathBuf {
        self.config.storage.path.clone()
    }

    /// Lock file path.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.cig_dir.join("lock")
    }

    /// Branch used when `--branch` is absent; Git is authoritative.
    pub fn resolve_branch(&self) -> Result<BranchName> {
        if let Some(branch) = &self.branch_override {
            return Ok(branch.clone());
        }
        if let Some(branch) = self.git_head_branch()? {
            return Ok(branch);
        }
        BranchName::parse("main")
    }

    /// Reads the current branch from Git without opening a repository handle
    /// (works for worktrees, and returns `@detached` for a detached HEAD).
    pub fn git_head_branch(&self) -> Result<Option<BranchName>> {
        if !self.root.join(".git").exists() {
            return Ok(None);
        }
        let output = std::process::Command::new("git")
            .args(["symbolic-ref", "--short", "-q", "HEAD"])
            .current_dir(&self.root)
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                if name.is_empty() {
                    return Ok(Some(BranchName::detached()));
                }
                Ok(Some(BranchName::parse(name)?))
            }
            Ok(_) => {
                // A detached HEAD makes `symbolic-ref` exit non-zero.
                if self.root.join(".git").exists() {
                    Ok(Some(BranchName::detached()))
                } else {
                    Ok(None)
                }
            }
            Err(_) => Ok(None),
        }
    }

    /// Human label for the repo root.
    #[must_use]
    pub fn root_display(&self) -> String {
        self.root.display().to_string()
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize(path);
    }
    std::env::current_dir()
        .map(|cwd| normalize(&cwd.join(path)))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn absolute_from(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        return normalize(path);
    }
    normalize(&base.join(path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Walks up from `start` looking for a `.git` entry or a `.cig` directory.
#[must_use]
pub fn find_repo_root(start: &Path) -> PathBuf {
    let mut current = normalize(start);
    loop {
        if current.join(".git").exists() || current.join(".cig").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return normalize(start),
        }
    }
}

/// Walks up looking for an existing `.cig` directory.
#[must_use]
pub fn find_cig_dir(start: &Path) -> Option<PathBuf> {
    let mut current = normalize(start);
    loop {
        let candidate = current.join(".cig");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = current.parent()?.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn finds_git_root_from_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_repo_root(&nested), dir.path().to_path_buf());
    }

    #[test]
    fn finds_cig_dir_from_subdirectory() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".cig")).unwrap();
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_cig_dir(&nested).unwrap(), dir.path().join(".cig"));
    }
}
