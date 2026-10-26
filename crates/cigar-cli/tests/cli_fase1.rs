//! End-to-end CLI tests for Fase 1: `init`, `status`, `version`.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn cigar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cigar"))
}

fn run(args: &[&str], cwd: &Path) -> (i32, String, String) {
    let output = cigar()
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .current_dir(cwd)
        .output()
        .expect("cigar harus bisa dijalankan");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn init_creates_state_and_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();

    let (code, stdout, stderr) = run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "64",
        ],
        root,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("diinisialisasi"), "{stdout}");

    let cig = root.join(".cig");
    assert!(cig.is_dir());
    assert!(cig.join("config.toml").is_file());
    assert!(cig.join("store.db").is_file());
    assert!(cig.join("HEAD").is_file());
    let head = std::fs::read_to_string(cig.join("HEAD")).unwrap();
    assert_eq!(head.trim(), "ref: refs/heads/main");

    let (code, stdout, stderr) = run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "64",
        ],
        root,
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("sudah diinisialisasi"), "{stdout}");
}

#[test]
fn init_twice_keeps_store_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "32",
        ],
        root,
    );
    let store = root.join(".cig").join("store.db");
    let first = std::fs::metadata(&store).unwrap().len();
    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "32",
            "--force",
        ],
        root,
    );
    assert!(store.exists());
    assert_eq!(std::fs::metadata(&store).unwrap().len(), first);
}

#[test]
fn status_reports_empty_index_as_json() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "64",
        ],
        root,
    );

    let (code, stdout, stderr) = run(&["status", "--json"], root);
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON valid");
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["stats"]["files"], 0);
    assert_eq!(value["stats"]["symbols"], 0);
    assert_eq!(value["branch"], "main");
    assert_eq!(value["namespace"], "default");
}

#[test]
fn status_verify_detects_clean_store() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "64",
        ],
        root,
    );
    let (code, stdout, stderr) = run(&["status", "--verify"], root);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("foreign_key_check: ok"), "{stdout}");
    assert!(stdout.contains("quick_check: ok"), "{stdout}");
}

#[test]
fn status_without_init_exits_3() {
    let dir = TempDir::new().unwrap();
    let (code, _stdout, stderr) = run(&["status"], dir.path());
    assert_eq!(code, 3, "stderr: {stderr}");
    assert!(stderr.contains("belum diinisialisasi"), "{stderr}");
}

#[test]
fn invalid_namespace_is_usage_error() {
    let dir = TempDir::new().unwrap();
    let (code, _stdout, stderr) = run(&["status", "--namespace", "../escape"], dir.path());
    assert_eq!(code, 2, "stderr: {stderr}");
}

#[test]
fn version_reports_schema_and_sqlite_vec() {
    let dir = TempDir::new().unwrap();
    let (code, stdout, stderr) = run(&["version"], dir.path());
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("cigar "), "{stdout}");
    assert!(stdout.contains("schema: 2"), "{stdout}");
    assert!(stdout.contains("sqlite-vec: v"), "{stdout}");
}

#[test]
fn version_json_is_machine_readable() {
    let dir = TempDir::new().unwrap();
    let (code, stdout, stderr) = run(&["version", "--json"], dir.path());
    assert_eq!(code, 0, "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("JSON valid");
    assert_eq!(value["schema_version"], 2);
    assert!(value["sqlite_vec"].as_str().unwrap().starts_with('v'));
}

#[test]
fn init_adds_cig_to_git_exclude_once() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let git_dir = root.join(".git");
    std::fs::create_dir_all(git_dir.join("info")).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "16",
        ],
        root,
    );
    run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "16",
        ],
        root,
    );

    let exclude = std::fs::read_to_string(git_dir.join("info").join("exclude")).unwrap();
    let occurrences = exclude
        .lines()
        .filter(|line| line.trim() == ".cig/")
        .count();
    assert_eq!(occurrences, 1, "{exclude}");
}

#[test]
fn quiet_init_prints_nothing_to_stdout() {
    let dir = TempDir::new().unwrap();
    let output = cigar()
        .args([
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "16",
            "--quiet",
        ])
        .arg("--cwd")
        .arg(dir.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
}
