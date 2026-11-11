// SPDX-License-Identifier: MPL-2.0

use cigar_storage::{TargetBatch, TargetEdge, TargetFile, TargetStore, TargetSymbol};

fn file(id: &str, path: &str, name: &str, imports: &str, calls: &[&str]) -> TargetFile {
    TargetFile {
        file_id: id.into(),
        file_path: path.into(),
        language: "rust".into(),
        blob_hash: "blob".into(),
        imports_text: imports.into(),
        symbols: vec![TargetSymbol {
            symbol_id: format!("{id}-symbol"),
            name: name.into(),
            qualified_name: name.into(),
            tokenized_name: name.into(),
            kind: "function".into(),
            signature: format!("fn {name}()"),
            code_snippet: name.into(),
            code_tokens: name.into(),
            start_line: 1,
            end_line: 1,
            body_hash: "body".into(),
            edges: calls
                .iter()
                .map(|target| TargetEdge {
                    target_name: (*target).into(),
                    target_symbol_id: None,
                    relation: "CALLS".into(),
                })
                .collect(),
            ..TargetSymbol::default()
        }],
    }
}

#[test]
fn imports_disambiguate_and_relink_after_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
    let mut batch = TargetBatch {
        namespace: "main".into(),
        commit_hash: "one".into(),
        files: vec![
            file(
                "caller",
                "src/caller.rs",
                "caller",
                "use crate::alpha::target;",
                &["target"],
            ),
            file("other", "src/other.rs", "other", "", &["target"]),
            file("alpha", "src/alpha.rs", "target", "", &[]),
            file("beta", "src/beta.rs", "target", "", &[]),
        ],
        ..TargetBatch::default()
    };
    store.apply_batch(&batch).unwrap();
    let neighbors = store.neighbors("main", "caller-symbol").unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].symbol.symbol_id, "alpha-symbol");
    assert!(store.neighbors("main", "other-symbol").unwrap().is_empty());
    let alpha = store
        .find_symbols("main", "target")
        .unwrap()
        .into_iter()
        .find(|s| s.symbol_id == "alpha-symbol")
        .unwrap();
    assert_eq!(alpha.in_degree, 1);

    batch.commit_hash = "two".into();
    batch.files.clear();
    batch.deleted_paths = vec!["src/alpha.rs".into()];
    store.apply_batch(&batch).unwrap();
    assert!(store.neighbors("main", "caller-symbol").unwrap().is_empty());
    assert_eq!(
        store.neighbors("main", "other-symbol").unwrap()[0]
            .symbol
            .symbol_id,
        "beta-symbol"
    );
    assert_eq!(
        store.find_symbols("main", "target").unwrap()[0].in_degree,
        1
    );
}

#[test]
fn aliases_and_ambiguous_module_hints_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
    let batch = TargetBatch {
        namespace: "main".into(),
        commit_hash: "one".into(),
        files: vec![
            file(
                "caller",
                "src/caller.py",
                "caller",
                "from pkg.alpha import target as chosen",
                &["chosen"],
            ),
            file("alpha", "pkg/alpha.py", "target", "", &[]),
            file("beta", "pkg/beta.py", "target", "", &[]),
            file(
                "ambiguous",
                "src/ambiguous.rs",
                "ambiguous",
                "use crate::alpha::target;",
                &["target"],
            ),
            file("duplicate", "other/alpha.rs", "target", "", &[]),
            file("rust", "src/alpha.rs", "target", "", &[]),
        ],
        ..TargetBatch::default()
    };
    store.apply_batch(&batch).unwrap();
    assert_eq!(
        store.neighbors("main", "caller-symbol").unwrap()[0]
            .symbol
            .symbol_id,
        "alpha-symbol"
    );
    assert!(store
        .neighbors("main", "ambiguous-symbol")
        .unwrap()
        .is_empty());
}
