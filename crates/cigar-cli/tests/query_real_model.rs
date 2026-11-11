// SPDX-License-Identifier: MPL-2.0

//! Semantic relevance test against a real ONNX model.
//!
//! Ignored by default; requires a downloaded model and ONNX Runtime:
//!
//! ```text
//! ORT_DYLIB_PATH=... CIGAR_TEST_MODEL_DIR=... \
//!   cargo test -p cigar-cli --test query_real_model -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn cigar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cigar"))
}

fn model_available() -> bool {
    let Some(root) = std::env::var_os("CIGAR_TEST_MODEL_DIR") else {
        return false;
    };
    let manifest = cigar_embed::ModelManifest::granite_125m();
    manifest.is_ready(Path::new(&root))
}

struct Repo {
    _dir: TempDir,
    root: PathBuf,
}

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .expect("git tersedia");
    assert!(status.success(), "git {args:?} gagal");
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
        let output = cigar()
            .args(args)
            .arg("--cwd")
            .arg(&self.root)
            .current_dir(&self.root)
            .output()
            .expect("cigar berjalan");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

#[test]
#[ignore = "butuh model ONNX nyata"]
fn semantic_ranking_matches_intent() {
    if !model_available() {
        eprintln!("model tidak tersedia; lewati");
        return;
    }
    let repo = Repo::new();
    repo.write(
        "src/auth/jwt.rs",
        "/// Decode a JWT and verify its expiry claim.\npub fn decode_jwt(token: &str) -> Result<Claims, Error> {\n    verify_expiry(token)\n}\n\nfn verify_expiry(_token: &str) -> Result<Claims, Error> {\n    todo!()\n}\n",
    );
    repo.write(
        "src/ui/theme.rs",
        "/// Pick the accent colour for a button.\npub fn button_accent() -> &'static str {\n    \"#4a90d9\"\n}\n",
    );
    repo.write(
        "src/db/pool.rs",
        "/// Open a database connection pool.\npub fn open_pool(url: &str) -> Pool {\n    Pool::new(url)\n}\n",
    );
    repo.commit("init");

    let (code, _out, err) = repo.run(&["init", "--no-model"]);
    assert_eq!(code, 0, "{err}");
    let (code, _out, err) = repo.run(&["sync"]);
    assert_eq!(code, 0, "{err}");

    let (code, stdout, stderr) =
        repo.run(&["query", "jwt expiry validation", "--json", "--top-k", "3"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = value["results"].as_array().unwrap();
    assert!(!results.is_empty(), "tidak ada hasil");
    let top = results[0]["symbol"]["qualified_name"].as_str().unwrap();
    println!("top result: {top}");
    assert!(
        top.contains("decode_jwt") || top.contains("verify_expiry"),
        "top result bukan kode JWT: {top}"
    );

    let (code, stdout, stderr) =
        repo.run(&["query", "button colour theme", "--json", "--top-k", "1"]);
    assert_eq!(code, 0, "{stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let top = value["results"][0]["symbol"]["qualified_name"]
        .as_str()
        .unwrap();
    println!("theme result: {top}");
    assert!(top.contains("button_accent"), "{top}");
}
