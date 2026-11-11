// SPDX-License-Identifier: MPL-2.0

//! Integration contracts for the namespace-scoped target index.

use cigar_storage::{TargetBatch, TargetEdge, TargetFile, TargetStore, TargetSymbol};
use rusqlite::Connection;
use tempfile::TempDir;

fn open() -> (TempDir, TargetStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = TargetStore::open(&dir.path().join("index.sqlite"), true).unwrap();
    (dir, store)
}

fn symbol(id: &str, name: &str, tokens: &str) -> TargetSymbol {
    TargetSymbol {
        symbol_id: id.into(),
        name: name.into(),
        qualified_name: format!("module::{name}"),
        tokenized_name: tokens.into(),
        kind: "function".into(),
        signature: format!("fn {name}()"),
        code_snippet: format!("fn {name}() {{}}"),
        code_tokens: tokens.into(),
        start_line: 1,
        end_line: 2,
        body_hash: format!("body-{id}"),
        ..Default::default()
    }
}

fn file(id: &str, path: &str, symbols: Vec<TargetSymbol>) -> TargetFile {
    TargetFile {
        file_id: id.into(),
        file_path: path.into(),
        language: "rust".into(),
        blob_hash: format!("blob-{id}"),
        imports_text: "use module;".into(),
        symbols,
    }
}

fn batch(namespace: &str, commit: &str, files: Vec<TargetFile>) -> TargetBatch {
    TargetBatch {
        namespace: namespace.into(),
        commit_hash: commit.into(),
        updated_at: 123,
        files,
        ..Default::default()
    }
}

#[test]
fn initializes_v4_schema_and_reopens_existing_store() {
    let (dir, store) = open();
    assert!(store.health_check().unwrap().is_empty());
    drop(store);

    let path = dir.path().join("index.sqlite");
    let db = Connection::open(&path).unwrap();
    let version: String = db
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "4");
    for table in ["sync_state", "files", "symbols", "edges", "symbols_fts"] {
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "missing {table}");
    }
    drop(db);
    let reopened = TargetStore::open(&path, false).unwrap();
    assert_eq!(reopened.stats("main").unwrap().symbols, 0);
    assert!(reopened.health_check().unwrap().is_empty());
}

#[test]
fn namespaces_isolate_search_names_files_graph_and_sync_state() {
    let (_dir, store) = open();
    let mut source = symbol("a-source", "caller", "shared caller");
    source.edges.push(TargetEdge {
        target_name: "callee".into(),
        relation: "calls".into(),
        ..Default::default()
    });
    store
        .apply_batch(&batch(
            "a",
            "a1",
            vec![file(
                "a-file",
                "src/lib.rs",
                vec![source, symbol("a-target", "callee", "shared callee")],
            )],
        ))
        .unwrap();
    store
        .apply_batch(&batch(
            "b",
            "b1",
            vec![file(
                "b-file",
                "src/lib.rs",
                vec![symbol("b-target", "callee", "shared callee")],
            )],
        ))
        .unwrap();

    assert_eq!(store.search("a", "shared", 10).unwrap().len(), 2);
    assert_eq!(
        store.search("b", "shared", 10).unwrap()[0].symbol_id,
        "b-target"
    );
    assert_eq!(
        store.find_symbols("a", "callee").unwrap()[0].symbol_id,
        "a-target"
    );
    assert_eq!(
        store.find_symbols("b", "module::callee").unwrap()[0].symbol_id,
        "b-target"
    );
    assert!(store.neighbors("b", "a-source").unwrap().is_empty());
    assert_eq!(
        store.neighbors("a", "a-source").unwrap()[0]
            .symbol
            .symbol_id,
        "a-target"
    );
    assert_eq!(store.stats("a").unwrap().edges, 1);
    assert_eq!(store.stats("b").unwrap().edges, 0);
    assert_eq!(store.files("a").unwrap()[0].file_path, "src/lib.rs");
    assert_eq!(store.sync_head("a").unwrap().as_deref(), Some("a1"));
    assert_eq!(store.sync_head("b").unwrap().as_deref(), Some("b1"));

    let mut delete = batch("a", "a2", vec![]);
    delete.deleted_paths.push("src/lib.rs".into());
    store.apply_batch(&delete).unwrap();
    assert!(store.search("a", "shared", 10).unwrap().is_empty());
    assert_eq!(store.stats("a").unwrap().files, 0);
    assert_eq!(store.search("b", "shared", 10).unwrap().len(), 1);
    assert_eq!(store.sync_head("b").unwrap().as_deref(), Some("b1"));
}

#[test]
fn replacing_and_deleting_files_updates_fts_and_metadata() {
    let (_dir, store) = open();
    store
        .apply_batch(&batch(
            "main",
            "one",
            vec![file(
                "file",
                "lib.rs",
                vec![symbol("old", "old_name", "obsoleteuniquetoken")],
            )],
        ))
        .unwrap();
    assert_eq!(
        store
            .search("main", "obsoleteuniquetoken", 10)
            .unwrap()
            .len(),
        1
    );

    store
        .apply_batch(&batch(
            "main",
            "two",
            vec![file(
                "file",
                "lib.rs",
                vec![symbol("new", "new_name", "freshuniquetoken")],
            )],
        ))
        .unwrap();
    assert!(store
        .search("main", "obsoleteuniquetoken", 10)
        .unwrap()
        .is_empty());
    assert!(store.find_symbols("main", "old_name").unwrap().is_empty());
    let hits = store.search("main", "freshuniquetoken", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].symbol_id, "new");
    assert_eq!(hits[0].file_path, "lib.rs");
    assert_eq!(hits[0].imports_text, "use module;");
    assert_eq!(store.stats("main").unwrap().symbols, 1);

    let mut delete = batch("main", "three", vec![]);
    delete.deleted_paths.push("lib.rs".into());
    store.apply_batch(&delete).unwrap();
    assert!(store
        .search("main", "freshuniquetoken", 10)
        .unwrap()
        .is_empty());
    assert!(store.find_symbols("main", "new_name").unwrap().is_empty());
    assert_eq!(store.stats("main").unwrap().files, 0);
    assert_eq!(store.sync_head("main").unwrap().as_deref(), Some("three"));
    assert!(store.health_check().unwrap().is_empty());
}

#[test]
fn search_ties_are_ordered_by_id_before_limit() {
    let (_dir, store) = open();
    let mut first = symbol("z-id", "same", "identicaltoken");
    let mut second = first.clone();
    second.symbol_id = "a-id".into();
    first.body_hash = "same-body".into();
    second.body_hash = "same-body".into();
    store
        .apply_batch(&batch(
            "main",
            "one",
            vec![file("file", "lib.rs", vec![first, second])],
        ))
        .unwrap();
    let hits = store.search("main", "identicaltoken", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].score, hits[1].score);
    assert_eq!(
        hits.iter()
            .map(|hit| hit.symbol_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-id", "z-id"]
    );
    assert_eq!(
        store.search("main", "identicaltoken", 1).unwrap()[0].symbol_id,
        "a-id"
    );
    assert!(store
        .search("main", "identicaltoken", 0)
        .unwrap()
        .is_empty());
    assert!(store.search("main", "  ", 10).unwrap().is_empty());
}

#[test]
fn graph_links_forward_references_and_relinks_after_ambiguity_and_deletion() {
    let (_dir, store) = open();
    let mut caller = symbol("caller", "caller", "caller");
    caller.edges.push(TargetEdge {
        target_name: "callee".into(),
        relation: "calls".into(),
        ..Default::default()
    });
    store
        .apply_batch(&batch(
            "main",
            "one",
            vec![file("source-file", "source.rs", vec![caller])],
        ))
        .unwrap();
    assert!(store.neighbors("main", "caller").unwrap().is_empty());
    assert_eq!(store.stats("main").unwrap().edges, 1);

    store
        .apply_batch(&batch(
            "main",
            "two",
            vec![file(
                "target-file",
                "target.rs",
                vec![symbol("target", "callee", "callee")],
            )],
        ))
        .unwrap();
    let outgoing = store.neighbors("main", "caller").unwrap();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].symbol.symbol_id, "target");
    assert_eq!(outgoing[0].relation, "calls");
    assert!(!outgoing[0].incoming);
    let incoming = store.neighbors("main", "target").unwrap();
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].symbol.symbol_id, "caller");
    assert!(incoming[0].incoming);
    assert_eq!(
        store.find_symbols("main", "callee").unwrap()[0].in_degree,
        1
    );

    store
        .apply_batch(&batch(
            "main",
            "three",
            vec![file(
                "duplicate-file",
                "duplicate.rs",
                vec![symbol("duplicate", "callee", "callee")],
            )],
        ))
        .unwrap();
    assert!(store.neighbors("main", "caller").unwrap().is_empty());
    assert!(store
        .find_symbols("main", "callee")
        .unwrap()
        .iter()
        .all(|hit| hit.in_degree == 0));
    let mut delete = batch("main", "four", vec![]);
    delete.deleted_paths.push("duplicate.rs".into());
    store.apply_batch(&delete).unwrap();
    assert_eq!(
        store.neighbors("main", "caller").unwrap()[0]
            .symbol
            .symbol_id,
        "target"
    );
    assert_eq!(
        store.find_symbols("main", "callee").unwrap()[0].in_degree,
        1
    );
    assert!(store.health_check().unwrap().is_empty());
}

#[test]
fn failed_batch_rolls_back_deletes_inserts_fts_and_sync_head() {
    let (_dir, store) = open();
    store
        .apply_batch(&batch(
            "main",
            "before",
            vec![file(
                "original-file",
                "original.rs",
                vec![symbol("original", "original", "originaltoken")],
            )],
        ))
        .unwrap();
    let duplicate = symbol("duplicate", "duplicate", "newtoken");
    let mut invalid = batch(
        "main",
        "after",
        vec![file(
            "new-file",
            "new.rs",
            vec![duplicate.clone(), duplicate],
        )],
    );
    invalid.deleted_paths.push("original.rs".into());
    assert!(store.apply_batch(&invalid).is_err());
    assert_eq!(store.sync_head("main").unwrap().as_deref(), Some("before"));
    assert_eq!(store.files("main").unwrap()[0].file_path, "original.rs");
    assert_eq!(
        store.search("main", "originaltoken", 10).unwrap()[0].symbol_id,
        "original"
    );
    assert!(store.search("main", "newtoken", 10).unwrap().is_empty());
    let stats = store.stats("main").unwrap();
    assert_eq!((stats.files, stats.symbols, stats.edges), (1, 1, 0));
    assert!(store.health_check().unwrap().is_empty());
}

#[test]
fn health_check_detects_missing_fts_entry() {
    let (dir, store) = open();
    store
        .apply_batch(&batch(
            "main",
            "one",
            vec![file(
                "file",
                "lib.rs",
                vec![symbol("symbol", "name", "searchable")],
            )],
        ))
        .unwrap();
    assert!(store.health_check().unwrap().is_empty());
    let db = Connection::open(dir.path().join("index.sqlite")).unwrap();
    db.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", ["symbol"])
        .unwrap();
    let problems = store.health_check().unwrap();
    assert!(problems
        .iter()
        .any(|problem| problem.contains("symbols_fts mismatch")));
}
