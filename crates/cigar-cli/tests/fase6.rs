// SPDX-License-Identifier: MPL-2.0

//! Fase 6 end-to-end coverage for the target (lexical) index.
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::Value;
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

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        run_in(args, &self.root)
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

fn git_head(root: &Path) -> String {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
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

fn json(repo: &Repo, args: &[&str]) -> Value {
    let (code, stdout, stderr) = repo.run(args);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|error| panic!("{error}: {stdout}"))
}

fn init(repo: &Repo, fs_only: bool) {
    let mut args = vec!["init", "--offline"];
    if fs_only {
        args.push("--git-mode");
        args.push("fs-only");
    }
    let (code, _stdout, stderr) = repo.run(&args);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn fs_mode_indexes_worktree_and_handles_dirty_update_delete() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn alpha() { beta(); }\nfn beta() {}\n");
    repo.write("notes.txt", "not source\n");
    init(&repo, true);

    let first = json(&repo, &["sync", "--json"]);
    assert_eq!(first["mode"], "commit");
    assert_eq!(first["branches"][0]["files_written"], 1);
    assert_eq!(first["branches"][0]["symbols_written"], 2);

    repo.write("src/a.rs", "pub fn alpha_updated() {}\n");
    std::fs::remove_file(repo.root.join("notes.txt")).unwrap();
    repo.write("src/new.rs", "pub struct NewType;\n");
    let second = json(&repo, &["sync", "--dirty", "--json"]);
    assert_eq!(second["mode"], "dirty");
    assert_eq!(second["branches"][0]["files_deleted"], 0);
    assert_eq!(second["branches"][0]["files_written"], 2);

    let status = json(&repo, &["status", "--json"]);
    assert_eq!(status["stats"]["files"], 2);
    assert_eq!(
        json(&repo, &["query", "alpha_updated", "--json"])["results"][0]["symbol"]["name"],
        "alpha_updated"
    );
    assert!(json(&repo, &["query", "beta", "--json"])["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn commit_sync_deletes_and_updates_target_files() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn old_name() {}\n");
    repo.write("src/remove.rs", "pub fn removed() {}\n");
    repo.commit("first");
    init(&repo, false);
    let first = json(&repo, &["sync", "--json"]);
    assert_eq!(first["branches"][0]["files_written"], 2);

    repo.write("src/a.rs", "pub fn new_name() {}\n");
    std::fs::remove_file(repo.root.join("src/remove.rs")).unwrap();
    repo.write("src/add.rs", "pub fn added() {}\n");
    repo.commit("second");
    let second = json(&repo, &["sync", "--json"]);
    assert_eq!(second["branches"][0]["files_deleted"], 1);
    assert_eq!(second["branches"][0]["files_written"], 2);

    assert_eq!(
        json(&repo, &["inspect", "new_name", "--json"])["symbol"]["file"],
        "src/a.rs"
    );
    let (code, _stdout, _stderr) = repo.run(&["inspect", "removed", "--json"]);
    assert_ne!(code, 0);
    assert!(
        json(&repo, &["query", "added", "--json"])["results"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
}

#[test]
fn commit_and_dirty_preserve_head_and_sync_state_contract() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn first() {}\n");
    repo.commit("first");
    init(&repo, false);
    let first_head = git_head(&repo.root);
    json(&repo, &["sync", "--json"]);

    let head_file = repo.root.join(".cig/HEAD");
    assert_eq!(
        std::fs::read_to_string(&head_file).unwrap().trim(),
        first_head
    );
    let store = repo.root.join(".cig/store.db");
    let connection = Connection::open(&store).unwrap();
    let state: String = connection
        .query_row(
            "SELECT last_commit_hash FROM sync_state WHERE namespace='default'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, first_head);
    drop(connection);

    repo.write("src/a.rs", "pub fn dirty_version() {}\n");
    json(&repo, &["sync", "--dirty", "--json"]);
    assert_eq!(
        std::fs::read_to_string(&head_file).unwrap().trim(),
        first_head
    );
    let connection = Connection::open(&store).unwrap();
    let state: String = connection
        .query_row(
            "SELECT last_commit_hash FROM sync_state WHERE namespace='default'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // Dirty snapshots invalidate the commit baseline so the next commit sync
    // fully reconciles files that changed only in the working tree.
    assert!(state.is_empty());
}

#[test]
fn strict_sync_error_rolls_back_target_batch_and_pointers() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn stable() {}\n");
    repo.commit("first");
    init(&repo, false);
    json(&repo, &["sync", "--json"]);
    let old_head = git_head(&repo.root);
    let before = json(&repo, &["status", "--json"])["stats"].clone();

    std::fs::write(repo.root.join("src/a.rs"), [0xff_u8, 0xfe, 0xfd]).unwrap();
    repo.commit("invalid");
    let (code, _stdout, stderr) = repo.run(&["sync", "--strict"]);
    assert_eq!(code, 2, "{stderr}");

    assert_eq!(json(&repo, &["status", "--json"])["stats"], before);
    assert_eq!(
        std::fs::read_to_string(repo.root.join(".cig/HEAD"))
            .unwrap()
            .trim(),
        old_head
    );
    assert_eq!(
        json(&repo, &["inspect", "stable", "--json"])["symbol"]["name"],
        "stable"
    );
}

#[test]
fn status_query_inspect_reindex_and_repair_use_target_contract() {
    let repo = Repo::new();
    repo.write(
        "src/lib.rs",
        "/// Checks a token.\npub fn validate_token() { helper(); }\nfn helper() {}\n",
    );
    repo.commit("init");
    init(&repo, false);
    json(&repo, &["sync", "--json"]);

    let status = json(&repo, &["status", "--verify", "--json"]);
    assert_eq!(status["search"]["mode"], "lexical-graph");
    assert_eq!(status["stats"]["symbols"], 2);
    assert!(status["health"].as_array().unwrap().is_empty());

    let query = json(&repo, &["query", "validate_token", "--json"]);
    assert_eq!(query["results"][0]["symbol"]["name"], "validate_token");
    let inspected = json(&repo, &["inspect", "validate_token", "--json"]);
    assert_eq!(inspected["symbol"]["name"], "validate_token");
    assert!(inspected["symbol"]["doc"]
        .as_str()
        .unwrap()
        .contains("Checks"));

    let rebuilt = json(&repo, &["reindex", "--yes", "--no-backup", "--json"]);
    assert_eq!(rebuilt["files"], 1);
    assert_eq!(rebuilt["symbols"], 2);
    assert_eq!(rebuilt["backup"], false);

    let repaired = json(&repo, &["repair", "--json"]);
    assert_eq!(repaired["healthy"], true);
    assert!(repaired["problems"].as_array().unwrap().is_empty());

    let (code, _stdout, stderr) = repo.run(&["repair", "--head"]);
    assert_eq!(code, 2, "{stderr}");
}

#[test]
fn empty_fs_repository_is_a_valid_target_snapshot() {
    let repo = Repo::new();
    init(&repo, true);
    let synced = json(&repo, &["sync", "--json"]);
    assert_eq!(synced["branches"][0]["files_written"], 0);
    let status = json(&repo, &["status", "--json"]);
    assert_eq!(status["stats"]["files"], 0);
    assert_eq!(status["stats"]["symbols"], 0);
}
