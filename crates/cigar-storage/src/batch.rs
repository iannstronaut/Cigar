//! Atomic batch application (docs/04 ?6, docs/21 ?5).
//!
//! `apply_content_batch` writes shared, immutable rows. `apply_branch_batch`
//! writes the per-branch manifest, occurrences, and edges. Both run inside one
//! `BEGIN IMMEDIATE` transaction so a crash cannot leave a half-written file.

use rusqlite::{params, OptionalExtension};

use cigar_core::domain::{
    BranchBatch, BranchFileContent, BranchOutcome, BranchState, ContentBatch, ContentFile,
    ContentOutcome, EmbeddingKey, Namespace,
};
use cigar_core::error::{EngineError, Result};

use crate::sqlite::SqliteStore;

pub(crate) fn apply_content(
    store: &SqliteStore,
    namespace: &Namespace,
    batch: &ContentBatch,
) -> Result<ContentOutcome> {
    let _ = namespace;
    let mut conn = store.conn()?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage)?;

    let mut outcome = ContentOutcome::default();
    for file in &batch.files {
        write_content_file(&tx, file, &mut outcome)?;
    }
    for (blob_hash, language) in &batch.removed_blobs {
        let referenced: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM branch_files WHERE blob_hash = ?1 AND language = ?2",
                params![blob_hash.as_str(), language.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if referenced > 0 {
            continue;
        }
        tx.execute(
            "DELETE FROM file_blobs WHERE blob_hash = ?1 AND language = ?2",
            params![blob_hash.as_str(), language.as_str()],
        )
        .map_err(storage)?;
    }

    tx.commit().map_err(storage)?;
    Ok(outcome)
}

fn write_content_file(
    tx: &rusqlite::Transaction<'_>,
    file: &ContentFile,
    outcome: &mut ContentOutcome,
) -> Result<()> {
    let inserted = tx
        .execute(
            "INSERT INTO file_blobs(blob_hash, language, imports_text, size_bytes)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(blob_hash, language) DO NOTHING",
            params![
                file.blob_hash.as_str(),
                file.language.as_str(),
                file.imports,
                file.size_bytes as i64
            ],
        )
        .map_err(storage)?;
    if inserted > 0 {
        outcome.blobs_inserted += 1;
    } else {
        let existing: String = tx
            .query_row(
                "SELECT imports_text FROM file_blobs WHERE blob_hash = ?1 AND language = ?2",
                params![file.blob_hash.as_str(), file.language.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        if existing != file.imports {
            return Err(EngineError::IdCollision {
                id: format!("{}:{}", file.blob_hash, file.language),
            });
        }
    }

    for content in &file.symbols {
        let symbol = &content.symbol;
        let inserted = tx
            .execute(
                "INSERT INTO symbol_defs(symbol_hash, language, name, qualified_name, kind,
                                         signature, docstring, code_snippet, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(symbol_hash) DO NOTHING",
                params![
                    symbol.symbol_hash.as_str(),
                    symbol.language.as_str(),
                    symbol.name,
                    symbol.qualified_name,
                    symbol.kind.as_str(),
                    symbol.signature,
                    symbol.docstring,
                    symbol.code_snippet,
                    symbol.body_hash.as_str(),
                ],
            )
            .map_err(storage)?;
        if inserted > 0 {
            outcome.symbol_defs_inserted += 1;
        } else {
            verify_symbol_def(tx, symbol)?;
        }

        tx.execute(
            "INSERT INTO blob_symbols(blob_hash, language, symbol_hash, start_line, end_line)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(blob_hash, language, symbol_hash, start_line) DO NOTHING",
            params![
                file.blob_hash.as_str(),
                file.language.as_str(),
                symbol.symbol_hash.as_str(),
                content.start_line as i64,
                content.end_line as i64
            ],
        )
        .map_err(storage)?;
    }

    for edge in &file.edges {
        let inserted = tx
            .execute(
                "INSERT INTO symbol_edges(source_symbol_hash, target_name, relation)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_symbol_hash, target_name, relation) DO NOTHING",
                params![
                    edge.source_symbol_hash.as_str(),
                    edge.target_name,
                    edge.relation.as_str()
                ],
            )
            .map_err(storage)?;
        outcome.symbol_edges_inserted += u64::from(inserted > 0);
    }

    Ok(())
}

type SymbolDefRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
);

fn verify_symbol_def(
    tx: &rusqlite::Transaction<'_>,
    symbol: &cigar_core::domain::SymbolDef,
) -> Result<()> {
    let row: Option<SymbolDefRow> = tx
        .query_row(
            "SELECT language, name, qualified_name, kind, docstring, signature, code_snippet, body_hash
             FROM symbol_defs WHERE symbol_hash = ?1",
            params![symbol.symbol_hash.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?;

    let Some((language, name, qualified_name, kind, docstring, signature, code_snippet, body_hash)) =
        row
    else {
        return Err(EngineError::IdCollision {
            id: symbol.symbol_hash.to_string(),
        });
    };
    let same = language == symbol.language.as_str()
        && name == symbol.name
        && qualified_name == symbol.qualified_name
        && kind == symbol.kind.as_str()
        && docstring == symbol.docstring
        && signature == symbol.signature
        && code_snippet == symbol.code_snippet
        && body_hash == symbol.body_hash.as_str();
    if !same {
        return Err(EngineError::IdCollision {
            id: symbol.symbol_hash.to_string(),
        });
    }
    Ok(())
}

pub(crate) fn apply_branch(
    store: &SqliteStore,
    namespace: &Namespace,
    branch: &cigar_core::domain::BranchName,
    batch: &BranchBatch,
) -> Result<BranchOutcome> {
    let mut conn = store.conn()?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage)?;

    upsert_branch_ref(&tx, namespace, branch, batch.state.as_ref())?;

    let mut outcome = BranchOutcome::default();

    for path in &batch.removed_files {
        tx.execute(
            "DELETE FROM branch_edges WHERE namespace = ?1 AND branch_name = ?2 AND file_path = ?3",
            params![namespace.as_str(), branch.as_str(), path.as_str()],
        )
        .map_err(storage)?;
        tx.execute(
            "DELETE FROM branch_symbols WHERE namespace = ?1 AND branch_name = ?2 AND file_path = ?3",
            params![namespace.as_str(), branch.as_str(), path.as_str()],
        )
        .map_err(storage)?;
        tx.execute(
            "DELETE FROM branch_files WHERE namespace = ?1 AND branch_name = ?2 AND file_path = ?3",
            params![namespace.as_str(), branch.as_str(), path.as_str()],
        )
        .map_err(storage)?;
    }

    for file in &batch.files {
        write_branch_file(&tx, namespace, branch, file)?;
        outcome.files_written += 1;
        outcome.symbols_written += file.symbols.len() as u64;
        outcome.edges_written += file.edges.len() as u64;
    }

    if let Some(state) = &batch.state {
        tx.execute(
            "UPDATE branch_refs SET commit_hash = ?1, last_sync_mode = ?2, status = ?3,
                    worktree_fingerprint = ?4, config_fingerprint = ?5, updated_at = ?6
             WHERE namespace = ?7 AND branch_name = ?8",
            params![
                state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
                state.last_sync_mode.as_str(),
                state.status.as_str(),
                state
                    .worktree_fingerprint
                    .as_ref()
                    .map(|d| d.as_str().to_owned()),
                state.config_fingerprint.as_str(),
                state.updated_at_ms,
                namespace.as_str(),
                branch.as_str(),
            ],
        )
        .map_err(storage)?;
    }

    tx.commit().map_err(storage)?;
    Ok(outcome)
}

fn write_branch_file(
    tx: &rusqlite::Transaction<'_>,
    namespace: &Namespace,
    branch: &cigar_core::domain::BranchName,
    file: &BranchFileContent,
) -> Result<()> {
    tx.execute(
        "DELETE FROM branch_edges WHERE namespace = ?1 AND branch_name = ?2 AND file_path = ?3",
        params![namespace.as_str(), branch.as_str(), file.file_path.as_str()],
    )
    .map_err(storage)?;
    tx.execute(
        "DELETE FROM branch_symbols WHERE namespace = ?1 AND branch_name = ?2 AND file_path = ?3",
        params![namespace.as_str(), branch.as_str(), file.file_path.as_str()],
    )
    .map_err(storage)?;
    tx.execute(
        "INSERT INTO branch_files(namespace, branch_name, file_path, blob_hash, language)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(namespace, branch_name, file_path)
         DO UPDATE SET blob_hash = excluded.blob_hash, language = excluded.language",
        params![
            namespace.as_str(),
            branch.as_str(),
            file.file_path.as_str(),
            file.blob_hash.as_str(),
            file.language.as_str()
        ],
    )
    .map_err(storage)?;

    let mut checked: std::collections::BTreeSet<&EmbeddingKey> = std::collections::BTreeSet::new();
    for symbol in &file.symbols {
        tx.execute(
            "INSERT INTO branch_symbols(namespace, branch_name, file_path, blob_hash, language,
                                        symbol_hash, start_line, end_line, embedding_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                namespace.as_str(),
                branch.as_str(),
                file.file_path.as_str(),
                file.blob_hash.as_str(),
                file.language.as_str(),
                symbol.symbol_hash.as_str(),
                symbol.start_line as i64,
                symbol.end_line as i64,
                symbol.embedding_key.as_str()
            ],
        )
        .map_err(storage)?;

        if checked.insert(&symbol.embedding_key)
            && !vector_exists(tx, namespace, &symbol.embedding_key)?
        {
            return Err(EngineError::storage(format!(
                "vektor untuk embedding_key {} belum ditulis; batch branch ditolak",
                symbol.embedding_key
            )));
        }
    }

    for edge in &file.edges {
        tx.execute(
            "INSERT INTO branch_edges(namespace, branch_name, file_path, source_symbol_hash,
                                      target_name, relation, target_symbol_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                namespace.as_str(),
                branch.as_str(),
                file.file_path.as_str(),
                edge.source_symbol_hash.as_str(),
                edge.target_name,
                edge.relation.as_str(),
                edge.target_symbol_hash
                    .as_ref()
                    .map(|h| h.as_str().to_owned())
            ],
        )
        .map_err(storage)?;
    }

    Ok(())
}

fn vector_exists(
    tx: &rusqlite::Transaction<'_>,
    namespace: &Namespace,
    key: &EmbeddingKey,
) -> Result<bool> {
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM symbol_vectors WHERE embedding_key = ?1 AND namespace = ?2",
            params![key.as_str(), namespace.as_str()],
            |row| row.get(0),
        )
        .map_err(storage)?;
    Ok(count > 0)
}

fn upsert_branch_ref(
    tx: &rusqlite::Transaction<'_>,
    namespace: &Namespace,
    branch: &cigar_core::domain::BranchName,
    state: Option<&BranchState>,
) -> Result<()> {
    let default_fingerprint = cigar_core::hash::config_fingerprint("unset", "unset", 1, 1, "l2")?;
    let (commit, mode, status, fingerprint, updated) = match state {
        Some(state) => (
            state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
            state.last_sync_mode.as_str(),
            state.status.as_str(),
            state.config_fingerprint.as_str().to_owned(),
            state.updated_at_ms,
        ),
        None => (
            None,
            "commit",
            "ready",
            default_fingerprint.as_str().to_owned(),
            0,
        ),
    };
    tx.execute(
        "INSERT INTO branch_refs(namespace, branch_name, commit_hash, last_sync_mode, status,
                                 config_fingerprint, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(namespace, branch_name) DO NOTHING",
        params![
            namespace.as_str(),
            branch.as_str(),
            commit,
            mode,
            status,
            fingerprint,
            updated
        ],
    )
    .map_err(storage)?;
    Ok(())
}

fn storage(err: rusqlite::Error) -> EngineError {
    EngineError::storage(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cigar_core::domain::{
        Digest, Language, ParsedSymbolContent, RepoPath, SymbolDef, SymbolHash, SymbolKind,
    };
    use cigar_core::traits::ContextStore;
    use tempfile::TempDir;

    fn open() -> (TempDir, SqliteStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let store = crate::open_or_init_store(&path, 8).unwrap();
        (dir, store)
    }

    fn symbol(name: &str) -> SymbolDef {
        let seed: u128 = name.bytes().map(u128::from).sum();
        SymbolDef {
            symbol_hash: SymbolHash::parse(format!("{seed:064x}")).unwrap(),
            language: Language::Rust,
            name: name.into(),
            qualified_name: name.into(),
            kind: SymbolKind::Function,
            signature: format!("fn {name}()"),
            docstring: None,
            code_snippet: format!("fn {name}() {{}}"),
            body_hash: Digest::parse(format!("{:064x}", 7)).unwrap(),
        }
    }

    #[test]
    fn content_batch_is_idempotent() {
        let (_dir, store) = open();
        let ns = Namespace::default();
        let sym = symbol("alpha");
        let batch = ContentBatch {
            files: vec![ContentFile {
                blob_hash: cigar_core::domain::BlobHash::parse(format!("{:064x}", 1)).unwrap(),
                language: Language::Rust,
                imports: "use std::fmt;".into(),
                size_bytes: 10,
                symbols: vec![ParsedSymbolContent {
                    symbol: sym,
                    start_line: 1,
                    end_line: 2,
                }],
                edges: vec![],
            }],
            removed_blobs: vec![],
        };
        store.apply_content_batch(&ns, batch.clone()).unwrap();
        store.apply_content_batch(&ns, batch).unwrap();
    }

    #[test]
    fn branch_batch_requires_existing_vectors() {
        let (_dir, store) = open();
        let ns = Namespace::default();
        let branch = cigar_core::domain::BranchName::parse("main").unwrap();
        let sym = symbol("beta");
        let blob = cigar_core::domain::BlobHash::parse(format!("{:064x}", 2)).unwrap();
        let content = ContentBatch {
            files: vec![ContentFile {
                blob_hash: blob.clone(),
                language: Language::Rust,
                imports: String::new(),
                size_bytes: 5,
                symbols: vec![ParsedSymbolContent {
                    symbol: sym.clone(),
                    start_line: 1,
                    end_line: 3,
                }],
                edges: vec![],
            }],
            removed_blobs: vec![],
        };
        store.apply_content_batch(&ns, content).unwrap();

        let key = cigar_core::hash::embedding_key(1, "text");
        let batch = BranchBatch {
            files: vec![BranchFileContent {
                file_path: RepoPath::parse("src/a.rs").unwrap(),
                blob_hash: blob,
                language: Language::Rust,
                symbols: vec![cigar_core::domain::BranchSymbolRow {
                    symbol_hash: sym.symbol_hash.clone(),
                    start_line: 1,
                    end_line: 3,
                    embedding_key: key,
                    embedding_text: "text".into(),
                }],
                edges: vec![],
            }],
            removed_files: vec![],
            state: None,
        };
        let error = store.apply_branch_batch(&ns, &branch, batch).unwrap_err();
        assert!(error.to_string().contains("vektor"), "{error}");
    }
}
