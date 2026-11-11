// SPDX-License-Identifier: MPL-2.0

//! Query ranking, budget, snapshot replacement, graph expansion,
//! inspect resolution and ambiguity for the v4 lexical index.

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

fn run(args: &[&str], cwd: &Path) -> (i32, String, String) {
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

fn init_and_sync(repo: &Repo) {
    let (code, _out, err) = run(&["init"], repo.root());
    assert_eq!(code, 0, "{err}");
    let (code, _out, err) = run(&["sync"], repo.root());
    assert_eq!(code, 0, "{err}");
}

#[test]
fn query_returns_ranked_results_with_relations() {
    let repo = Repo::new();
    repo.write(
        "src/auth.rs",
        "pub fn validate_token(token: &str) -> bool {\n    check_expiry(token)\n}\n\nfn check_expiry(_t: &str) -> bool { true }\n",
    );
    repo.write(
        "src/ui.rs",
        "pub fn button_color() -> &'static str { \"#fff\" }\n",
    );
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(&["query", "validate token", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["schema_version"], 4);
    assert!(value["namespace"].is_string());
    assert_eq!(value["query"], "validate token");
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    // Check ranking and the v4 lexical response structure.
    let first = &results[0];
    assert_eq!(first["rank"], 1);
    assert!(first["symbol"]["file"]
        .as_str()
        .unwrap()
        .starts_with("src/"));
    assert!(first["score"].as_f64().unwrap() > 0.0);
    assert!(first["symbol"]["signature"].is_string());
    assert!(first["relations"].is_array());
}

#[test]
fn query_honours_token_budget() {
    let repo = Repo::new();
    repo.write(
        "src/big.rs",
        &format!(
            "pub fn function_long() -> &'static str {{ \"{}\" }}\n",
            "x".repeat(2000)
        ),
    );
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(
        &[
            "query",
            "function",
            "--max-tokens",
            "300",
            "--top-k",
            "5",
            "--json",
        ],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let used: usize = results
        .iter()
        .map(|result| result["symbol"]["code"].as_str().unwrap().len())
        .sum();
    assert_eq!(used, 300 * 4, "code budget should truncate the long symbol");
    assert_eq!(value["results"][0]["rank"], 1);
}

#[test]
fn tiny_budget_is_rejected() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, _out, stderr) = run(&["query", "a", "--max-tokens", "100"], repo.root());
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("256"), "{stderr}");
}

#[test]
fn empty_query_is_rejected() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, _out, stderr) = run(&["query", "   "], repo.root());
    assert_eq!(code, 2, "{stderr}");
}

#[test]
fn path_filter_restricts_results() {
    let repo = Repo::new();
    repo.write(
        "src/auth/token.rs",
        "pub fn validate_token() -> bool { true }\n",
    );
    repo.write(
        "src/ui/button.rs",
        "pub fn validate_token_ui() -> bool { true }\n",
    );
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(
        &["query", "validate token", "--path", "src/auth/**", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    for result in results {
        let file = result["symbol"]["file"].as_str().unwrap();
        assert!(file.starts_with("src/auth/"), "{file}");
    }
}

#[test]
fn kind_filter_restricts_results() {
    let repo = Repo::new();
    repo.write(
        "src/a.rs",
        "pub struct Token { value: String }\npub fn token() -> Token { Token { value: String::new() } }\n",
    );
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(
        &["query", "token", "--kind", "struct", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    for result in results {
        assert_eq!(result["symbol"]["kind"], "struct");
    }
}

#[test]
fn sync_replaces_the_single_snapshot_after_branch_changes() {
    let repo = Repo::new();
    repo.write("src/shared.rs", "pub fn shared_helper() {}\n");
    repo.commit("init");
    init_and_sync(&repo);

    repo.checkout("feat/only", true);
    repo.write("src/feature.rs", "pub fn feature_only_marker() {}\n");
    repo.commit("feature");
    let (code, _, err) = run(&["sync"], repo.root());
    assert_eq!(code, 0, "{err}");
    let (code, stdout, stderr) = run(&["query", "feature_only_marker", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(value["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["symbol"]["file"] == "src/feature.rs"));

    repo.checkout("main", false);
    let (code, _, err) = run(&["sync"], repo.root());
    assert_eq!(code, 0, "{err}");
    let (code, stdout, stderr) = run(&["query", "feature_only_marker", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(value["results"].as_array().unwrap().is_empty());
}

#[test]
fn query_rejects_a_branch_other_than_checkout() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, _out, stderr) = run(&["query", "a", "--branch", "nope"], repo.root());
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("branch checkout saat ini"), "{stderr}");
}

#[test]
fn inspect_resolves_by_qualified_name_and_shows_relations() {
    let repo = Repo::new();
    repo.write(
        "src/a.rs",
        "struct S;\nimpl S {\n    fn helper(&self) {}\n    fn run(&self) { self.helper(); }\n}\n",
    );
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(&["inspect", "S::run", "--json"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["schema_version"], 4);
    assert!(value["namespace"].is_string());
    assert_eq!(value["symbol"]["qualified_name"], "S::run");
    assert_eq!(value["symbol"]["kind"], "method");
    let relations = value["relations"].as_array().unwrap();
    assert!(relations.iter().any(
        |relation| relation["symbol"]["qualified_name"] == "S::helper"
            && relation["relation"] == "CALLS"
            && relation["incoming"] == false
            && relation["depth"] == 1
    ));
}

#[test]
fn inspect_ambiguous_reports_error_and_file_filter_disambiguates() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn dup() {}\n");
    repo.write("src/b.rs", "pub fn dup() {}\n");
    repo.commit("init");
    init_and_sync(&repo);

    let (code, _stdout, stderr) = run(&["inspect", "dup", "--json"], repo.root());
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("nama simbol ambigu: dup (2 kandidat)"),
        "{stderr}"
    );

    let (code, stdout, stderr) = run(
        &["inspect", "dup", "--file", "src/a.rs", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["symbol"]["file"], "src/a.rs");
}

#[test]
fn inspect_missing_symbol_exits_1() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, _out, stderr) = run(&["inspect", "missing"], repo.root());
    assert_eq!(code, 1, "{stderr}");
}

#[test]
fn depth_zero_has_no_relations() {
    let repo = Repo::new();
    repo.write("src/a.rs", "fn callee() {}\nfn caller() { callee(); }\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, stdout, stderr) = run(
        &["inspect", "caller", "--depth", "0", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(value["relations"].as_array().unwrap().is_empty());

    let (code, stdout, stderr) = run(
        &["inspect", "caller", "--depth", "1", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(value["relations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|relation| relation["symbol"]["name"] == "callee"
            && relation["relation"] == "CALLS"
            && relation["incoming"] == false
            && relation["depth"] == 1));
}

#[test]
fn text_output_labels_untrusted_content() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, stdout, stderr) = run(&["query", "a", "--top-k", "1"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("UNTRUSTED REPOSITORY CONTENT"), "{stdout}");
    assert!(stdout.contains("src/a.rs"), "{stdout}");
    assert!(stdout.contains("pub fn a()"), "{stdout}");
}
