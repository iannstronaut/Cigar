// SPDX-License-Identifier: MPL-2.0

//! Integration coverage for lexical commit and worktree sync.
use cigar_core::config::Config;
use cigar_core::domain::SyncMode;
use cigar_core::git::GitSource;
use cigar_core::parse::ParserRegistry;
use cigar_core::sync::target::{sync_target, TargetSyncSummary};
use cigar_storage::TargetStore;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

struct Repo {
    _dir: TempDir,
    root: PathBuf,
}
impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.com"]);
        git(&root, &["config", "user.name", "Test"]);
        Self { _dir: dir, root }
    }
    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    fn commit(&self) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", "change"]);
    }
}
fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}
fn sync(repo: &Repo, store: &TargetStore, mode: SyncMode, full: bool) -> TargetSyncSummary {
    let config = Config::default();
    let source = GitSource::discover(&repo.root, config.git.max_file_bytes).unwrap();
    sync_target(
        store,
        &source,
        &ParserRegistry::with_defaults(),
        &config,
        "default",
        mode,
        full,
        1,
        true,
        false,
    )
    .unwrap()
}
fn new_store() -> (TempDir, TargetStore) {
    let dir = TempDir::new().unwrap();
    let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
    (dir, store)
}
fn paths(store: &TargetStore) -> Vec<String> {
    store
        .files("default")
        .unwrap()
        .into_iter()
        .map(|f| f.file_path)
        .collect()
}
#[test]
fn initial_and_noop_sync() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() { b(); }\nfn b() {}\n");
    repo.write("src/c.rs", "pub fn c() {}\n");
    repo.commit();
    let (_dir, store) = new_store();
    let first = sync(&repo, &store, SyncMode::Commit, false);
    assert_eq!(first.files_written, 2);
    assert_eq!(first.symbols_written, 3);
    assert_eq!(store.stats("default").unwrap().symbols, 3);
    let second = sync(&repo, &store, SyncMode::Commit, false);
    assert_eq!(second.files_written, 0);
    assert_eq!(second.symbols_written, 0);
}
#[test]
fn incremental_changes_match_full_rebuild() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() { b(); }\nfn b() {}\n");
    repo.write("src/c.rs", "pub fn c() {}\n");
    repo.commit();
    let (_dir, store) = new_store();
    sync(&repo, &store, SyncMode::Commit, false);
    repo.write("src/a.rs", "pub fn a() { d(); }\nfn d() {}\n");
    std::fs::remove_file(repo.root.join("src/c.rs")).unwrap();
    repo.write("src/new.rs", "pub struct New;\n");
    repo.commit();
    let outcome = sync(&repo, &store, SyncMode::Commit, false);
    assert_eq!(outcome.files_deleted, 1);
    let (_fresh_dir, fresh) = new_store();
    sync(&repo, &fresh, SyncMode::Commit, true);
    assert_eq!(paths(&store), paths(&fresh));
    let a = store.stats("default").unwrap();
    let b = fresh.stats("default").unwrap();
    assert_eq!((a.files, a.symbols, a.edges), (b.files, b.symbols, b.edges));
}
#[test]
fn dirty_then_commit_replaces_worktree_snapshot() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit();
    let (_dir, store) = new_store();
    sync(&repo, &store, SyncMode::Commit, false);
    repo.write("src/b.rs", "pub fn b() {}\n");
    sync(&repo, &store, SyncMode::Dirty, false);
    assert!(store.find_symbols("default", "b").unwrap().len() == 1);
    repo.commit();
    sync(&repo, &store, SyncMode::Commit, false);
    assert_eq!(store.stats("default").unwrap().files, 2);
}
#[test]
fn unsupported_and_binary_files_are_skipped() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.write("README.md", "docs\n");
    std::fs::write(repo.root.join("binary.rs"), [0_u8, 1, 2]).unwrap();
    repo.commit();
    let (_dir, store) = new_store();
    sync(&repo, &store, SyncMode::Commit, false);
    assert_eq!(paths(&store), vec!["src/a.rs"]);
}
