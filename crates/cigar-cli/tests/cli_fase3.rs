//! Fase 3 CLI tests: reindex, model status, config drift, Go/Python sync.

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
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git tersedia");
    assert!(status.success(), "git {args:?} gagal");
}

fn run(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let output = cigar()
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("cigar berjalan");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn init_fake(repo: &Repo) {
    let (code, _out, err) = run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "32",
        ],
        repo.root(),
    );
    assert_eq!(code, 0, "{err}");
}

#[test]
fn model_status_reports_configured_model() {
    let repo = Repo::new();
    init_fake(&repo);
    let (code, stdout, stderr) = run(&["model", "status", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["dimensions"], 768);
    assert_eq!(value["pooling"], "cls");
    assert_eq!(value["license"], "Apache-2.0");
}

#[test]
fn model_download_refuses_offline() {
    let repo = Repo::new();
    init_fake(&repo);
    let (code, _out, stderr) = run(&["model", "download", "--offline"], repo.root());
    assert_eq!(code, 5, "{stderr}");
    assert!(stderr.contains("offline"), "{stderr}");
}

#[test]
fn reindex_rebuilds_and_reports() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.write("src/b.rs", "pub fn b() {}\n");
    repo.commit("init");
    init_fake(&repo);
    let (code, _out, err) = run(&["sync"], repo.root());
    assert_eq!(code, 0, "{err}");

    let (code, stdout, stderr) = run(&["reindex", "--yes", "--no-backup", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["files"], 2);
    assert_eq!(value["symbols"], 2);
}

#[test]
fn reindex_creates_backup_by_default() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    run(&["sync"], repo.root());
    let (code, _out, err) = run(&["reindex", "--yes"], repo.root());
    assert_eq!(code, 0, "{err}");
    let backups: Vec<PathBuf> = std::fs::read_dir(repo.root().join(".cig"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(".bak-"))
        })
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
}

#[test]
fn go_and_python_files_are_indexed() {
    let repo = Repo::new();
    repo.write(
        "internal/auth.go",
        "package auth\n\n// Authenticate checks a token.\nfunc Authenticate(token string) bool {\n\treturn validate(token)\n}\n\nfunc validate(_ string) bool { return true }\n",
    );
    repo.write(
        "app/auth.py",
        "class Repo:\n    def save(self):\n        return None\n\ndef authenticate(token):\n    \"\"\"Checks a token.\"\"\"\n    return Repo().save()\n",
    );
    repo.commit("init");
    init_fake(&repo);
    let (code, stdout, stderr) = run(&["sync", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let branch = &value["branches"][0];
    assert_eq!(branch["files_added"], 2);
    // Go: Authenticate + validate; Python: Repo, Repo::save, authenticate.
    assert_eq!(branch["symbols_added"], 5);
}

#[test]
fn dimension_mismatch_is_exit_6() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_fake(&repo);
    run(&["sync"], repo.root());

    // Corrupt the configured dimension while the store holds 32.
    let config = repo.root().join(".cig").join("config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    let patched = text.replace("dimensions = 32", "dimensions = 64");
    std::fs::write(&config, patched).unwrap();

    let (code, _out, stderr) = run(&["status"], repo.root());
    assert_eq!(code, 6, "{stderr}");
    assert!(stderr.contains("tidak cocok"), "{stderr}");
}
