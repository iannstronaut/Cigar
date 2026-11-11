// SPDX-License-Identifier: MPL-2.0

//! `gix`-backed source provider.

use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use gix::object::tree::diff::ChangeDetached;

use crate::domain::{
    BlobHash, BranchName, ChangeStatus, CommitId, Digest, RepoPath, SourceMode, SyncMode,
};
use crate::error::{EngineError, Result};
use crate::git::worktree::WorktreeScan;
use crate::hash::normalize_file_bytes;
use crate::traits::{ChangeSet, FileChange, Revision, SourceProvider};

/// A Git repository opened through `gix`.
pub struct GitSource {
    repo: gix::Repository,
    mode: SourceMode,
    max_file_bytes: u64,
}

impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource")
            .field("git_dir", &self.repo.git_dir())
            .field("workdir", &self.repo.workdir())
            .field("mode", &self.mode)
            .finish()
    }
}

impl GitSource {
    /// Discovers a repository from `start`, honouring `GIT_DIR`.
    pub fn discover(start: &Path, max_file_bytes: u64) -> Result<Self> {
        let repo = gix::discover(start).map_err(|err| EngineError::Git {
            message: format!(
                "tidak menemukan repository Git di {}: {err}",
                start.display()
            ),
        })?;
        let mode = if repo.is_bare() {
            SourceMode::GitBare
        } else {
            SourceMode::GitWorktree
        };
        Ok(Self {
            repo,
            mode,
            max_file_bytes,
        })
    }

    /// Opens a specific git directory (bare repositories).
    pub fn open_bare(git_dir: &Path, max_file_bytes: u64) -> Result<Self> {
        let repo = gix::open(git_dir).map_err(|err| EngineError::Git {
            message: format!("gagal membuka {}: {err}", git_dir.display()),
        })?;
        if !repo.is_bare() {
            return Err(EngineError::Git {
                message: format!("{} bukan bare repository", git_dir.display()),
            });
        }
        Ok(Self {
            repo,
            mode: SourceMode::GitBare,
            max_file_bytes,
        })
    }

    /// The repository root (worktree, or git dir for bare).
    #[must_use]
    pub fn root(&self) -> &Path {
        self.repo.workdir().unwrap_or_else(|| self.repo.git_dir())
    }

    /// The worktree root when this is a non-bare repository.
    #[must_use]
    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }

    /// Path to the git directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// True for linked worktrees (`.git` file pointing into `worktrees/<name>`).
    #[must_use]
    pub fn is_linked_worktree(&self) -> bool {
        self.repo
            .git_dir()
            .components()
            .any(|component| component.as_os_str() == "worktrees")
    }

    /// True when the repository uses SHA-256 object ids.
    #[must_use]
    pub fn is_sha256(&self) -> bool {
        matches!(self.repo.object_hash(), gix::hash::Kind::Sha256)
    }

    /// Scans the worktree for dirty mode.
    pub fn worktree_scan(&self, include_untracked: bool) -> Result<WorktreeScan> {
        let Some(workdir) = self.repo.workdir() else {
            return Err(EngineError::Git {
                message: "repository bare tidak punya worktree; gunakan mode commit".into(),
            });
        };
        WorktreeScan::scan(workdir, include_untracked, self.max_file_bytes)
    }

    /// Finds a commit object, mapping "missing object" to an actionable error.
    fn commit(&self, id: &CommitId) -> Result<gix::Commit<'_>> {
        let oid =
            gix::ObjectId::from_hex(id.as_str().as_bytes()).map_err(|err| EngineError::Git {
                message: format!("commit id tidak valid {}: {err}", id),
            })?;
        let object = self.repo.find_object(oid).map_err(|err| EngineError::Git {
            message: format!(
                "commit {} tidak tersedia (shallow/partial clone atau object hilang): {err}",
                id.short()
            ),
        })?;
        object.try_into_commit().map_err(|_| EngineError::Git {
            message: format!("{} bukan commit", id.short()),
        })
    }

    fn head_commit(&self) -> Result<Option<gix::Commit<'_>>> {
        let head = self.repo.head().map_err(|err| EngineError::Git {
            message: format!("gagal membaca HEAD: {err}"),
        })?;
        if head.is_unborn() {
            return Ok(None);
        }
        let mut head = head;
        head.try_peel_to_id()
            .map_err(|err| EngineError::Git {
                message: format!("gagal me-resolve HEAD: {err}"),
            })?
            .map(|id| {
                id.object()
                    .map_err(|err| EngineError::Git {
                        message: format!("object HEAD tidak tersedia: {err}"),
                    })?
                    .try_into_commit()
                    .map_err(|_| EngineError::Git {
                        message: "HEAD bukan commit".into(),
                    })
            })
            .transpose()
    }

    /// Reads the blob for `path` at `revision`, enforcing the size limit.
    pub fn read_blob(&self, path: &RepoPath, revision: Revision) -> Result<Vec<u8>> {
        let tree = match revision {
            Revision::Head => {
                let commit = self.head_commit()?.ok_or_else(|| EngineError::Git {
                    message: "HEAD belum punya commit".into(),
                })?;
                commit.tree().map_err(|err| EngineError::Git {
                    message: format!("gagal membaca tree HEAD: {err}"),
                })?
            }
            Revision::Commit(id) => {
                let commit = self.commit(&id)?;
                commit.tree().map_err(|err| EngineError::Git {
                    message: format!("gagal membaca tree {}: {err}", id.short()),
                })?
            }
            Revision::Worktree => {
                let workdir = self.repo.workdir().ok_or_else(|| EngineError::Git {
                    message: "repository bare tidak punya worktree".into(),
                })?;
                let absolute = workdir.join(path.as_str());
                return read_file_limited(&absolute, self.max_file_bytes);
            }
        };

        let entry = tree
            .lookup_entry_by_path(path.as_str())
            .map_err(|err| EngineError::Git {
                message: format!("gagal mencari {} di tree: {err}", path),
            })?;
        let Some(entry) = entry else {
            return Err(EngineError::NotFound {
                entity: "path".into(),
                id: path.to_string(),
            });
        };
        if entry.mode().is_tree() {
            return Err(EngineError::Git {
                message: format!("{} adalah direktori, bukan file", path),
            });
        }
        if entry.mode().is_link() {
            return Err(EngineError::Git {
                message: format!("{} adalah symlink; dilewati", path),
            });
        }
        if entry.mode().is_commit() {
            return Err(EngineError::Git {
                message: format!("{} adalah submodule; dilewati", path),
            });
        }
        let object = entry.object().map_err(|err| EngineError::Git {
            message: format!("blob {} tidak tersedia: {err}", path),
        })?;
        let blob = object.try_into_blob().map_err(|_| EngineError::Git {
            message: format!("{} bukan blob", path),
        })?;
        if blob.data.len() as u64 > self.max_file_bytes {
            return Err(EngineError::Git {
                message: format!(
                    "{} melebihi batas {} byte ({} byte)",
                    path,
                    self.max_file_bytes,
                    blob.data.len()
                ),
            });
        }
        Ok(blob.data.clone())
    }

    /// Reads every file in a tree into a manifest.
    pub fn list_tree(&self, commit: &CommitId) -> Result<Vec<RepoPath>> {
        let commit = self.commit(commit)?;
        let tree = commit.tree().map_err(|err| EngineError::Git {
            message: format!("gagal membaca tree: {err}"),
        })?;
        let mut out = Vec::new();
        collect_paths(&tree, "", &mut out)?;
        out.sort();
        Ok(out)
    }
}

fn collect_paths(tree: &gix::Tree<'_>, prefix: &str, out: &mut Vec<RepoPath>) -> Result<()> {
    for entry in tree.iter() {
        let entry = entry.map_err(|err| EngineError::Git {
            message: format!("gagal membaca tree entry: {err}"),
        })?;
        let name = entry.filename().to_str().map_err(|_| EngineError::Git {
            message: "path Git bukan UTF-8".into(),
        })?;
        let path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        match entry.mode().kind() {
            gix::object::tree::EntryKind::Tree => {
                let child = entry.object().map_err(|err| EngineError::Git {
                    message: format!("tree {path} tidak tersedia: {err}"),
                })?;
                let child = child.try_into_tree().map_err(|_| EngineError::Git {
                    message: format!("{path} bukan tree"),
                })?;
                collect_paths(&child, &path, out)?;
            }
            gix::object::tree::EntryKind::Blob | gix::object::tree::EntryKind::BlobExecutable => {
                if let Ok(repo_path) = RepoPath::parse(path) {
                    out.push(repo_path);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn read_file_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let metadata = std::fs::metadata(path).map_err(|err| EngineError::io(path, err))?;
    if !metadata.is_file() {
        return Err(EngineError::Git {
            message: format!("{} bukan file biasa", path.display()),
        });
    }
    if metadata.len() > max_bytes {
        return Err(EngineError::Git {
            message: format!(
                "{} melebihi batas {} byte ({} byte)",
                path.display(),
                max_bytes,
                metadata.len()
            ),
        });
    }
    std::fs::read(path).map_err(|err| EngineError::io(path, err))
}

impl SourceProvider for GitSource {
    fn mode(&self) -> SourceMode {
        self.mode
    }

    fn head(&self) -> Result<Option<CommitId>> {
        let Some(commit) = self.head_commit()? else {
            return Ok(None);
        };
        let hex = commit.id().detach().to_hex().to_string();
        Ok(Some(CommitId::parse(hex)?))
    }

    fn head_branch(&self) -> Result<Option<BranchName>> {
        let head = self.repo.head().map_err(|err| EngineError::Git {
            message: format!("gagal membaca HEAD: {err}"),
        })?;
        if head.is_detached() {
            return Ok(Some(BranchName::detached()));
        }
        if head.is_unborn() {
            return Ok(None);
        }
        let Some(name) = head.referent_name() else {
            return Ok(None);
        };
        let full = name.as_bstr().to_str().map_err(|_| EngineError::Git {
            message: "nama ref bukan UTF-8".into(),
        })?;
        let short = full.strip_prefix("refs/heads/").unwrap_or(full);
        Ok(Some(BranchName::parse(short.to_owned())?))
    }

    fn changes(&self, base: Option<&CommitId>, mode: SyncMode) -> Result<ChangeSet> {
        if mode != SyncMode::Commit {
            return Err(EngineError::invalid(
                "perubahan worktree dihitung oleh pemindai filesystem, bukan gix",
            ));
        }
        let head = self.head()?;
        let mut set = ChangeSet {
            base: base.cloned(),
            head: head.clone(),
            mode,
            ..ChangeSet::default()
        };
        let Some(head) = head else {
            return Ok(set);
        };
        let new_tree = self.commit(&head)?.tree().map_err(|err| EngineError::Git {
            message: format!("gagal membaca tree HEAD: {err}"),
        })?;

        let Some(base) = base else {
            // No base: every file in HEAD is an addition.
            let mut paths = Vec::new();
            collect_paths(&new_tree, "", &mut paths)?;
            for path in paths {
                set.changes.push(FileChange {
                    path,
                    old_path: None,
                    status: ChangeStatus::Added,
                    size: None,
                });
            }
            return Ok(set);
        };

        let old_tree = self.commit(base)?.tree().map_err(|err| EngineError::Git {
            message: format!(
                "tree {} tidak tersedia (shallow/partial clone?): {err}",
                base.short()
            ),
        })?;

        // Rewrite tracking is disabled: CIGAR treats renames as delete + add,
        // which keeps branch manifests correct without reading blobs twice.
        let mut options = gix::diff::Options::default();
        options.track_path();
        options.track_rewrites(None);

        let changes = self
            .repo
            .diff_tree_to_tree(Some(&old_tree), Some(&new_tree), options)
            .map_err(|err| EngineError::Git {
                message: format!("gagal menghitung diff tree: {err}"),
            })?;

        for change in changes {
            push_change(change, &mut set.changes);
        }
        Ok(set)
    }

    fn read(&self, path: &RepoPath, revision: Revision) -> Result<Vec<u8>> {
        self.read_blob(path, revision)
    }

    fn fingerprint(&self) -> Result<Option<Digest>> {
        let Some(workdir) = self.repo.workdir() else {
            return Ok(None);
        };
        let scan = WorktreeScan::scan(workdir, true, self.max_file_bytes)?;
        let entries: Vec<(RepoPath, Digest)> = scan
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.path.clone(),
                    Digest::from_hex_unchecked(entry.blob_hash.as_str().to_owned()),
                )
            })
            .collect();
        Ok(Some(crate::hash::manifest_fingerprint(&entries)))
    }

    fn workdir(&self) -> Option<PathBuf> {
        self.repo.workdir().map(Path::to_path_buf)
    }

    fn list(&self, revision: Revision) -> Result<Vec<RepoPath>> {
        match revision {
            Revision::Commit(id) => self.list_tree(&id),
            Revision::Head => match self.head()? {
                Some(id) => self.list_tree(&id),
                None => Ok(Vec::new()),
            },
            Revision::Worktree => {
                let Some(workdir) = self.repo.workdir() else {
                    return Ok(Vec::new());
                };
                let scan = WorktreeScan::scan(workdir, true, self.max_file_bytes)?;
                Ok(scan.entries.into_iter().map(|file| file.path).collect())
            }
        }
    }
}

fn push_change(change: ChangeDetached, out: &mut Vec<FileChange>) {
    match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => {
            if entry_mode.is_tree() || entry_mode.is_link() || entry_mode.is_commit() {
                return;
            }
            if let Some(path) = bstr_to_path(location.as_ref()) {
                let _ = id;
                out.push(FileChange {
                    path,
                    old_path: None,
                    status: ChangeStatus::Added,
                    size: None,
                });
            }
        }
        ChangeDetached::Deletion {
            location,
            entry_mode,
            ..
        } => {
            if entry_mode.is_tree() || entry_mode.is_link() || entry_mode.is_commit() {
                return;
            }
            if let Some(path) = bstr_to_path(location.as_ref()) {
                out.push(FileChange {
                    path,
                    old_path: None,
                    status: ChangeStatus::Deleted,
                    size: None,
                });
            }
        }
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            if entry_mode.is_tree() || entry_mode.is_link() || entry_mode.is_commit() {
                // A file turning into a symlink/submodule is a deletion for us.
                if previous_entry_mode.is_blob() {
                    if let Some(path) = bstr_to_path(location.as_ref()) {
                        out.push(FileChange {
                            path,
                            old_path: None,
                            status: ChangeStatus::Deleted,
                            size: None,
                        });
                    }
                }
                return;
            }
            if let Some(path) = bstr_to_path(location.as_ref()) {
                out.push(FileChange {
                    path,
                    old_path: None,
                    status: if previous_entry_mode.is_blob() {
                        ChangeStatus::Modified
                    } else {
                        // mode-only change from tree/submodule to blob
                        ChangeStatus::Added
                    },
                    size: None,
                });
            }
        }
        ChangeDetached::Rewrite {
            source_location,
            location,
            entry_mode,
            ..
        } => {
            if entry_mode.is_tree() || entry_mode.is_link() || entry_mode.is_commit() {
                return;
            }
            let Some(new_path) = bstr_to_path(location.as_ref()) else {
                return;
            };
            let old_path = bstr_to_path(source_location.as_ref());
            out.push(FileChange {
                path: new_path,
                old_path: old_path.clone(),
                status: ChangeStatus::Renamed,
                size: None,
            });
            // A rename also removes the old path from the branch manifest.
        }
    }
}

fn bstr_to_path(value: &[u8]) -> Option<RepoPath> {
    let text = value.to_str().ok()?;
    RepoPath::parse(text.to_owned()).ok()
}

/// Normalizes and hashes file bytes, used by both source adapters.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> BlobHash {
    BlobHash::from_hex_unchecked(
        crate::hash::blob_hash(&normalize_file_bytes(bytes))
            .as_str()
            .to_owned(),
    )
}

/// Convenience wrapper used by tests and callers that need a git dir path.
#[must_use]
pub fn git_dir_of(path: &Path) -> Option<PathBuf> {
    gix::discover(path)
        .ok()
        .map(|repo| repo.git_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceMode;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        run_git(&root, &["init", "-q", "-b", "main"]);
        run_git(&root, &["config", "user.email", "test@example.com"]);
        run_git(&root, &["config", "user.name", "Test"]);
        (dir, root)
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git tersedia untuk fixture");
        assert!(status.success(), "git {args:?} gagal");
    }

    fn commit_all(root: &Path, message: &str) -> String {
        run_git(root, &["add", "-A"]);
        run_git(root, &["commit", "-q", "-m", message]);
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn head_and_head_branch() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let head = commit_all(&root, "init");
        let source = GitSource::discover(&root, 1_048_576).unwrap();
        assert_eq!(source.mode(), SourceMode::GitWorktree);
        assert_eq!(source.head().unwrap().unwrap().as_str(), head.as_str());
        assert_eq!(source.head_branch().unwrap().unwrap().as_str(), "main");
    }

    #[test]
    fn tree_diff_reports_add_modify_delete() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(root.join("gone.rs"), "fn gone() {}\n").unwrap();
        let base = commit_all(&root, "base");
        std::fs::write(root.join("keep.rs"), "fn keep() { changed(); }\n").unwrap();
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::write(root.join("new.rs"), "fn new() {}\n").unwrap();
        commit_all(&root, "change");

        let source = GitSource::discover(&root, 1_048_576).unwrap();
        let base = CommitId::parse(base).unwrap();
        let changes = source.changes(Some(&base), SyncMode::Commit).unwrap();
        let mut statuses: Vec<(String, ChangeStatus)> = changes
            .changes
            .iter()
            .map(|change| (change.path.to_string(), change.status))
            .collect();
        statuses.sort();
        assert_eq!(
            statuses,
            vec![
                ("gone.rs".to_owned(), ChangeStatus::Deleted),
                ("keep.rs".to_owned(), ChangeStatus::Modified),
                ("new.rs".to_owned(), ChangeStatus::Added),
            ]
        );
    }

    #[test]
    fn no_base_yields_every_file_added() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();
        commit_all(&root, "init");
        let source = GitSource::discover(&root, 1_048_576).unwrap();
        let changes = source.changes(None, SyncMode::Commit).unwrap();
        assert_eq!(changes.changes.len(), 2);
        assert!(changes
            .changes
            .iter()
            .all(|change| change.status == ChangeStatus::Added));
    }

    #[test]
    fn read_blob_from_head_and_commit() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let head = commit_all(&root, "init");
        let source = GitSource::discover(&root, 1_048_576).unwrap();
        let path = RepoPath::parse("a.rs").unwrap();
        let bytes = source.read(&path, Revision::Head).unwrap();
        assert_eq!(normalize_file_bytes(&bytes), b"fn a() {}\n");
        let base = CommitId::parse(head).unwrap();
        let bytes = source.read(&path, Revision::Commit(base)).unwrap();
        assert_eq!(normalize_file_bytes(&bytes), b"fn a() {}\n");
    }

    #[test]
    fn symlink_entries_are_not_followed() {
        let (_dir, root) = fixture();
        std::fs::write(root.join("real.rs"), "fn r() {}\n").unwrap();
        commit_all(&root, "init");
        // Force a symlink entry into the index/tree via plumbing, which works
        // without OS symlink privileges. The blob must exist first.
        std::fs::write(root.join("link-target"), "real.rs").unwrap();
        let output = std::process::Command::new("git")
            .args(["hash-object", "-w", "link-target"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let oid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let cacheinfo = format!("120000,{oid},link.rs");
        run_git(&root, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
        run_git(&root, &["commit", "-q", "-m", "symlink"]);

        let source = GitSource::discover(&root, 1_048_576).unwrap();
        let link = RepoPath::parse("link.rs").unwrap();
        let error = source.read(&link, Revision::Head).unwrap_err();
        assert!(
            error.to_string().contains("symlink"),
            "symlink harus dilewati, dapat: {error}"
        );
    }
}
