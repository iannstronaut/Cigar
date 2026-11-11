// SPDX-License-Identifier: MPL-2.0

//! Namespace-scoped lexical index and atomic sync mutations.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use cigar_core::domain::TargetFileState;
pub use cigar_core::domain::{TargetBatch, TargetEdge, TargetFile, TargetSymbol};
use cigar_core::error::{EngineError, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

const SCHEMA_VERSION: u32 = 4;
const SCHEMA: &str = r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sync_state (namespace TEXT PRIMARY KEY, last_commit_hash TEXT NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE files (
    file_id TEXT PRIMARY KEY, namespace TEXT NOT NULL, file_path TEXT NOT NULL,
    language TEXT NOT NULL, blob_hash TEXT NOT NULL, imports_text TEXT NOT NULL,
    UNIQUE(namespace, file_path)
);
CREATE INDEX idx_files_ns_path ON files(namespace, file_path);
CREATE TABLE symbols (
    symbol_id TEXT PRIMARY KEY, file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
    namespace TEXT NOT NULL, name TEXT NOT NULL, qualified_name TEXT NOT NULL,
    tokenized_name TEXT NOT NULL, kind TEXT NOT NULL, signature TEXT NOT NULL,
    docstring TEXT, code_snippet TEXT NOT NULL, start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL, in_degree INTEGER NOT NULL DEFAULT 0, body_hash TEXT NOT NULL
);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_symbols_name ON symbols(namespace, name);
CREATE TABLE edges (
    source_symbol_id TEXT NOT NULL REFERENCES symbols(symbol_id) ON DELETE CASCADE,
    target_name TEXT NOT NULL, target_symbol_id TEXT REFERENCES symbols(symbol_id) ON DELETE SET NULL,
    relation TEXT NOT NULL, PRIMARY KEY(source_symbol_id, target_name, relation)
);
CREATE INDEX idx_edges_target ON edges(target_symbol_id);
CREATE VIRTUAL TABLE symbols_fts USING fts5(
    symbol_id UNINDEXED, namespace UNINDEXED, tokenized_name, signature, docstring, code_tokens,
    tokenize = 'porter unicode61'
);
CREATE TRIGGER symbols_fts_delete AFTER DELETE ON symbols BEGIN
    DELETE FROM symbols_fts WHERE symbol_id = old.symbol_id;
END;
INSERT INTO meta(key, value) VALUES ('schema_version', '4');
"#;

/// A symbol with its file context, suitable for lexical results and inspection.
#[derive(Clone, Debug)]
pub struct TargetHit {
    pub symbol_id: String,
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: String,
    pub docstring: Option<String>,
    pub code_snippet: String,
    pub file_path: String,
    pub imports_text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub in_degree: u32,
    pub score: f64,
}

#[derive(Clone, Debug)]
pub struct TargetNeighbor {
    pub symbol: TargetHit,
    pub relation: String,
    pub incoming: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TargetStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
}

/// SQLite store for the lexical index. No extension registration is required.
pub struct TargetStore {
    connection: Mutex<Connection>,
    path: PathBuf,
}

impl TargetStore {
    /// Opens an existing index, or creates a new one if `create` is true.
    pub fn open(path: &Path, create: bool) -> Result<Self> {
        let flags = if create {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let mut conn = Connection::open_with_flags(path, flags)
            .map_err(|e| storage_error("membuka database", e))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| storage_error("busy timeout", e))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| storage_error("mengaktifkan foreign keys", e))?;
        if create {
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| storage_error("memulai inisialisasi", e))?;
            let has_meta: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| storage_error("membaca schema", e))?;
            if has_meta {
                validate_target_schema(&tx)?;
            } else {
                let occupied: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name NOT LIKE 'sqlite_%')",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|e| storage_error("membaca schema", e))?;
                if occupied {
                    return Err(EngineError::storage(
                        "database bukan indeks CIGAR kosong; buat ulang indeks secara aman",
                    ));
                }
                tx.execute_batch(SCHEMA)
                    .map_err(|e| storage_error("membuat schema", e))?;
            }
            tx.commit()
                .map_err(|e| storage_error("menyimpan schema", e))?;
        } else {
            validate_target_schema(&conn)?;
        }
        Ok(Self {
            connection: Mutex::new(conn),
            path: path.to_path_buf(),
        })
    }

    /// Produces a consistent SQLite backup, including committed WAL contents.
    pub fn backup(&self, timestamp: i64) -> Result<PathBuf> {
        let target = PathBuf::from(format!("{}.bak-{timestamp}", self.path.display()));
        if target.exists() {
            return Err(EngineError::storage(format!(
                "backup sudah ada: {}",
                target.display()
            )));
        }
        let path = target
            .to_str()
            .ok_or_else(|| EngineError::storage("path backup bukan UTF-8"))?;
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        conn.execute("VACUUM INTO ?1", [path])
            .map_err(|e| storage_error("membuat backup", e))?;
        Ok(target)
    }

    /// Reads the last committed revision for one namespace.
    pub fn sync_head(&self, namespace: &str) -> Result<Option<String>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        conn.query_row(
            "SELECT last_commit_hash FROM sync_state WHERE namespace=?1",
            [namespace],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| storage_error("membaca sync state", e))
    }

    /// Lists indexed files for delta and dirty sync planning.
    pub fn files(&self, namespace: &str) -> Result<Vec<TargetFileState>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let mut stmt = conn
            .prepare("SELECT file_path,blob_hash FROM files WHERE namespace=?1 ORDER BY file_path")
            .map_err(|e| storage_error("menyiapkan daftar file", e))?;
        let rows = stmt
            .query_map([namespace], |row| {
                Ok(TargetFileState {
                    file_path: row.get(0)?,
                    blob_hash: row.get(1)?,
                })
            })
            .map_err(|e| storage_error("membaca daftar file", e))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| storage_error("membaca file", e))
    }
    /// Returns the tokenizer setting committed with this namespace's index.
    pub fn split_identifiers(&self, namespace: &str) -> Result<Option<bool>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                [format!("split_identifiers:{namespace}")],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| storage_error("membaca pengaturan tokenizer", e))?;
        match value.as_deref() {
            None => Ok(None),
            Some("true") => Ok(Some(true)),
            Some("false") => Ok(Some(false)),
            Some(_) => Err(EngineError::storage(
                "pengaturan tokenizer indeks tidak valid",
            )),
        }
    }

    /// Searches a normalized FTS5 expression using weighted BM25 and centrality.
    pub fn search(
        &self,
        namespace: &str,
        match_query: &str,
        limit: usize,
    ) -> Result<Vec<TargetHit>> {
        self.search_weighted(
            namespace,
            match_query,
            limit,
            &cigar_core::config::SearchWeights::default(),
        )
    }

    /// Searches with the configured BM25 field weights.
    pub fn search_weighted(
        &self,
        namespace: &str,
        match_query: &str,
        limit: usize,
        weights: &cigar_core::config::SearchWeights,
    ) -> Result<Vec<TargetHit>> {
        if match_query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let mut stmt = conn.prepare("SELECT s.symbol_id,s.name,s.qualified_name,s.kind,s.signature,s.docstring,s.code_snippet,f.file_path,f.imports_text,s.start_line,s.end_line,s.in_degree,bm25(symbols_fts,0.0,0.0,?3,?4,?5,?6) FROM symbols_fts JOIN symbols s ON s.symbol_id=symbols_fts.symbol_id JOIN files f ON f.file_id=s.file_id WHERE symbols_fts MATCH ?1 AND s.namespace=?2 AND symbols_fts.namespace=?2")
            .map_err(|e| storage_error("menyiapkan pencarian", e))?;
        let rows = stmt
            .query_map(
                params![
                    match_query,
                    namespace,
                    weights.qualified_name,
                    weights.signature,
                    weights.docstring,
                    weights.code_tokens
                ],
                |row| {
                    let mut hit = read_hit(row)?;
                    let bm25: f64 = row.get(12)?;
                    hit.score = bm25.abs() * (1.0 + f64::from(hit.in_degree).ln_1p());
                    Ok(hit)
                },
            )
            .map_err(|e| storage_error("mencari simbol", e))?;
        let mut hits = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| storage_error("membaca hasil pencarian", e))?;
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.symbol_id.cmp(&b.symbol_id))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    /// Finds symbols with the exact name or qualified name in one namespace.
    pub fn find_symbols(&self, namespace: &str, name: &str) -> Result<Vec<TargetHit>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let mut stmt = conn.prepare("SELECT s.symbol_id,s.name,s.qualified_name,s.kind,s.signature,s.docstring,s.code_snippet,f.file_path,f.imports_text,s.start_line,s.end_line,s.in_degree FROM symbols s JOIN files f ON f.file_id=s.file_id WHERE s.namespace=?1 AND (s.name=?2 OR s.qualified_name=?2) ORDER BY f.file_path,s.start_line,s.symbol_id")
            .map_err(|e| storage_error("menyiapkan pencarian nama", e))?;
        let rows = stmt
            .query_map(params![namespace, name], read_hit)
            .map_err(|e| storage_error("mencari nama", e))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| storage_error("membaca simbol", e))
    }

    /// Returns resolved incoming and outgoing one-hop graph neighbors.
    pub fn neighbors(&self, namespace: &str, symbol_id: &str) -> Result<Vec<TargetNeighbor>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let mut stmt = conn.prepare("SELECT s.symbol_id,s.name,s.qualified_name,s.kind,s.signature,s.docstring,s.code_snippet,f.file_path,f.imports_text,s.start_line,s.end_line,s.in_degree,e.relation,0 FROM edges e JOIN symbols src ON src.symbol_id=e.source_symbol_id JOIN symbols s ON s.symbol_id=e.target_symbol_id JOIN files f ON f.file_id=s.file_id WHERE src.namespace=?1 AND s.namespace=?1 AND src.symbol_id=?2 UNION ALL SELECT s.symbol_id,s.name,s.qualified_name,s.kind,s.signature,s.docstring,s.code_snippet,f.file_path,f.imports_text,s.start_line,s.end_line,s.in_degree,e.relation,1 FROM edges e JOIN symbols target ON target.symbol_id=e.target_symbol_id JOIN symbols s ON s.symbol_id=e.source_symbol_id JOIN files f ON f.file_id=s.file_id WHERE target.namespace=?1 AND s.namespace=?1 AND target.symbol_id=?2 ORDER BY 14,8,10,1")
            .map_err(|e| storage_error("menyiapkan tetangga", e))?;
        let rows = stmt
            .query_map(params![namespace, symbol_id], |row| {
                Ok(TargetNeighbor {
                    symbol: read_hit(row)?,
                    relation: row.get(12)?,
                    incoming: row.get::<_, i64>(13)? != 0,
                })
            })
            .map_err(|e| storage_error("mencari tetangga", e))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| storage_error("membaca tetangga", e))
    }

    /// Counts files, symbols and outgoing edges in one namespace.
    pub fn stats(&self, namespace: &str) -> Result<TargetStats> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files WHERE namespace=?1),
                    (SELECT COUNT(*) FROM symbols WHERE namespace=?1),
                    (SELECT COUNT(*) FROM edges e JOIN symbols s ON s.symbol_id=e.source_symbol_id WHERE s.namespace=?1)",
            [namespace],
            |row| {
                Ok(TargetStats {
                    files: row.get::<_, i64>(0)? as u64,
                    symbols: row.get::<_, i64>(1)? as u64,
                    edges: row.get::<_, i64>(2)? as u64,
                })
            },
        )
        .map_err(|e| storage_error("membaca statistik", e))
    }

    /// Checks SQLite structure, foreign keys and lexical index membership.
    pub fn health_check(&self) -> Result<Vec<String>> {
        let conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let mut problems = Vec::new();
        let mut check = conn
            .prepare("PRAGMA quick_check")
            .map_err(|e| storage_error("menyiapkan quick_check", e))?;
        let rows = check
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| storage_error("menjalankan quick_check", e))?;
        for row in rows {
            let result = row.map_err(|e| storage_error("membaca quick_check", e))?;
            if result != "ok" {
                problems.push(result);
            }
        }
        let mut foreign = conn
            .prepare("PRAGMA foreign_key_check")
            .map_err(|e| storage_error("menyiapkan foreign_key_check", e))?;
        let mut rows = foreign
            .query([])
            .map_err(|e| storage_error("menjalankan foreign_key_check", e))?;
        if rows
            .next()
            .map_err(|e| storage_error("membaca foreign_key_check", e))?
            .is_some()
        {
            problems.push("foreign key violation".into());
        }
        let mismatch: i64 = conn.query_row("SELECT (SELECT COUNT(*) FROM symbols s WHERE (SELECT COUNT(*) FROM symbols_fts f WHERE f.symbol_id=s.symbol_id AND f.namespace=s.namespace AND f.tokenized_name=s.tokenized_name AND f.signature=s.signature AND f.docstring IS s.docstring) != 1) + (SELECT COUNT(*) FROM symbols_fts f WHERE NOT EXISTS(SELECT 1 FROM symbols s WHERE s.symbol_id=f.symbol_id AND s.namespace=f.namespace))", [], |row| row.get(0))
            .map_err(|e| storage_error("memeriksa symbols_fts", e))?;
        if mismatch != 0 {
            problems.push(format!("symbols_fts mismatch: {mismatch}"));
        }
        Ok(problems)
    }

    /// Replaces changed files, relinks the graph and advances the pointer in one transaction.
    pub fn apply_batch(&self, batch: &TargetBatch) -> Result<()> {
        let mut conn = self
            .connection
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))?;
        let tx = conn
            .transaction()
            .map_err(|e| storage_error("memulai batch", e))?;
        for path in &batch.deleted_paths {
            tx.execute(
                "DELETE FROM files WHERE namespace=?1 AND file_path=?2",
                params![batch.namespace, path],
            )
            .map_err(|e| storage_error("menghapus file", e))?;
        }
        for file in &batch.files {
            tx.execute(
                "DELETE FROM files WHERE namespace=?1 AND file_path=?2",
                params![batch.namespace, file.file_path],
            )
            .map_err(|e| storage_error("mengganti file", e))?;
            tx.execute("INSERT INTO files(file_id, namespace, file_path, language, blob_hash, imports_text) VALUES (?1,?2,?3,?4,?5,?6)",
                params![file.file_id, batch.namespace, file.file_path, file.language, file.blob_hash, file.imports_text])
                .map_err(|e| storage_error("menulis file", e))?;
            for symbol in &file.symbols {
                tx.execute("INSERT INTO symbols(symbol_id,file_id,namespace,name,qualified_name,tokenized_name,kind,signature,docstring,code_snippet,start_line,end_line,body_hash) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![symbol.symbol_id,file.file_id,batch.namespace,symbol.name,symbol.qualified_name,symbol.tokenized_name,symbol.kind,symbol.signature,symbol.docstring,symbol.code_snippet,symbol.start_line,symbol.end_line,symbol.body_hash])
                    .map_err(|e| storage_error("menulis simbol", e))?;
                tx.execute("INSERT INTO symbols_fts(symbol_id,namespace,tokenized_name,signature,docstring,code_tokens) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![symbol.symbol_id,batch.namespace,symbol.tokenized_name,symbol.signature,symbol.docstring,symbol.code_tokens])
                    .map_err(|e| storage_error("mengindeks simbol", e))?;
            }
        }
        // Insert edges after all symbols so forward references within the batch work.
        for file in &batch.files {
            for symbol in &file.symbols {
                for edge in &symbol.edges {
                    tx.execute("INSERT INTO edges(source_symbol_id,target_name,target_symbol_id,relation) VALUES (?1,?2,NULL,?3)",
                        params![symbol.symbol_id,edge.target_name,edge.relation])
                        .map_err(|e| storage_error("menulis edge", e))?;
                }
            }
        }
        relink_edges(&tx, &batch.namespace)?;
        for file in &batch.files {
            for symbol in &file.symbols {
                for edge in &symbol.edges {
                    if let Some(target) = &edge.target_symbol_id {
                        tx.execute("UPDATE edges SET target_symbol_id=(SELECT symbol_id FROM symbols WHERE symbol_id=?1 AND namespace=?2) WHERE source_symbol_id=?3 AND target_name=?4 AND relation=?5", params![target,batch.namespace,symbol.symbol_id,edge.target_name,edge.relation])
                            .map_err(|e| storage_error("menghubungkan edge eksplisit", e))?;
                    }
                }
            }
        }
        tx.execute("UPDATE symbols SET in_degree=(SELECT COUNT(DISTINCT source_symbol_id) FROM edges WHERE target_symbol_id=symbols.symbol_id) WHERE namespace=?1", [&batch.namespace])
            .map_err(|e| storage_error("menghitung in-degree", e))?;
        tx.execute("INSERT INTO sync_state(namespace,last_commit_hash,updated_at) VALUES (?1,?2,?3) ON CONFLICT(namespace) DO UPDATE SET last_commit_hash=excluded.last_commit_hash, updated_at=excluded.updated_at", params![batch.namespace,batch.commit_hash,batch.updated_at])
            .map_err(|e| storage_error("memperbarui sync state", e))?;
        if let Some(split) = batch.split_identifiers {
            tx.execute("INSERT INTO meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("split_identifiers:{}", batch.namespace), split.to_string()])
                .map_err(|e| storage_error("menyimpan pengaturan tokenizer", e))?;
        }
        tx.commit().map_err(|e| storage_error("menyimpan batch", e))
    }
}

// Import hints only disambiguate candidates when their module and imported name
// both match. Unknown import syntax remains unresolved rather than guessing.
fn relink_edges(tx: &rusqlite::Transaction<'_>, namespace: &str) -> Result<()> {
    let mut edges = tx.prepare("SELECT e.source_symbol_id,e.target_name,e.relation,f.file_path,f.imports_text FROM edges e JOIN symbols s ON s.symbol_id=e.source_symbol_id JOIN files f ON f.file_id=s.file_id WHERE s.namespace=?1")
        .map_err(|e| storage_error("menyiapkan edge", e))?;
    let rows = edges
        .query_map([namespace], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| storage_error("membaca edge", e))?;
    let mut candidates = tx.prepare("SELECT s.symbol_id,s.name,s.qualified_name,f.file_path FROM symbols s JOIN files f ON f.file_id=s.file_id WHERE s.namespace=?1")
        .map_err(|e| storage_error("menyiapkan kandidat edge", e))?;
    for edge in rows {
        let (source, target, relation, source_path, imports) =
            edge.map_err(|e| storage_error("membaca edge", e))?;
        let hints = import_hints(&imports, &target);
        let rows = candidates
            .query_map([namespace], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(|e| storage_error("membaca kandidat edge", e))?;
        let mut plain = Vec::new();
        let mut hinted = Vec::new();
        for row in rows {
            let (id, name, qualified, path) =
                row.map_err(|e| storage_error("membaca kandidat edge", e))?;
            if name == target || qualified == target {
                plain.push(id.clone());
            }
            if hints.iter().any(|(module, imported)| {
                (name == *imported || qualified == *imported)
                    && module_matches(&source_path, &path, module)
            }) {
                hinted.push(id);
            }
        }
        let resolved = if hinted.len() == 1 {
            hinted.first()
        } else if hints.is_empty() && plain.len() == 1 {
            plain.first()
        } else {
            None
        };
        tx.execute("UPDATE edges SET target_symbol_id=?1 WHERE source_symbol_id=?2 AND target_name=?3 AND relation=?4", params![resolved, source, target, relation])
            .map_err(|e| storage_error("menghubungkan edge", e))?;
    }
    Ok(())
}

fn import_hints(imports: &str, target: &str) -> Vec<(String, String)> {
    let mut hints = Vec::new();
    for line in imports.lines().map(str::trim) {
        let (module, names) = if let Some(rest) = line.strip_prefix("from ") {
            match rest.split_once(" import ") {
                Some(pair) => pair,
                None => continue,
            }
        } else if let Some(rest) = line.strip_prefix("use ") {
            let rest = rest.trim_end_matches(';');
            match rest.rsplit_once("::") {
                Some(pair) => pair,
                None => continue,
            }
        } else if let Some(rest) = line.strip_prefix("import ") {
            match rest.split_once(" from ") {
                Some((names, module)) => (
                    module
                        .trim()
                        .trim_end_matches(';')
                        .trim_matches(['\'', '"']),
                    names,
                ),
                None => continue,
            }
        } else {
            continue;
        };
        for item in names
            .trim()
            .trim_matches(|c| matches!(c, '{' | '}' | '(' | ')'))
            .split(',')
        {
            let item = item.trim().trim_end_matches(';');
            let (name, local) = item.split_once(" as ").unwrap_or((item, item));
            if local.trim() == target && !name.trim().is_empty() {
                hints.push((module.trim().to_owned(), name.trim().to_owned()));
            }
        }
    }
    hints
}

fn module_matches(source_path: &str, candidate_path: &str, module: &str) -> bool {
    let module = module.replace("::", "/");
    let module = if module.starts_with("./") || module.starts_with("../") {
        let parent = source_path
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent);
        format!("{parent}/{module}")
    } else {
        module
            .replace('.', "/")
            .trim_start_matches("crate/")
            .to_owned()
    };
    let mut parts = Vec::new();
    for part in module.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            _ => parts.push(part),
        }
    }
    if parts.is_empty() {
        return false;
    }
    let module = parts.join("/");
    let path = candidate_path
        .rsplit_once('.')
        .map_or(candidate_path, |(stem, _)| stem);
    path == module
        || path
            .strip_suffix(&module)
            .is_some_and(|prefix| prefix.ends_with('/'))
}

impl cigar_core::traits::TargetStorePort for TargetStore {
    fn sync_head(&self, namespace: &str) -> Result<Option<String>> {
        TargetStore::sync_head(self, namespace)
    }

    fn files(&self, namespace: &str) -> Result<Vec<TargetFileState>> {
        TargetStore::files(self, namespace)
    }

    fn split_identifiers(&self, namespace: &str) -> Result<Option<bool>> {
        TargetStore::split_identifiers(self, namespace)
    }

    fn apply_batch(&self, batch: &TargetBatch) -> Result<()> {
        TargetStore::apply_batch(self, batch)
    }
}

fn validate_target_schema(conn: &Connection) -> Result<()> {
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| storage_error("membaca versi schema", e))?;
    let version: u32 = version
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| EngineError::storage("meta.schema_version hilang atau tidak valid"))?;
    let supported = SCHEMA_VERSION;
    if version > supported {
        return Err(EngineError::SchemaTooNew {
            found: version,
            supported,
        });
    }
    if version != supported {
        return Err(EngineError::SchemaMismatch {
            found: version,
            expected: supported,
        });
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('files','symbols','edges','sync_state','symbols_fts')", [], |row| row.get(0),
    ).map_err(|e| storage_error("memeriksa schema", e))?;
    let trigger: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='symbols_fts_delete')", [], |row| row.get(0),
    ).map_err(|e| storage_error("memeriksa trigger FTS", e))?;
    if count != 5 || !trigger {
        return Err(EngineError::storage(
            "schema target tidak lengkap; buat ulang indeks secara aman",
        ));
    }
    Ok(())
}

fn read_hit(row: &rusqlite::Row<'_>) -> rusqlite::Result<TargetHit> {
    Ok(TargetHit {
        symbol_id: row.get(0)?,
        name: row.get(1)?,
        qualified_name: row.get(2)?,
        kind: row.get(3)?,
        signature: row.get(4)?,
        docstring: row.get(5)?,
        code_snippet: row.get(6)?,
        file_path: row.get(7)?,
        imports_text: row.get(8)?,
        start_line: row.get(9)?,
        end_line: row.get(10)?,
        in_degree: row.get(11)?,
        score: 0.0,
    })
}

fn storage_error(action: &str, error: rusqlite::Error) -> EngineError {
    EngineError::storage(format!("gagal {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, path: &str, name: &str) -> TargetFile {
        TargetFile {
            file_id: id.into(),
            file_path: path.into(),
            language: "rust".into(),
            blob_hash: "blob".into(),
            imports_text: String::new(),
            symbols: vec![TargetSymbol {
                symbol_id: format!("{id}-symbol"),
                name: name.into(),
                qualified_name: name.into(),
                tokenized_name: name.into(),
                kind: "function".into(),
                signature: format!("fn {name}()"),
                code_snippet: format!("fn {name}() {{}}"),
                code_tokens: name.into(),
                start_line: 1,
                end_line: 1,
                body_hash: "body".into(),
                ..TargetSymbol::default()
            }],
        }
    }

    #[test]
    fn configured_weights_change_ranking_and_split_setting_is_transactional() {
        let dir = tempfile::tempdir().unwrap();
        let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
        let mut named = file("named", "named.rs", "needle");
        named.symbols[0].signature = "fn other()".into();
        named.symbols[0].code_tokens = "other".into();
        let mut documented = file("documented", "documented.rs", "other");
        documented.symbols[0].docstring = Some("needle".into());
        let mut batch = TargetBatch {
            namespace: "a".into(),
            files: vec![named, documented],
            split_identifiers: Some(true),
            ..TargetBatch::default()
        };
        store.apply_batch(&batch).unwrap();
        assert_eq!(store.split_identifiers("a").unwrap(), Some(true));
        let mut weights = cigar_core::config::SearchWeights::default();
        weights.qualified_name = 20.0;
        weights.docstring = 1.0;
        assert_eq!(
            store.search_weighted("a", "needle", 10, &weights).unwrap()[0].name,
            "needle"
        );
        weights.qualified_name = 1.0;
        weights.docstring = 20.0;
        assert_eq!(
            store.search_weighted("a", "needle", 10, &weights).unwrap()[0].name,
            "other"
        );
        batch.split_identifiers = Some(false);
        let mut duplicate = batch.files[0].clone();
        duplicate.file_path = "duplicate.rs".into();
        batch.files.push(duplicate);
        assert!(store.apply_batch(&batch).is_err());
        assert_eq!(store.split_identifiers("a").unwrap(), Some(true));
        assert_eq!(store.split_identifiers("other").unwrap(), None);
    }

    #[test]
    fn replacement_and_deletion_clean_fts_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
        let mut caller = file("caller", "caller.rs", "caller");
        caller.symbols[0].edges.push(TargetEdge {
            target_name: "target".into(),
            relation: "CALLS".into(),
            target_symbol_id: None,
        });
        let mut batch = TargetBatch {
            namespace: "a".into(),
            commit_hash: "one".into(),
            files: vec![caller, file("target", "target.rs", "target")],
            ..TargetBatch::default()
        };
        store.apply_batch(&batch).unwrap();
        let hits = store.search("a", "target", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "target");
        assert_eq!(store.find_symbols("a", "target").unwrap().len(), 1);
        assert_eq!(
            store.neighbors("a", "target-symbol").unwrap()[0]
                .symbol
                .name,
            "caller"
        );
        assert_eq!(store.stats("a").unwrap().edges, 1);
        assert!(store.health_check().unwrap().is_empty());
        {
            let conn = store.connection.lock().unwrap();
            let degree: i64 = conn
                .query_row(
                    "SELECT in_degree FROM symbols WHERE name='target'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(degree, 1);
        }
        batch.files = vec![file("target", "target.rs", "replacement")];
        batch.commit_hash = "two".into();
        store.apply_batch(&batch).unwrap();
        {
            let conn = store.connection.lock().unwrap();
            let old: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'target'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let new: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'replacement'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let unresolved: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM edges WHERE target_symbol_id IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!((old, new, unresolved), (0, 1, 1));
        }
        batch.files.clear();
        batch.deleted_paths.push("target.rs".into());
        store.apply_batch(&batch).unwrap();
        let conn = store.connection.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'replacement'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn failed_batch_rolls_back_files_fts_and_head() {
        let dir = tempfile::tempdir().unwrap();
        let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
        let mut batch = TargetBatch {
            namespace: "a".into(),
            commit_hash: "one".into(),
            files: vec![file("one", "one.rs", "original")],
            ..TargetBatch::default()
        };
        store.apply_batch(&batch).unwrap();
        batch.commit_hash = "two".into();
        batch.files = vec![
            file("one", "one.rs", "changed"),
            file("one", "duplicate.rs", "duplicate"),
        ];
        assert!(store.apply_batch(&batch).is_err());
        assert_eq!(store.sync_head("a").unwrap().as_deref(), Some("one"));
        let conn = store.connection.lock().unwrap();
        let original: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'original'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let changed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'changed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((original, changed), (1, 0));
    }

    #[test]
    fn links_are_isolated_by_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let store = TargetStore::open(&dir.path().join("store.db"), true).unwrap();
        let mut caller = file("caller-a", "caller.rs", "caller");
        caller.symbols[0].edges.push(TargetEdge {
            target_name: "target".into(),
            relation: "CALLS".into(),
            target_symbol_id: None,
        });
        store
            .apply_batch(&TargetBatch {
                namespace: "a".into(),
                commit_hash: "a1".into(),
                files: vec![caller],
                ..TargetBatch::default()
            })
            .unwrap();
        store
            .apply_batch(&TargetBatch {
                namespace: "b".into(),
                commit_hash: "b1".into(),
                files: vec![file("target-b", "target.rs", "target")],
                ..TargetBatch::default()
            })
            .unwrap();
        let conn = store.connection.lock().unwrap();
        let target: Option<String> = conn
            .query_row("SELECT target_symbol_id FROM edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(target, None);
    }

    #[test]
    fn open_validates_schema_and_preserves_unrelated_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('keep')",
        )
        .unwrap();
        drop(conn);
        assert!(TargetStore::open(&path, true).is_err());
        let conn = Connection::open(&path).unwrap();
        let value: String = conn
            .query_row("SELECT value FROM unrelated", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "keep");
        drop(conn);

        let target = dir.path().join("target.db");
        drop(TargetStore::open(&target, true).unwrap());
        drop(TargetStore::open(&target, true).unwrap());
        drop(TargetStore::open(&target, false).unwrap());
        let conn = Connection::open(&target).unwrap();
        conn.execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
            .unwrap();
        drop(conn);
        assert!(matches!(
            TargetStore::open(&target, false),
            Err(EngineError::SchemaTooNew { found: 99, .. })
        ));
    }
}
