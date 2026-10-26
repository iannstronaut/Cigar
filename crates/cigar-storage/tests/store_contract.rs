//! Contract tests every `ContextStore` implementation must satisfy.
//!
//! These exercise the invariants that doc 04/21 require: atomic batches, vector
//! ownership, branch isolation, and referential integrity.

use cigar_core::domain::{
    BranchBatch, BranchFileContent, BranchName, BranchState, ConfigFingerprint, ContentBatch,
    ContentFile, Digest, EmbeddingKey, Language, Namespace, ParsedSymbolContent, RepoPath,
    SymbolDef, SymbolKind, SyncMode, SyncStatus,
};
use cigar_core::traits::ContextStore;
use cigar_storage::{open_or_init_store, SqliteStore};
use tempfile::TempDir;

const DIMENSIONS: usize = 8;

fn open() -> (TempDir, SqliteStore) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("store.db");
    let store = open_or_init_store(&path, DIMENSIONS).unwrap();
    (dir, store)
}

fn namespace() -> Namespace {
    Namespace::default()
}

fn branch(name: &str) -> BranchName {
    BranchName::parse(name).unwrap()
}

fn fingerprint() -> ConfigFingerprint {
    cigar_core::hash::config_fingerprint("fake", "fake-v1", DIMENSIONS, 1, "l2").unwrap()
}

fn symbol(name: &str) -> SymbolDef {
    let hash = cigar_core::hash::symbol_hash(
        Language::Rust,
        SymbolKind::Function,
        name,
        &format!("fn {name}()"),
        None,
        &format!("body-{name}"),
        1,
    );
    SymbolDef {
        symbol_hash: hash,
        language: Language::Rust,
        name: name.into(),
        qualified_name: name.into(),
        kind: SymbolKind::Function,
        signature: format!("fn {name}()"),
        docstring: None,
        code_snippet: format!("fn {name}() {{\n    let x = {name};\n}}"),
        body_hash: Digest::parse(format!("{:064x}", 42)).unwrap(),
    }
}

fn content(blob_seed: u8, symbols: &[SymbolDef], imports: &str) -> ContentFile {
    ContentFile {
        blob_hash: cigar_core::domain::BlobHash::parse(format!("{blob_seed:064x}")).unwrap(),
        language: Language::Rust,
        imports: imports.into(),
        size_bytes: 128,
        symbols: symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| ParsedSymbolContent {
                symbol: symbol.clone(),
                start_line: 1 + index as u32 * 5,
                end_line: 4 + index as u32 * 5,
            })
            .collect(),
        edges: vec![],
    }
}

fn vector(seed: f32) -> Vec<f32> {
    let mut out = vec![0.0; DIMENSIONS];
    out[0] = seed;
    out[1] = (1.0 - seed * seed).max(0.0).sqrt();
    out
}

fn key_for(text: &str) -> EmbeddingKey {
    cigar_core::hash::embedding_key(1, text)
}

#[test]
fn content_batch_is_atomic_and_idempotent() {
    let (_dir, store) = open();
    let ns = namespace();
    let alpha = symbol("alpha");
    let beta = symbol("beta");
    let batch = ContentBatch {
        files: vec![content(1, &[alpha.clone(), beta.clone()], "use std::fmt;")],
        removed_blobs: vec![],
    };

    store.apply_content_batch(&ns, batch.clone()).unwrap();
    store.apply_content_batch(&ns, batch).unwrap();

    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 1)).unwrap();
    let symbols = store.blob_symbols(&blob, Language::Rust).unwrap();
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[0].imports, "use std::fmt;");
}

#[test]
fn branch_batch_rejects_missing_vectors() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let alpha = symbol("alpha");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 2)).unwrap();

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(2, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();

    let key = key_for("alpha-template");
    let batch = BranchBatch {
        files: vec![BranchFileContent {
            file_path: RepoPath::parse("src/lib.rs").unwrap(),
            blob_hash: blob.clone(),
            language: Language::Rust,
            symbols: vec![cigar_core::domain::BranchSymbolRow {
                symbol_hash: alpha.symbol_hash.clone(),
                start_line: 1,
                end_line: 4,
                embedding_key: key,
                embedding_text: "alpha-template".into(),
            }],
            edges: vec![],
        }],
        removed_files: vec![],
        state: None,
    };
    let error = store.apply_branch_batch(&ns, &main, batch).unwrap_err();
    assert!(
        error.to_string().contains("vektor"),
        "expected vector guard, got {error}"
    );
    assert!(store.get_branch_manifest(&ns, &main).unwrap().is_empty());
}

#[test]
fn branch_batch_is_atomic_with_vectors() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let alpha = symbol("alpha");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 3)).unwrap();
    let key = key_for("alpha-template");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(3, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
        .unwrap();

    let batch = BranchBatch {
        files: vec![BranchFileContent {
            file_path: RepoPath::parse("src/lib.rs").unwrap(),
            blob_hash: blob,
            language: Language::Rust,
            symbols: vec![cigar_core::domain::BranchSymbolRow {
                symbol_hash: alpha.symbol_hash.clone(),
                start_line: 1,
                end_line: 4,
                embedding_key: key,
                embedding_text: "alpha-template".into(),
            }],
            edges: vec![],
        }],
        removed_files: vec![],
        state: Some(BranchState {
            namespace: ns.clone(),
            branch: main.clone(),
            commit_hash: None,
            last_sync_mode: SyncMode::Commit,
            status: SyncStatus::Ready,
            worktree_fingerprint: None,
            config_fingerprint: fingerprint(),
            updated_at_ms: 1,
        }),
    };
    let outcome = store.apply_branch_batch(&ns, &main, batch).unwrap();
    assert_eq!(outcome.files_written, 1);
    assert_eq!(store.get_branch_manifest(&ns, &main).unwrap().len(), 1);
}

#[test]
fn search_is_scoped_to_branch() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let feature = branch("feat/auth");

    let alpha = symbol("alpha");
    let beta = symbol("beta");
    let blob_main = cigar_core::domain::BlobHash::parse(format!("{:064x}", 4)).unwrap();
    let blob_feature = cigar_core::domain::BlobHash::parse(format!("{:064x}", 5)).unwrap();
    let key_alpha = key_for("alpha");
    let key_beta = key_for("beta");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![
                    content(4, std::slice::from_ref(&alpha), ""),
                    content(5, std::slice::from_ref(&beta), ""),
                ],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(
            &ns,
            &[
                (key_alpha.clone(), vector(1.0)),
                (key_beta.clone(), vector(0.0)),
            ],
        )
        .unwrap();

    let state = |branch: &BranchName| BranchState {
        namespace: ns.clone(),
        branch: branch.clone(),
        commit_hash: None,
        last_sync_mode: SyncMode::Commit,
        status: SyncStatus::Ready,
        worktree_fingerprint: None,
        config_fingerprint: fingerprint(),
        updated_at_ms: 1,
    };

    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/main.rs").unwrap(),
                    blob_hash: blob_main,
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: alpha.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key_alpha.clone(),
                        embedding_text: "alpha".into(),
                    }],
                    edges: vec![],
                }],
                removed_files: vec![],
                state: Some(state(&main)),
            },
        )
        .unwrap();
    store
        .apply_branch_batch(
            &ns,
            &feature,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/feature.rs").unwrap(),
                    blob_hash: blob_feature,
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: beta.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key_beta.clone(),
                        embedding_text: "beta".into(),
                    }],
                    edges: vec![],
                }],
                removed_files: vec![],
                state: Some(state(&feature)),
            },
        )
        .unwrap();

    let main_hits = store.search_symbols(&ns, &main, &vector(1.0), 5).unwrap();
    assert_eq!(main_hits.len(), 1);
    assert_eq!(main_hits[0].file_path.as_str(), "src/main.rs");
    assert_eq!(main_hits[0].symbol_hash, alpha.symbol_hash);

    let feature_hits = store
        .search_symbols(&ns, &feature, &vector(1.0), 5)
        .unwrap();
    assert_eq!(feature_hits.len(), 1);
    assert_eq!(feature_hits[0].file_path.as_str(), "src/feature.rs");
    assert_eq!(feature_hits[0].symbol_hash, beta.symbol_hash);
}

#[test]
fn removing_file_cascades_branch_rows_only() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let alpha = symbol("alpha");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 6)).unwrap();
    let key = key_for("alpha");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(6, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
        .unwrap();
    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/lib.rs").unwrap(),
                    blob_hash: blob.clone(),
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: alpha.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key,
                        embedding_text: "alpha".into(),
                    }],
                    edges: vec![],
                }],
                removed_files: vec![],
                state: None,
            },
        )
        .unwrap();

    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![],
                removed_files: vec![RepoPath::parse("src/lib.rs").unwrap()],
                state: None,
            },
        )
        .unwrap();

    assert!(store.get_branch_manifest(&ns, &main).unwrap().is_empty());
    assert_eq!(store.blob_symbols(&blob, Language::Rust).unwrap().len(), 1);
}

#[test]
fn removing_blob_is_refused_while_referenced() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let alpha = symbol("alpha");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 7)).unwrap();
    let key = key_for("alpha");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(7, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
        .unwrap();
    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/lib.rs").unwrap(),
                    blob_hash: blob.clone(),
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: alpha.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key,
                        embedding_text: "alpha".into(),
                    }],
                    edges: vec![],
                }],
                removed_files: vec![],
                state: None,
            },
        )
        .unwrap();

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![],
                removed_blobs: vec![(blob.clone(), Language::Rust)],
            },
        )
        .unwrap();
    assert_eq!(store.blob_symbols(&blob, Language::Rust).unwrap().len(), 1);
}

#[test]
fn health_check_reports_clean_store() {
    let (_dir, store) = open();
    let health = store.health_check().unwrap();
    assert!(health.ok, "{health:?}");
    assert!(health.quick_check_ok);
    assert_eq!(health.foreign_key_violations, 0);
}

#[test]
fn schema_version_and_branch_state_round_trip() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    assert_eq!(store.schema_version().unwrap(), 2);
    assert!(store.get_branch_state(&ns, &main).unwrap().is_none());

    let state = BranchState {
        namespace: ns.clone(),
        branch: main.clone(),
        commit_hash: Some(cigar_core::domain::CommitId::parse("a".repeat(40)).unwrap()),
        last_sync_mode: SyncMode::Dirty,
        status: SyncStatus::Dirty,
        worktree_fingerprint: Some(Digest::parse("b".repeat(64)).unwrap()),
        config_fingerprint: fingerprint(),
        updated_at_ms: 1_700_000_000_000,
    };
    store.set_branch_state(&ns, &state).unwrap();
    assert_eq!(store.get_branch_state(&ns, &main).unwrap(), Some(state));
    assert_eq!(store.list_branches(&ns).unwrap().len(), 1);
}

#[test]
fn vector_keys_round_trip() {
    let (_dir, store) = open();
    let ns = namespace();
    let key = key_for("vector-a");
    let other = key_for("vector-b");
    assert_eq!(
        store
            .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
            .unwrap(),
        0
    );
    let existing = store
        .existing_vector_keys(&ns, &[key.clone(), other])
        .unwrap();
    assert_eq!(existing, vec![true, false]);
}

#[test]
fn dimension_mismatch_is_rejected() {
    let (_dir, store) = open();
    let ns = namespace();
    let error = store
        .ensure_vectors(&ns, &[(key_for("bad"), vec![0.0; 3])])
        .unwrap_err();
    assert!(matches!(
        error,
        cigar_core::EngineError::DimensionMismatch { .. }
    ));
    let error = store
        .search_symbols(&ns, &branch("main"), &[0.0; 3], 5)
        .unwrap_err();
    assert!(matches!(
        error,
        cigar_core::EngineError::DimensionMismatch { .. }
    ));
}

#[test]
fn symbol_hash_collision_is_detected() {
    let (_dir, store) = open();
    let ns = namespace();
    let mut alpha = symbol("alpha");
    let original = alpha.clone();
    alpha.code_snippet = "fn alpha() { different(); }".into();

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(8, std::slice::from_ref(&original), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    let error = store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(9, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap_err();
    assert!(matches!(error, cigar_core::EngineError::IdCollision { .. }));
}

#[test]
fn edges_resolve_and_backfill() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let caller = symbol("caller");
    let callee = symbol("callee");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 10)).unwrap();
    let key = key_for("caller");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![
                    content(10, std::slice::from_ref(&caller), ""),
                    content(12, std::slice::from_ref(&callee), ""),
                ],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
        .unwrap();

    let edge = cigar_core::domain::BranchEdgeRow {
        source_symbol_hash: caller.symbol_hash.clone(),
        target_name: "callee".into(),
        relation: cigar_core::domain::EdgeRelation::Calls,
        target_symbol_hash: None,
    };
    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/lib.rs").unwrap(),
                    blob_hash: blob,
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: caller.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key,
                        embedding_text: "caller".into(),
                    }],
                    edges: vec![edge],
                }],
                removed_files: vec![],
                state: None,
            },
        )
        .unwrap();

    let unresolved = store
        .unresolved_edges_for_name(&ns, &main, "callee", 10)
        .unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].source_symbol_hash, caller.symbol_hash);

    store
        .resolve_edge(&ns, &main, &unresolved[0], &callee.symbol_hash)
        .unwrap();
    assert!(store
        .unresolved_edges_for_name(&ns, &main, "callee", 10)
        .unwrap()
        .is_empty());
}

#[test]
fn stats_counts_rows() {
    let (_dir, store) = open();
    let ns = namespace();
    let main = branch("main");
    let alpha = symbol("alpha");
    let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 11)).unwrap();
    let key = key_for("alpha");

    store
        .apply_content_batch(
            &ns,
            ContentBatch {
                files: vec![content(11, std::slice::from_ref(&alpha), "")],
                removed_blobs: vec![],
            },
        )
        .unwrap();
    store
        .ensure_vectors(&ns, &[(key.clone(), vector(1.0))])
        .unwrap();
    store
        .apply_branch_batch(
            &ns,
            &main,
            BranchBatch {
                files: vec![BranchFileContent {
                    file_path: RepoPath::parse("src/lib.rs").unwrap(),
                    blob_hash: blob,
                    language: Language::Rust,
                    symbols: vec![cigar_core::domain::BranchSymbolRow {
                        symbol_hash: alpha.symbol_hash.clone(),
                        start_line: 1,
                        end_line: 4,
                        embedding_key: key,
                        embedding_text: "alpha".into(),
                    }],
                    edges: vec![cigar_core::domain::BranchEdgeRow {
                        source_symbol_hash: alpha.symbol_hash.clone(),
                        target_name: "missing".into(),
                        relation: cigar_core::domain::EdgeRelation::Calls,
                        target_symbol_hash: None,
                    }],
                }],
                removed_files: vec![],
                state: None,
            },
        )
        .unwrap();

    let stats = store.stats(&ns, &main).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(stats.symbols, 1);
    assert_eq!(stats.vectors, 1);
    assert_eq!(stats.unresolved_edges, 1);
}
