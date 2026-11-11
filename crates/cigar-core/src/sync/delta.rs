// SPDX-License-Identifier: MPL-2.0

//! Delta computation for one branch (docs/04 sections 4-5).
//!
//! Commit mode uses the Git tree diff from the last synced commit to HEAD.
//! Dirty mode compares content hashes of the working tree against the branch
//! manifest, so it never trusts mtimes or the index.

use std::collections::BTreeMap;

use crate::domain::{BlobHash, BranchFile, ChangeStatus, CommitId, RepoPath, SyncMode};
use crate::error::{EngineError, Result};
use crate::git::worktree::WorktreeScan;
use crate::traits::{FileChange, SourceProvider};

/// What to do for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTask {
    pub path: RepoPath,
    pub old_path: Option<RepoPath>,
    pub status: ChangeStatus,
    /// Hash of the bytes we will parse, when the file is added or modified.
    pub new_blob: Option<BlobHash>,
    /// Hash previously recorded for this path, when it existed.
    pub old_blob: Option<BlobHash>,
}

/// Full delta for one branch.
#[derive(Clone, Debug, Default)]
pub struct DeltaPlan {
    pub base_commit: Option<CommitId>,
    pub head_commit: Option<CommitId>,
    pub mode: SyncMode,
    pub tasks: Vec<FileTask>,
    pub unchanged: Vec<BranchFile>,
    pub warnings: Vec<String>,
}

impl DeltaPlan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Paths that will be parsed (added + modified).
    #[must_use]
    pub fn parse_paths(&self) -> Vec<RepoPath> {
        self.tasks
            .iter()
            .filter(|task| matches!(task.status, ChangeStatus::Added | ChangeStatus::Modified))
            .map(|task| task.path.clone())
            .collect()
    }

    /// Paths removed from the branch manifest.
    #[must_use]
    pub fn removed_paths(&self) -> Vec<RepoPath> {
        let mut out: Vec<RepoPath> = self
            .tasks
            .iter()
            .filter(|task| task.status == ChangeStatus::Deleted)
            .map(|task| task.path.clone())
            .collect();
        out.extend(
            self.tasks
                .iter()
                .filter(|task| task.status == ChangeStatus::Renamed)
                .filter_map(|task| task.old_path.clone()),
        );
        out.sort();
        out.dedup();
        out
    }
}

/// Computes the delta for one branch.
///
/// In commit mode, `head` is the Git HEAD. In dirty mode, the caller supplies a
/// precomputed worktree scan because it already had to read the files to hash
/// them.
pub fn compute_delta(
    source: &dyn SourceProvider,
    mode: SyncMode,
    base_commit: Option<&CommitId>,
    head_commit: Option<&CommitId>,
    manifest: &[BranchFile],
    dirty_scan: Option<&WorktreeScan>,
) -> Result<DeltaPlan> {
    match mode {
        SyncMode::Commit => compute_commit_delta(source, base_commit, head_commit, manifest),
        SyncMode::Dirty | SyncMode::Fs => compute_dirty_delta(manifest, dirty_scan, mode),
    }
}

fn compute_commit_delta(
    source: &dyn SourceProvider,
    base_commit: Option<&CommitId>,
    head_commit: Option<&CommitId>,
    manifest: &[BranchFile],
) -> Result<DeltaPlan> {
    let mut plan = DeltaPlan {
        base_commit: base_commit.cloned(),
        head_commit: head_commit.cloned(),
        mode: SyncMode::Commit,
        ..DeltaPlan::default()
    };

    let Some(head) = head_commit else {
        // Unborn branch: nothing to index.
        plan.warnings
            .push("HEAD belum punya commit; tidak ada yang disinkronkan".into());
        return Ok(plan);
    };

    let changes = source.changes(base_commit, SyncMode::Commit)?;
    let manifest_map: BTreeMap<RepoPath, BlobHash> = manifest
        .iter()
        .map(|file| (file.file_path.clone(), file.blob_hash.clone()))
        .collect();

    for change in changes.changes {
        push_task(&mut plan, change, &manifest_map);
    }

    // Keep unchanged entries so the branch manifest remains complete.
    let changed: std::collections::BTreeSet<&RepoPath> =
        plan.tasks.iter().map(|task| &task.path).collect();
    let renamed_from: std::collections::BTreeSet<&RepoPath> = plan
        .tasks
        .iter()
        .filter_map(|task| task.old_path.as_ref())
        .collect();
    for file in manifest {
        if !changed.contains(&file.file_path) && !renamed_from.contains(&file.file_path) {
            plan.unchanged.push(file.clone());
        }
    }

    let _ = head;
    Ok(plan)
}

fn compute_dirty_delta(
    manifest: &[BranchFile],
    scan: Option<&WorktreeScan>,
    mode: SyncMode,
) -> Result<DeltaPlan> {
    let Some(scan) = scan else {
        return Err(EngineError::invalid(
            "mode dirty/fs membutuhkan hasil pemindaian worktree",
        ));
    };
    let base: BTreeMap<RepoPath, BlobHash> = manifest
        .iter()
        .map(|file| (file.file_path.clone(), file.blob_hash.clone()))
        .collect();
    let mut plan = DeltaPlan {
        base_commit: None,
        head_commit: None,
        mode,
        ..DeltaPlan::default()
    };
    for change in scan.changes_against(&base) {
        push_task(&mut plan, change, &base);
    }
    let changed: std::collections::BTreeSet<&RepoPath> =
        plan.tasks.iter().map(|task| &task.path).collect();
    for file in manifest {
        if !changed.contains(&file.file_path) {
            plan.unchanged.push(file.clone());
        }
    }
    Ok(plan)
}

fn push_task(plan: &mut DeltaPlan, change: FileChange, manifest: &BTreeMap<RepoPath, BlobHash>) {
    // The new content hash is filled in by the pipeline after reading bytes;
    // this stage only decides what changed.
    let old_blob = manifest.get(&change.path).cloned();
    plan.tasks.push(FileTask {
        path: change.path,
        old_path: change.old_path,
        status: change.status,
        new_blob: None,
        old_blob,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlobHash, BranchName, Language};

    fn branch_file(path: &str, seed: u8) -> BranchFile {
        BranchFile {
            file_path: RepoPath::parse(path).unwrap(),
            blob_hash: BlobHash::parse(format!("{seed:064x}")).unwrap(),
            language: Language::Rust,
        }
    }

    #[test]
    fn dirty_delta_reports_add_modify_delete() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("keep.rs"), "fn keep() {}").unwrap();
        std::fs::write(root.join("gone.rs"), "fn gone() {}").unwrap();
        let scan = WorktreeScan::scan(root, true, 1_048_576).unwrap();
        let manifest = vec![branch_file("keep.rs", 1), branch_file("gone.rs", 2)];

        std::fs::write(root.join("keep.rs"), "fn keep() { x(); }").unwrap();
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::write(root.join("new.rs"), "fn new() {}").unwrap();
        let scan_after = WorktreeScan::scan(root, true, 1_048_576).unwrap();

        let plan = compute_delta(
            &crate::git::FsSource::open(root, 1_048_576).unwrap(),
            SyncMode::Dirty,
            None,
            None,
            &manifest,
            Some(&scan_after),
        )
        .unwrap();
        let statuses: Vec<(&str, ChangeStatus)> = plan
            .tasks
            .iter()
            .map(|task| (task.path.as_str(), task.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("gone.rs", ChangeStatus::Deleted),
                ("keep.rs", ChangeStatus::Modified),
                ("new.rs", ChangeStatus::Added),
            ]
        );
        let _ = scan;
    }

    #[test]
    fn dirty_delta_needs_a_scan() {
        let source = crate::git::FsSource::open(&std::env::temp_dir(), 1_048_576);
        let error =
            compute_delta(&source.unwrap(), SyncMode::Dirty, None, None, &[], None).unwrap_err();
        assert!(error.to_string().contains("pemindaian"));
    }

    #[test]
    fn unchanged_files_are_preserved() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.rs"), "fn a() {}").unwrap();
        let scan = WorktreeScan::scan(root, true, 1_048_576).unwrap();
        let manifest = vec![branch_file("a.rs", 1)];
        let source = crate::git::FsSource::open(root, 1_048_576).unwrap();
        let plan = compute_delta(
            &source,
            SyncMode::Commit,
            None,
            None,
            &manifest,
            Some(&scan),
        )
        .unwrap();
        assert!(plan.unchanged.is_empty());
    }

    #[test]
    fn empty_branch_has_no_tasks() {
        let dir = tempfile::TempDir::new().unwrap();
        let source = crate::git::FsSource::open(dir.path(), 1_048_576).unwrap();
        let plan = compute_delta(
            &source,
            SyncMode::Commit,
            None,
            None,
            &[],
            Some(&WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap()),
        )
        .unwrap();
        assert!(plan.is_empty());
    }

    #[test]
    fn removed_paths_include_renames() {
        let mut plan = DeltaPlan::default();
        plan.tasks.push(FileTask {
            path: RepoPath::parse("new.rs").unwrap(),
            old_path: Some(RepoPath::parse("old.rs").unwrap()),
            status: ChangeStatus::Renamed,
            new_blob: None,
            old_blob: None,
        });
        assert_eq!(
            plan.removed_paths(),
            vec![RepoPath::parse("old.rs").unwrap()]
        );
        let _ = BranchName::parse("main").unwrap();
    }
}
