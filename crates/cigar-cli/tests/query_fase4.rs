//! Fase 4 tests: query ranking, budget, branch isolation, graph expansion,
//! inspect resolution and ambiguity.

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

/// Uses the fake embedder: deterministic, offline, fast.
fn init_and_sync(repo: &Repo) {
    let (code, _out, err) = run(
        &[
            "init",
            "--no-model",
            "--embedding",
            "fake",
            "--dimensions",
            "64",
        ],
        repo.root(),
    );
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
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["index"]["branch"], "main");
    assert_eq!(value["index"]["stale"], false);
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty());
    // The fake embedder is deterministic but not semantic, so this asserts
    // structure and ranking invariants rather than semantic relevance.
    let first = &results[0];
    assert_eq!(first["rank"], 1);
    assert!(first["symbol"]["file"]
        .as_str()
        .unwrap()
        .starts_with("src/"));
    assert!(first["score"].as_f64().unwrap() > 0.0);
    assert!(first["symbol"]["signature"].is_string());
    // Relations are present as arrays even when empty.
    assert!(first["relations"]["callers"].is_array());
}

#[test]
fn query_honours_token_budget() {
    let repo = Repo::new();
    repo.write(
        "src/big.rs",
        &(0..60)
            .map(|i| format!("pub fn function_{i}() -> i32 {{ {i} }}\n"))
            .collect::<String>(),
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
    let used = value["stats"]["tokens_estimated"].as_u64().unwrap();
    assert!(used <= 300, "tokens {used} melebihi budget");
    assert!(!value["results"].as_array().unwrap().is_empty());
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
    for result in value["results"].as_array().unwrap() {
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
    for result in value["results"].as_array().unwrap() {
        assert_eq!(result["symbol"]["kind"], "struct");
    }
}

#[test]
fn branch_isolation_holds_for_queries() {
    let repo = Repo::new();
    repo.write("src/shared.rs", "pub fn shared_helper() {}\n");
    repo.commit("init");
    init_and_sync(&repo);

    repo.checkout("feat/only", true);
    repo.write("src/feature.rs", "pub fn feature_only_marker() {}\n");
    repo.commit("feature");
    let (code, _out, err) = run(&["sync"], repo.root());
    assert_eq!(code, 0, "{err}");

    // On the feature branch the feature symbol is visible.
    let (code, stdout, stderr) = run(
        &["query", "feature only marker", "--json", "--top-k", "3"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["index"]["branch"], "feat/only");
    assert!(value["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["symbol"]["file"] == "src/feature.rs"));

    // The main branch must not contain it.
    let (code, stdout, stderr) = run(
        &["query", "feature only marker", "--branch", "main", "--json"],
        repo.root(),
    );
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(!value["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|result| result["symbol"]["file"] == "src/feature.rs"));
}

#[test]
fn query_on_unindexed_branch_is_actionable() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, _out, stderr) = run(&["query", "a", "--branch", "nope"], repo.root());
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("cigar sync --branch nope"), "{stderr}");
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
    assert_eq!(value["symbol"]["qualified_name"], "S::run");
    assert_eq!(value["symbol"]["kind"], "method");
    let callees = value["relations"]["callees"].as_array().unwrap();
    assert!(callees
        .iter()
        .any(|view| view["qualified_name"] == "S::helper"));
}

#[test]
fn inspect_ambiguous_lists_candidates_and_exits_1() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn dup() {}\n");
    repo.write("src/b.rs", "pub fn dup() {}\n");
    repo.commit("init");
    init_and_sync(&repo);

    let (code, stdout, stderr) = run(&["inspect", "dup", "--json"], repo.root());
    assert_eq!(code, 1, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ambiguous"], true);
    assert_eq!(value["count"], 2);
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
    assert!(value["relations"]["callees"].as_array().unwrap().is_empty());
}

#[test]
fn text_output_labels_untrusted_content() {
    let repo = Repo::new();
    repo.write("src/a.rs", "pub fn a() {}\n");
    repo.commit("init");
    init_and_sync(&repo);
    let (code, stdout, stderr) = run(&["query", "a", "--top-k", "1"], repo.root());
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("# CIGAR context"), "{stdout}");
    assert!(stdout.contains("query: \"a\""), "{stdout}");
    assert!(stdout.contains("UNTRUSTED REPOSITORY CONTENT"), "{stdout}");
    assert!(stdout.contains("```rust"), "{stdout}");
    assert!(stdout.contains("index: branch=main"), "{stdout}");
}
