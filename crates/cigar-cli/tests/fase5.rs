//! Fase 5 tests: hooks, MCP stdio, branch/gc, bare repo, fs-only, detached HEAD.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
fn hook_install_is_idempotent_and_preserves_existing() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);

    let hooks = repo.root().join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let post_commit = hooks.join("post-commit");
    std::fs::write(&post_commit, "#!/bin/sh\necho pre-existing\n").unwrap();

    let (code, _out, err) = repo.run(&["hook", "install"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&post_commit).unwrap();
    assert!(text.contains("echo pre-existing"), "{text}");
    assert!(text.contains("# >>> cigar >>>"), "{text}");

    // Second install must not duplicate the block.
    let (code, _out, err) = repo.run(&["hook", "install"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&post_commit).unwrap();
    assert_eq!(text.matches("# >>> cigar >>>").count(), 1, "{text}");

    let (code, _out, err) = repo.run(&["hook", "uninstall"]);
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(&post_commit).unwrap();
    assert!(text.contains("echo pre-existing"), "{text}");
    assert!(!text.contains("# >>> cigar >>>"), "{text}");
}

#[test]
fn hook_install_dry_run_writes_nothing() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    let (code, stdout, err) = repo.run(&["hook", "install", "--dry-run"]);
    assert_eq!(code, 0, "{err}");
    assert!(stdout.contains("akan diperbarui"), "{stdout}");
    let post_commit = repo.root().join(".git").join("hooks").join("post-commit");
    assert!(!post_commit.exists());
}

#[test]
fn branch_list_and_prune() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    git(repo.root(), &["checkout", "-q", "-b", "temp/branch"]);
    repo.write("src/b.rs", "pub fn b() {}\n");
    repo.commit("temp");
    repo.run(&["sync"]);
    git(repo.root(), &["checkout", "-q", "main"]);
    git(repo.root(), &["branch", "-D", "temp/branch"]);

    let (code, stdout, err) = repo.run(&["branch", "list", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"].as_array().unwrap().len(), 2);

    let (code, _out, err) = repo.run(&["branch", "prune", "--dry-run"]);
    assert_eq!(code, 0, "{err}");
    let (code, stdout, err) = repo.run(&["branch", "prune"]);
    assert_eq!(code, 0, "{err}");
    assert!(stdout.contains("dihapus"), "{stdout}");
}

#[test]
fn gc_dry_run_reports_without_deleting() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    let (code, stdout, err) = repo.run(&["gc", "--dry-run", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["dry_run"], true);
    let vectors = value["vectors_deleted"].as_u64().unwrap();
    assert_eq!(
        vectors, 0,
        "tidak boleh ada vektor orphan pada indeks bersih"
    );
}

#[test]
fn bare_repository_syncs_and_queries() {
    let source = Repo::new();
    source.write("src/a.rs", "pub fn bare_fn() {}\n");
    source.commit("init");

    let bare_dir = TempDir::new().unwrap();
    let bare = bare_dir.path().join("repo.git");
    let status = Command::new("git")
        .args(["clone", "-q", "--bare"])
        .arg(source.root())
        .arg(&bare)
        .status()
        .unwrap();
    assert!(status.success());

    let (code, _out, err) = run_in(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "32",
            "--git-mode",
            "bare",
            "--branch",
            "main",
        ],
        &bare,
    );
    assert_eq!(code, 0, "{err}");
    let (code, stdout, err) = run_in(&["sync", "--branch", "main", "--json"], &bare);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["files_added"], 1);

    let (code, stdout, err) = run_in(&["query", "bare fn", "--branch", "main", "--json"], &bare);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["index"]["branch"], "main");
}

#[test]
fn fs_only_mode_indexes_without_git() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn plain_file() {}\n").unwrap();

    let (code, _out, err) = run_in(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "32",
            "--git-mode",
            "fs-only",
        ],
        dir.path(),
    );
    assert_eq!(code, 0, "{err}");
    let (code, stdout, err) = run_in(&["sync", "--json"], dir.path());
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["files_added"], 1);
}

#[test]
fn detached_head_uses_detached_branch_name() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo.root())
        .output()
        .unwrap();
    let oid = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    git(repo.root(), &["checkout", "-q", &oid]);

    let (code, stdout, err) = repo.run(&["sync", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["branches"][0]["branch"], "@detached");
}

#[test]
fn lock_contention_exits_4() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);

    // Hold the lock from this process, then ask cigar to sync.
    let lock_path = repo.root().join(".cig").join("lock");
    let lock = cigar_storage::StoreLock::acquire(&lock_path).unwrap();
    let (code, _out, stderr) = repo.run(&["sync"]);
    assert_eq!(code, 4, "{stderr}");
    assert!(stderr.contains("lock"), "{stderr}");
    drop(lock);
}

#[test]
fn mcp_stdio_handshake_and_tools() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn mcp_target() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    let mut child = cigar()
        .args(["mcp"])
        .arg("--cwd")
        .arg(repo.root())
        .current_dir(repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp berjalan");

    let requests = [
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cigar_status","arguments":{}}}),
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"cigar_query","arguments":{"query":"mcp target","top_k":1,"max_tokens":400}}}),
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"resources/list"}),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for request in &requests {
            writeln!(stdin, "{request}").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("JSON-RPC valid"))
        .collect();
    assert_eq!(lines.len(), 5, "{stdout}");

    assert_eq!(lines[0]["result"]["protocolVersion"], "2024-11-05");
    let tools = lines[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"cigar_query"));
    assert!(names.contains(&"cigar_inspect"));
    assert!(names.contains(&"cigar_status"));
    assert!(!names.contains(&"cigar_sync"), "cigar_sync harus opt-in");

    assert_eq!(lines[2]["result"]["isError"], false);
    assert_eq!(lines[3]["result"]["isError"], false);
    assert!(lines[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("# CIGAR context"));
    assert_eq!(lines[4]["error"]["code"], -32601);
}

#[test]
fn mcp_sync_tool_requires_allow_write() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    repo.run(&["sync"]);

    let mut child = cigar()
        .args(["mcp"])
        .arg("--cwd")
        .arg(repo.root())
        .current_dir(repo.root())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).unwrap();
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"cigar_sync","arguments":{}}})).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let tools = lines[1]["result"]["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|tool| tool["name"] == "cigar_sync"));
    // Calling an unregistered tool is an invalid-params style error.
    assert_eq!(lines[2]["error"]["code"], -32602);
}

#[test]
fn mcp_without_init_returns_tool_error() {
    let dir = TempDir::new().unwrap();
    let mut child = cigar()
        .args(["mcp"])
        .arg("--cwd")
        .arg(dir.path())
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}})).unwrap();
        writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"cigar_status","arguments":{}}})).unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[1]["result"]["isError"], true);
    assert!(lines[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("NOT_INITIALIZED"));
}
