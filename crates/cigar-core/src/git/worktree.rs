// SPDX-License-Identifier: MPL-2.0

//! Filesystem scanning for dirty mode and fs-only sources.
//!
//! Dirty mode never trusts mtimes: every candidate file is read and hashed with
//! the same normalization as commit mode, so `incremental == full` holds across
//! both paths (docs/04 section 5, docs/21 section 10).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::domain::{BlobHash, Digest, Language, RepoPath, SourceMode, SyncMode};
use crate::error::{EngineError, Result};
use crate::hash::{blob_hash, normalize_file_bytes};
use crate::traits::{ChangeSet, FileChange, Revision, SourceProvider};

/// One scanned file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: RepoPath,
    pub blob_hash: BlobHash,
    pub language: Option<Language>,
    pub size_bytes: u64,
    /// Normalized source text, when the file is valid UTF-8.
    pub text: Option<String>,
}

/// Result of scanning a working tree.
#[derive(Clone, Debug, Default)]
pub struct WorktreeScan {
    pub root: PathBuf,
    pub entries: Vec<ScannedFile>,
    pub skipped: Vec<SkippedFile>,
}

/// A file excluded from the index with a reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

/// Why a file was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    TooLarge,
    Binary,
    NotUtf8,
    Symlink,
    Unsupported,
    ReadError,
}

impl SkipReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "terlalu besar",
            Self::Binary => "biner",
            Self::NotUtf8 => "bukan UTF-8",
            Self::Symlink => "symlink",
            Self::Unsupported => "tidak didukung",
            Self::ReadError => "gagal dibaca",
        }
    }
}

impl WorktreeScan {
    /// Scans `root`, applying default exclusions plus `extra_excludes`.
    pub fn scan(root: &Path, include_untracked: bool, max_file_bytes: u64) -> Result<Self> {
        Self::scan_with(root, include_untracked, max_file_bytes, &[])
    }

    /// Scans with additional repository-relative exclusions.
    pub fn scan_with(
        root: &Path,
        _include_untracked: bool,
        max_file_bytes: u64,
        extra_excludes: &[RepoPath],
    ) -> Result<Self> {
        if !root.is_dir() {
            return Err(EngineError::Git {
                message: format!("{} bukan direktori", root.display()),
            });
        }
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(true)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(false)
            .parents(false)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_path(|a, b| a.cmp(b));

        // Internal state and build output never belong in the index. `.cig` is
        // added to `.git/info/exclude`, but fs-only repos have no Git config.
        let mut excludes: Vec<String> = vec![
            ".git".into(),
            ".cig".into(),
            "target".into(),
            "node_modules".into(),
            "vendor".into(),
            ".next".into(),
            "dist".into(),
            "__pycache__".into(),
            ".venv".into(),
        ];
        excludes.extend(extra_excludes.iter().map(|path| path.to_string()));
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        for exclude in &excludes {
            overrides.add(&format!("!{exclude}/**")).map_err(|err| {
                EngineError::config(format!("exclude tidak valid {exclude}: {err}"))
            })?;
        }
        let overrides = overrides
            .build()
            .map_err(|err| EngineError::config(format!("gagal membangun exclude: {err}")))?;
        builder.overrides(overrides);

        let mut entries = Vec::new();
        let mut skipped = Vec::new();

        for result in builder.build() {
            let entry = match result {
                Ok(entry) => entry,
                Err(err) => {
                    skipped.push(SkippedFile {
                        path: err.to_string(),
                        reason: SkipReason::ReadError,
                    });
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let absolute = entry.path();
            let relative = match absolute.strip_prefix(root) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            let Some(path) = path_from_os(relative) else {
                skipped.push(SkippedFile {
                    path: absolute.display().to_string(),
                    reason: SkipReason::NotUtf8,
                });
                continue;
            };
            match scan_file(absolute, &path, max_file_bytes) {
                Ok(Some(file)) => entries.push(file),
                Ok(None) => skipped.push(SkippedFile {
                    path: path.to_string(),
                    reason: SkipReason::Unsupported,
                }),
                Err(reason) => skipped.push(SkippedFile {
                    path: path.to_string(),
                    reason,
                }),
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            root: root.to_path_buf(),
            entries,
            skipped,
        })
    }

    /// Map of path to content hash, used for delta comparison.
    #[must_use]
    pub fn manifest(&self) -> BTreeMap<RepoPath, BlobHash> {
        self.entries
            .iter()
            .map(|file| (file.path.clone(), file.blob_hash.clone()))
            .collect()
    }

    /// Fingerprint over the manifest, matching `GitSource::fingerprint`.
    #[must_use]
    pub fn fingerprint(&self) -> Digest {
        let mut entries: Vec<(RepoPath, Digest)> = self
            .entries
            .iter()
            .map(|file| {
                (
                    file.path.clone(),
                    Digest::from_hex_unchecked(file.blob_hash.as_str().to_owned()),
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        crate::hash::manifest_fingerprint(&entries)
    }

    /// Builds a change set relative to a previous manifest.
    #[must_use]
    pub fn changes_against(&self, base: &BTreeMap<RepoPath, BlobHash>) -> Vec<FileChange> {
        let mut changes = Vec::new();
        for file in &self.entries {
            match base.get(&file.path) {
                Some(existing) if existing == &file.blob_hash => {}
                Some(_) => changes.push(FileChange {
                    path: file.path.clone(),
                    old_path: None,
                    status: crate::domain::ChangeStatus::Modified,
                    size: Some(file.size_bytes),
                }),
                None => changes.push(FileChange {
                    path: file.path.clone(),
                    old_path: None,
                    status: crate::domain::ChangeStatus::Added,
                    size: Some(file.size_bytes),
                }),
            }
        }
        let current: std::collections::BTreeSet<&RepoPath> =
            self.entries.iter().map(|file| &file.path).collect();
        for path in base.keys() {
            if !current.contains(path) {
                changes.push(FileChange {
                    path: path.clone(),
                    old_path: None,
                    status: crate::domain::ChangeStatus::Deleted,
                    size: None,
                });
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        changes
    }

    /// Number of skipped files by reason.
    #[must_use]
    pub fn skipped_summary(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for skipped in &self.skipped {
            *out.entry(skipped.reason.as_str()).or_insert(0) += 1;
        }
        out
    }
}

fn path_from_os(path: &Path) -> Option<RepoPath> {
    let text = path.to_str()?;
    RepoPath::parse(text.replace('\\', "/")).ok()
}

fn scan_file(
    absolute: &Path,
    path: &RepoPath,
    max_file_bytes: u64,
) -> std::result::Result<Option<ScannedFile>, SkipReason> {
    let metadata = std::fs::symlink_metadata(absolute).map_err(|_| SkipReason::ReadError)?;
    if metadata.file_type().is_symlink() {
        return Err(SkipReason::Symlink);
    }
    if !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > max_file_bytes {
        return Err(SkipReason::TooLarge);
    }
    let language = Language::from_path(path);
    if language.is_none() {
        return Ok(None);
    }
    let bytes = std::fs::read(absolute).map_err(|_| SkipReason::ReadError)?;
    if bytes.iter().take(8000).any(|byte| *byte == 0) {
        return Err(SkipReason::Binary);
    }
    let normalized = normalize_file_bytes(&bytes);
    let text = String::from_utf8(normalized.clone()).map_err(|_| SkipReason::NotUtf8)?;
    Ok(Some(ScannedFile {
        path: path.clone(),
        blob_hash: BlobHash::from_hex_unchecked(blob_hash(&normalized).as_str().to_owned()),
        language,
        size_bytes: bytes.len() as u64,
        text: Some(text),
    }))
}

/// Filesystem-only source provider (no Git at all).
#[derive(Debug)]
pub struct FsSource {
    root: PathBuf,
    max_file_bytes: u64,
    scan: WorktreeScan,
}

impl FsSource {
    /// Scans `root` once and keeps the result for this provider instance.
    pub fn open(root: &Path, max_file_bytes: u64) -> Result<Self> {
        let scan = WorktreeScan::scan(root, true, max_file_bytes)?;
        Ok(Self {
            root: root.to_path_buf(),
            max_file_bytes,
            scan,
        })
    }

    /// Access to the initial scan.
    #[must_use]
    pub fn scan(&self) -> &WorktreeScan {
        &self.scan
    }

    /// Recomputes the scan (used by `sync`).
    pub fn rescan(&mut self) -> Result<()> {
        self.scan = WorktreeScan::scan(&self.root, true, self.max_file_bytes)?;
        Ok(())
    }
}

impl SourceProvider for FsSource {
    fn mode(&self) -> SourceMode {
        SourceMode::FsOnly
    }

    fn head(&self) -> Result<Option<crate::domain::CommitId>> {
        Ok(None)
    }

    fn head_branch(&self) -> Result<Option<crate::domain::BranchName>> {
        Ok(Some(crate::domain::BranchName::parse("main")?))
    }

    fn changes(
        &self,
        _base: Option<&crate::domain::CommitId>,
        mode: SyncMode,
    ) -> Result<ChangeSet> {
        let mut set = ChangeSet {
            base: None,
            head: None,
            mode,
            ..ChangeSet::default()
        };
        for file in &self.scan.entries {
            set.changes.push(FileChange {
                path: file.path.clone(),
                old_path: None,
                status: crate::domain::ChangeStatus::Added,
                size: Some(file.size_bytes),
            });
        }
        Ok(set)
    }

    fn read(&self, path: &RepoPath, _revision: Revision) -> Result<Vec<u8>> {
        let absolute = self.root.join(path.as_str());
        let metadata =
            std::fs::metadata(&absolute).map_err(|err| EngineError::io(&absolute, err))?;
        if metadata.len() > self.max_file_bytes {
            return Err(EngineError::Git {
                message: format!("{} melebihi batas ukuran", path),
            });
        }
        std::fs::read(&absolute).map_err(|err| EngineError::io(&absolute, err))
    }

    fn fingerprint(&self) -> Result<Option<Digest>> {
        Ok(Some(self.scan.fingerprint()))
    }

    fn workdir(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn list(&self, _revision: Revision) -> Result<Vec<RepoPath>> {
        Ok(self
            .scan
            .entries
            .iter()
            .map(|file| file.path.clone())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(dir: &Path, relative: &str, content: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn scans_supported_files_and_skips_others() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/a.rs", "fn a() {}");
        write(dir.path(), "README.md", "# docs");
        write(dir.path(), "target/debug/x.rs", "fn x() {}");
        write(dir.path(), ".cig/store.db", "binary");
        let scan = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        let paths: Vec<&str> = scan.entries.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs"]);
    }

    #[test]
    fn skips_large_and_binary_files() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "big.rs", &"x".repeat(200));
        std::fs::write(dir.path().join("bin.rs"), [0_u8, 1, 2, 3]).unwrap();
        let scan = WorktreeScan::scan(dir.path(), true, 50).unwrap();
        assert!(scan.entries.is_empty());
        assert!(scan
            .skipped
            .iter()
            .any(|s| s.reason == SkipReason::TooLarge));
    }

    #[test]
    fn hash_is_line_ending_independent() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.rs", "fn a() {}\r\n");
        let scan = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        let normalized = normalize_file_bytes(b"fn a() {}\n");
        let expected = blob_hash(&normalized);
        assert_eq!(scan.entries[0].blob_hash.as_str(), expected.as_str());
    }

    #[test]
    fn fingerprint_changes_with_content() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.rs", "fn a() {}");
        let first = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        write(dir.path(), "a.rs", "fn a() { b(); }");
        let second = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        assert_ne!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn changes_against_manifest_reports_three_kinds() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "keep.rs", "fn keep() {}");
        write(dir.path(), "gone.rs", "fn gone() {}");
        let base = WorktreeScan::scan(dir.path(), true, 1_048_576)
            .unwrap()
            .manifest();
        write(dir.path(), "keep.rs", "fn keep() { x(); }");
        std::fs::remove_file(dir.path().join("gone.rs")).unwrap();
        write(dir.path(), "new.rs", "fn new() {}");
        let scan = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        let changes = scan.changes_against(&base);
        let statuses: Vec<(&str, crate::domain::ChangeStatus)> = changes
            .iter()
            .map(|change| (change.path.as_str(), change.status))
            .collect();
        assert_eq!(
            statuses,
            vec![
                ("gone.rs", crate::domain::ChangeStatus::Deleted),
                ("keep.rs", crate::domain::ChangeStatus::Modified),
                ("new.rs", crate::domain::ChangeStatus::Added),
            ]
        );
    }

    #[test]
    fn ignored_directories_are_excluded() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".gitignore", "generated/\n");
        write(dir.path(), "generated/x.rs", "fn x() {}");
        write(dir.path(), "src/y.rs", "fn y() {}");
        let scan = WorktreeScan::scan(dir.path(), true, 1_048_576).unwrap();
        let paths: Vec<&str> = scan.entries.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["src/y.rs"]);
    }
}
