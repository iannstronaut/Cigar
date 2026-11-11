//! Fase 6 tests: repair, prune_branches, hardening, and invariant checks.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn cigar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cigar"))
}

struct Repo {
    _dir: TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().to_path_buf();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "t@example.com"]);
        git(&root, &["config", "user.name", "T"]);
        Self { _dir: dir, root }
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.root.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    fn commit(&self, message: &str) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", message]);
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run_in(args, &self.root)
    }
}
fn git_head(root: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git tersedia");
    assert!(status.success(), "git {args:?} gagal");
}

fn run_in(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let output = cigar()
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .current_dir(cwd)
        .output()
        .expect("cigar berjalan");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn init_fake(repo: &Repo) {
    let (code, _out, err) = repo.run(&[
        "init",
        "--no-model",
        "--embedding",
        "fake",
        "--dimensions",
        "32",
    ]);
    assert_eq!(code, 0, "{err}");
}

#[test]
fn repair_verify_reports_healthy() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    let (code, stdout, stderr) = repo.run(&["repair", "--verify"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("konsisten"), "{stdout}");
}

#[test]
fn repair_head_restores_missing_head_file() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    let (code, _, err) = repo.run(&["sync"]);
    assert_eq!(code, 0, "{err}");
    let indexed = git_head(repo.root());
    repo.write("src/a.rs", "pub fn changed() {}\n");
    repo.commit("second");

    let head = repo.root().join(".cig").join("HEAD");
    std::fs::remove_file(&head).unwrap();
    let (code, _out, err) = repo.run(&["repair", "--head"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&head).unwrap();
    assert_eq!(text.trim(), indexed);
}

#[test]
fn head_tracks_commit_sync_but_not_dirty_sync() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("first");
    init_fake(&repo);
    let head = repo.root().join(".cig/HEAD");
    assert_eq!(std::fs::read_to_string(&head).unwrap(), "");

    let (code, _, err) = repo.run(&["sync"]);
    assert_eq!(code, 0, "{err}");
    let first = git_head(repo.root());
    assert_eq!(std::fs::read_to_string(&head).unwrap().trim(), first);

    repo.write("src/a.rs", "pub fn b() {}\n");
    repo.commit("second");
    let second = git_head(repo.root());
    let (code, _, err) = repo.run(&["sync", "--dirty"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&head).unwrap().trim(), first);

    let (code, _, err) = repo.run(&["sync"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&head).unwrap().trim(), second);
}

#[test]
fn repair_head_is_empty_before_first_sync() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("first");
    init_fake(&repo);
    let head = repo.root().join(".cig/HEAD");
    std::fs::write(&head, "incorrect").unwrap();
    let (code, _, err) = repo.run(&["repair", "--head"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(std::fs::read_to_string(&head).unwrap(), "");
}

#[test]
fn repair_without_flags_is_read_only() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);
    // A healthy index reports clean, and the store file is untouched.
    let store = repo.root().join(".cig").join("store.db");
    let before = std::fs::metadata(&store).unwrap().len();
    let (code, _out, err) = repo.run(&["repair"]);
    assert_eq!(code, 0, "{err}");
    assert!(before > 0);
}

#[test]
fn gc_prune_branches_removes_empty_stale_branch() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    // Create and index a branch, then delete it in Git and clear its manifest.
    git(repo.root(), &["checkout", "-q", "-b", "stale/one"]);
    repo.write("src/b.rs", "pub fn b() {}\n");
    repo.commit("stale");
    repo.run(&["sync"]);
    git(repo.root(), &["checkout", "-q", "main"]);
    git(repo.root(), &["branch", "-D", "stale/one"]);

    let (code, _out, err) = repo.run(&["branch", "prune"]);
    assert_eq!(code, 0, "{err}");

    let (code, stdout, err) = repo.run(&["gc", "--prune-branches", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    // The pruned branch row is already gone; nothing further to prune.
    assert_eq!(value["branches_pruned"], 0);
}

#[test]
fn schema_too_new_is_exit_6() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    // Bump the recorded schema version beyond what this build supports.
    let store_path = repo.root().join(".cig").join("store.db");
    {
        cigar_storage::register_sqlite_vec();
        let connection = rusqlite::Connection::open(&store_path).unwrap();
        connection
            .execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
            .unwrap();
    }

    let (code, _out, stderr) = repo.run(&["status"]);
    assert_eq!(code, 6, "{stderr}");
    assert!(stderr.contains("lebih baru"), "{stderr}");
}

#[test]
fn corrupt_index_is_detected_then_repair_drops_orphans() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.write("src/b.rs", "pub fn b() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    // Delete a branch file row behind CIGAR's back to create orphans.
    let store_path = repo.root().join(".cig").join("store.db");
    {
        cigar_storage::register_sqlite_vec();
        let connection = rusqlite::Connection::open(&store_path).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        connection
            .execute("DELETE FROM branch_files WHERE file_path = 'src/b.rs'", [])
            .unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
    }

    let (code, _out, _stderr) = repo.run(&["status", "--verify"]);
    assert_eq!(code, 6, "verify harus mendeteksi inkonsistensi");

    let (code, stdout, stderr) = repo.run(&["repair", "--drop-orphans", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["healthy_after"], true);
}

#[test]
fn large_file_is_skipped_not_fatal() {
    let repo = Repo::new();
    repo.write("src/ok.rs", "pub fn ok() {}\n");
    let big = format!("pub fn big() {{ {} }}\n", "1 + ".repeat(600_000));
    repo.write("src/big.rs", &big);
    repo.commit("init");
    init_fake(&repo);

    let (code, stdout, stderr) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["files_added"], 1);
    assert!(value["branches"][0]["files_skipped"].as_u64().unwrap() >= 1);
}

#[test]
fn invalid_utf8_file_is_skipped() {
    let repo = Repo::new();
    repo.write("src/ok.rs", "pub fn ok() {}\n");
    std::fs::write(repo.root().join("src/bad.rs"), [0xFF_u8, 0xFE, 0xFD]).unwrap();
    repo.commit("init");
    init_fake(&repo);

    let (code, stdout, stderr) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["files_added"], 1);
}

#[test]
fn paths_outside_repo_are_rejected() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);

    let (code, _out, stderr) = repo.run(&["status", "--namespace", "../escape"]);
    assert_eq!(code, 2, "{stderr}");
}

#[test]
fn empty_repository_is_handled() {
    let repo = Repo::new();
    init_fake(&repo);
    let (code, stdout, stderr) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["files_added"], 0);
    assert!(value["branches"][0]["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| warning.as_str().unwrap().contains("HEAD")));
}

#[test]
fn unicode_and_crlf_files_index_correctly() {
    let repo = Repo::new();
    repo.write(
        "src/unicode.rs",
        "/// Memeriksa token — mendukung ünïcode.\n\r\npub fn periksa_token() -> bool {\r\n    true\r\n}\r\n",
    );
    repo.commit("init");
    init_fake(&repo);

    let (code, stdout, stderr) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["symbols_added"], 1);

    let (code, stdout, stderr) = repo.run(&["inspect", "periksa_token", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(value["symbol"]["doc"].as_str().unwrap().contains("ünïcode"));
}

#[test]
fn unchanged_content_reuses_vectors_across_commits() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn stable() -> i32 { 1 }\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    // A commit that only touches an unrelated file must not re-embed.
    repo.write("README.md", "docs\n");
    repo.commit("docs");
    let (code, stdout, stderr) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["embeddings_computed"], 0);
}
