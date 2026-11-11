//! End-to-end sync tests (Fase 2): commit mode, dirty mode, multi-branch
//! content sharing, and incremental == full equality.

use std::path::{Path, PathBuf};
use std::process::Command;

use cigar_core::config::Config;
use cigar_core::domain::{BranchName, Namespace, SyncMode, SyncStatus};
use cigar_core::git::GitSource;
use cigar_core::parse::ParserRegistry;
use cigar_core::sync::{SyncEngine, SyncOptions};
use cigar_core::traits::{ContextStore, SystemClock};
use cigar_embed::FakeEmbedder;
use cigar_storage::open_or_init_store;
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
        let full = self.root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    fn commit(&self, message: &str) -> String {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", message]);
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.root)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn checkout(&self, branch: &str, create: bool) {
        if create {
            git(&self.root, &["checkout", "-q", "-b", branch]);
        } else {
            git(&self.root, &["checkout", "-q", branch]);
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git tersedia");
    assert!(status.success(), "git {args:?} gagal");
}

fn setup(repo: &Repo) -> (TempDir, cigar_storage::SqliteStore, Config, FakeEmbedder) {
    let store_dir = TempDir::new().unwrap();
    let dimensions = 16;
    let store = open_or_init_store(&store_dir.path().join("store.db"), dimensions).unwrap();
    let mut config = Config::default();
    config.embedding.provider = cigar_core::domain::EmbeddingProvider::Fake;
    config.embedding.dimensions = dimensions;
    config.git.mode = cigar_core::domain::GitMode::Worktree;
    let embedder = FakeEmbedder::new(dimensions).unwrap();
    let _ = repo;
    (store_dir, store, config, embedder)
}

fn sync(
    repo: &Repo,
    store: &cigar_storage::SqliteStore,
    config: &Config,
    embedder: &FakeEmbedder,
    branch: &str,
    options: SyncOptions,
) -> cigar_core::sync::SyncOutcome {
    let source = GitSource::discover(repo.root(), config.git.max_file_bytes).unwrap();
    let registry = ParserRegistry::with_defaults();
    let clock = SystemClock;
    let engine = SyncEngine::new(store, Some(embedder), &registry, config, &clock);
    engine
        .run(
            &source,
            &Namespace::default(),
            &BranchName::parse(branch).unwrap(),
            &options,
        )
        .unwrap()
}

fn manifest_paths(store: &cigar_storage::SqliteStore, branch: &str) -> Vec<String> {
    store
        .get_branch_manifest(&Namespace::default(), &BranchName::parse(branch).unwrap())
        .unwrap()
        .into_iter()
        .map(|file| file.file_path.to_string())
        .collect()
}

fn symbol_count(store: &cigar_storage::SqliteStore, branch: &str) -> u64 {
    store
        .stats(&Namespace::default(), &BranchName::parse(branch).unwrap())
        .unwrap()
        .symbols
}

#[test]
fn initial_sync_indexes_files_and_symbols() {
    let repo = Repo::new();
    repo.write(
        "src/auth.rs",
        "pub fn authenticate() -> bool {\n    validate()\n}\n\nfn validate() -> bool { true }\n",
    );
    repo.write("src/lib.rs", "pub fn init() {}\n");
    repo.commit("init");

    let (_dir, store, config, embedder) = setup(&repo);
    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    assert_eq!(outcome.summary.files_added, 2);
    assert_eq!(outcome.status, SyncStatus::Ready);
    assert_eq!(
        manifest_paths(&store, "main"),
        vec!["src/auth.rs", "src/lib.rs"]
    );
    assert_eq!(symbol_count(&store, "main"), 3);
    assert!(outcome.summary.embeddings_computed >= 3);
}

#[test]
fn noop_sync_computes_no_embeddings() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    let second = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    assert_eq!(second.summary.files_added, 0);
    assert_eq!(second.summary.files_modified, 0);
    assert_eq!(second.summary.embeddings_computed, 0);
    assert_eq!(second.summary.embeddings_reused, 0);
}

#[test]
fn one_changed_function_recomputes_one_vector() {
    let repo = Repo::new();
    repo.write(
        "src/a.rs",
        "pub fn keep() -> i32 { 1 }\n\npub fn change() -> i32 { 2 }\n",
    );
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    repo.write(
        "src/a.rs",
        "pub fn keep() -> i32 { 1 }\n\npub fn change() -> i32 { 3 }\n",
    );
    repo.commit("change");
    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    assert_eq!(outcome.summary.embeddings_computed, 1);
    assert_eq!(outcome.summary.files_modified, 1);
}

#[test]
fn import_change_recomputes_all_symbols_in_file() {
    let repo = Repo::new();
    repo.write(
        "src/a.rs",
        "use std::fmt;\n\npub fn one() {}\n\npub fn two() {}\n",
    );
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    repo.write(
        "src/a.rs",
        "use std::io;\n\npub fn one() {}\n\npub fn two() {}\n",
    );
    repo.commit("imports");
    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    // Imports are part of the template, so both symbols get new keys.
    assert_eq!(outcome.summary.embeddings_computed, 2);
}

#[test]
fn deleting_file_removes_occurrences_but_keeps_shared_objects() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn shared() {}\n");
    repo.write("src/b.rs", "pub fn shared() {}\n");
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    let symbols_before = symbol_count(&store, "main");

    std::fs::remove_file(repo.root().join("src/b.rs")).unwrap();
    repo.commit("remove b");
    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    assert_eq!(outcome.summary.files_deleted, 1);
    assert_eq!(manifest_paths(&store, "main"), vec!["src/a.rs"]);
    // The shared symbol definition remains because `a.rs` still uses it.
    assert!(symbol_count(&store, "main") < symbols_before + 1);
    assert!(symbol_count(&store, "main") >= 1);
}

#[test]
fn branches_share_content_but_isolate_occurrences() {
    let repo = Repo::new();
    repo.write("src/shared.rs", "pub fn shared() {}\n");
    repo.write("src/main_only.rs", "pub fn main_only() {}\n");
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    repo.checkout("feat/auth", true);
    repo.write(
        "src/feature.rs",
        "pub fn feature() {}\n\nfn helper() -> bool { true }\n",
    );
    repo.commit("feature");
    let feature = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "feat/auth",
        SyncOptions::default(),
    );
    assert_eq!(feature.summary.files_added, 3);

    assert_eq!(
        manifest_paths(&store, "main"),
        vec!["src/main_only.rs", "src/shared.rs"]
    );
    assert_eq!(
        manifest_paths(&store, "feat/auth"),
        vec!["src/feature.rs", "src/main_only.rs", "src/shared.rs"]
    );

    // The shared branch is untouched by the feature branch sync.
    let main_state = store
        .get_branch_state(&Namespace::default(), &BranchName::parse("main").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(main_state.status, SyncStatus::Ready);
}

#[test]
fn dirty_mode_does_not_advance_commit() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    let commit = repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);

    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions {
            mode: SyncMode::Dirty,
            ..SyncOptions::default()
        },
    );
    assert_eq!(outcome.summary.files_added, 1);
    assert!(outcome.head.is_none());
    let state = store
        .get_branch_state(&Namespace::default(), &BranchName::parse("main").unwrap())
        .unwrap()
        .unwrap();
    assert!(state.commit_hash.is_none());
    assert_eq!(state.status, SyncStatus::Dirty);

    // A following commit sync publishes the commit.
    repo.write("src/b.rs", "pub fn b() {}\n");
    let second = repo.commit("second");
    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    let state = store
        .get_branch_state(&Namespace::default(), &BranchName::parse("main").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(state.commit_hash.unwrap().as_str(), second);
    assert_eq!(state.status, SyncStatus::Ready);
    let _ = (outcome, commit);
}

#[test]
fn incremental_matches_full_reindex() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() { b() }\nfn b() {}\n");
    repo.write("src/c.rs", "pub fn c() {}\n");
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    repo.write("src/a.rs", "pub fn a() { c() }\nfn c() -> i32 { 2 }\n");
    repo.write("src/d.rs", "pub struct D;\n");
    std::fs::remove_file(repo.root().join("src/c.rs")).unwrap();
    repo.commit("change");
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    let incremental = manifest_paths(&store, "main");
    let incremental_symbols = symbol_count(&store, "main");
    let incremental_edges = store
        .stats(&Namespace::default(), &BranchName::parse("main").unwrap())
        .unwrap()
        .edges;

    // Full reindex into a second store.
    let fresh_dir = TempDir::new().unwrap();
    let fresh = open_or_init_store(&fresh_dir.path().join("store.db"), 16).unwrap();
    let full = sync(
        &repo,
        &fresh,
        &config,
        &embedder,
        "main",
        SyncOptions {
            full: true,
            ..SyncOptions::default()
        },
    );
    assert_eq!(full.summary.files_added, 2);
    assert_eq!(manifest_paths(&fresh, "main"), incremental,);
    assert_eq!(symbol_count(&fresh, "main"), incremental_symbols);
    assert_eq!(
        fresh
            .stats(&Namespace::default(), &BranchName::parse("main").unwrap())
            .unwrap()
            .edges,
        incremental_edges
    );
}

#[test]
fn crash_recovery_recomputes_after_error_state() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);

    // Simulate an interrupted sync.
    let namespace = Namespace::default();
    let branch = BranchName::parse("main").unwrap();
    store
        .set_branch_state(
            &namespace,
            &cigar_core::domain::BranchState {
                namespace: namespace.clone(),
                branch: branch.clone(),
                commit_hash: None,
                last_sync_mode: SyncMode::Commit,
                status: SyncStatus::Syncing,
                worktree_fingerprint: None,
                config_fingerprint: config.fingerprint().unwrap(),
                updated_at_ms: 1,
            },
        )
        .unwrap();

    let outcome = sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    assert_eq!(outcome.summary.files_added, 1);
    let state = store
        .get_branch_state(&namespace, &branch)
        .unwrap()
        .unwrap();
    assert_eq!(state.status, SyncStatus::Ready);
}

#[test]
fn config_drift_is_rejected() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    let (_dir, store, mut config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );

    config.embedding.model = "different-model".into();
    let source = GitSource::discover(repo.root(), config.git.max_file_bytes).unwrap();
    let registry = ParserRegistry::with_defaults();
    let clock = SystemClock;
    let engine = SyncEngine::new(&store, Some(&embedder), &registry, &config, &clock);
    let error = engine
        .run(
            &source,
            &Namespace::default(),
            &BranchName::parse("main").unwrap(),
            &SyncOptions::default(),
        )
        .unwrap_err();
    assert!(
        matches!(error, cigar_core::EngineError::ConfigMismatch { .. }),
        "{error}"
    );
}

#[test]
fn unsupported_and_binary_files_are_skipped() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.write("README.md", "# docs\n");
    std::fs::write(repo.root().join("blob.rs"), [0_u8, 1, 2, 3]).unwrap();
    repo.commit("init");
    let (_dir, store, config, embedder) = setup(&repo);
    sync(
        &repo,
        &store,
        &config,
        &embedder,
        "main",
        SyncOptions::default(),
    );
    assert_eq!(manifest_paths(&store, "main"), vec!["src/a.rs"]);
}
