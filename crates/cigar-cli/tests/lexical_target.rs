// SPDX-License-Identifier: MPL-2.0

//! End-to-end lexical index contract without a model or network.
use std::process::Command;
use tempfile::TempDir;

fn run(root: &std::path::Path, args: &[&str]) -> (i32, serde_json::Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_cigar"))
        .args(args)
        .arg("--cwd")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        output.status.code().unwrap(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn offline_init_sync_query_inspect_and_reindex() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn validate_token() { helper(); }\nfn helper() {}\n",
    )
    .unwrap();
    let (_, init) = run(
        dir.path(),
        &["init", "--git-mode", "fs-only", "--offline", "--json"],
    );
    assert_eq!(init["search"]["mode"], "lexical-graph");
    assert!(init.get("embedding").is_none());
    let (_, synced) = run(dir.path(), &["sync", "--json"]);
    assert_eq!(synced["branches"][0]["symbols_written"], 2);
    let (_, result) = run(dir.path(), &["query", "validate_token", "--json"]);
    assert_eq!(result["results"][0]["symbol"]["name"], "validate_token");
    let (_, inspected) = run(dir.path(), &["inspect", "validate_token", "--json"]);
    assert_eq!(inspected["symbol"]["name"], "validate_token");
    let (_, status) = run(dir.path(), &["status", "--json"]);
    assert_eq!(status["stats"]["symbols"], 2);
    assert!(status.get("model").is_none());
    let (_, rebuilt) = run(dir.path(), &["reindex", "--yes", "--no-backup", "--json"]);
    assert_eq!(rebuilt["symbols"], 2);
}
