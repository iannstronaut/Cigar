//! Schema version and canonical DDL (docs/21 ?3).
//!
//! `SCHEMA_SQL_V2` is executed inside one transaction on first init. The vector
//! dimension is substituted from validated config, never from raw input.

/// Schema version understood by this build.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 2;

/// DDL for schema version 2. `{DIM}` is replaced with a validated integer.
pub const SCHEMA_SQL_V2: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS branch_refs (
    namespace          TEXT NOT NULL,
    branch_name        TEXT NOT NULL,
    commit_hash        TEXT,
    last_sync_mode     TEXT NOT NULL DEFAULT 'commit'
                       CHECK (last_sync_mode IN ('commit','dirty','fs')),
    status             TEXT NOT NULL DEFAULT 'ready'
                       CHECK (status IN ('ready','syncing','dirty','error')),
    worktree_fingerprint TEXT,
    config_fingerprint  TEXT NOT NULL,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (namespace, branch_name)
);

CREATE TABLE IF NOT EXISTS file_blobs (
    blob_hash    TEXT NOT NULL,
    language     TEXT NOT NULL,
    imports_text TEXT NOT NULL DEFAULT '',
    size_bytes   INTEGER NOT NULL,
    PRIMARY KEY (blob_hash, language)
);

CREATE TABLE IF NOT EXISTS symbol_defs (
    symbol_hash  TEXT PRIMARY KEY,
    language     TEXT NOT NULL,
    name         TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    kind         TEXT NOT NULL,
    signature    TEXT NOT NULL,
    docstring    TEXT,
    code_snippet TEXT NOT NULL,
    body_hash    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_symbol_defs_name ON symbol_defs(name);
CREATE INDEX IF NOT EXISTS idx_symbol_defs_qname ON symbol_defs(qualified_name);

CREATE TABLE IF NOT EXISTS branch_files (
    namespace   TEXT NOT NULL,
    branch_name TEXT NOT NULL,
    file_path   TEXT NOT NULL,
    blob_hash   TEXT NOT NULL,
    language    TEXT NOT NULL,
    PRIMARY KEY (namespace, branch_name, file_path),
    FOREIGN KEY (namespace, branch_name)
        REFERENCES branch_refs(namespace, branch_name) ON DELETE CASCADE,
    FOREIGN KEY (blob_hash, language)
        REFERENCES file_blobs(blob_hash, language) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_branch_files_blob
    ON branch_files(namespace, branch_name, blob_hash, language);

CREATE TABLE IF NOT EXISTS blob_symbols (
    blob_hash   TEXT NOT NULL,
    language    TEXT NOT NULL,
    symbol_hash TEXT NOT NULL,
    start_line  INTEGER NOT NULL CHECK (start_line >= 1),
    end_line    INTEGER NOT NULL CHECK (end_line >= start_line),
    PRIMARY KEY (blob_hash, language, symbol_hash, start_line),
    FOREIGN KEY (blob_hash, language)
        REFERENCES file_blobs(blob_hash, language) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash)
        REFERENCES symbol_defs(symbol_hash) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_blob_symbols_symbol ON blob_symbols(symbol_hash);

CREATE TABLE IF NOT EXISTS branch_symbols (
    namespace    TEXT NOT NULL,
    branch_name  TEXT NOT NULL,
    file_path    TEXT NOT NULL,
    blob_hash    TEXT NOT NULL,
    language     TEXT NOT NULL,
    symbol_hash  TEXT NOT NULL,
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    embedding_key TEXT NOT NULL,
    PRIMARY KEY (namespace, branch_name, file_path, symbol_hash, start_line),
    FOREIGN KEY (namespace, branch_name, file_path)
        REFERENCES branch_files(namespace, branch_name, file_path) ON DELETE CASCADE,
    FOREIGN KEY (blob_hash, language, symbol_hash, start_line)
        REFERENCES blob_symbols(blob_hash, language, symbol_hash, start_line) ON DELETE CASCADE,
    FOREIGN KEY (symbol_hash)
        REFERENCES symbol_defs(symbol_hash) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_branch_symbols_embedding
    ON branch_symbols(namespace, branch_name, embedding_key);

CREATE TABLE IF NOT EXISTS symbol_edges (
    source_symbol_hash TEXT NOT NULL,
    target_name        TEXT NOT NULL,
    relation           TEXT NOT NULL
                       CHECK (relation IN ('CALLS','USES_TYPE','IMPLEMENTS')),
    PRIMARY KEY (source_symbol_hash, target_name, relation),
    FOREIGN KEY (source_symbol_hash)
        REFERENCES symbol_defs(symbol_hash) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS branch_edges (
    namespace          TEXT NOT NULL,
    branch_name        TEXT NOT NULL,
    file_path          TEXT NOT NULL,
    source_symbol_hash TEXT NOT NULL,
    target_name        TEXT NOT NULL,
    relation           TEXT NOT NULL
                       CHECK (relation IN ('CALLS','USES_TYPE','IMPLEMENTS')),
    target_symbol_hash TEXT,
    PRIMARY KEY (namespace, branch_name, file_path,
                 source_symbol_hash, target_name, relation),
    FOREIGN KEY (namespace, branch_name, file_path)
        REFERENCES branch_files(namespace, branch_name, file_path) ON DELETE CASCADE,
    FOREIGN KEY (source_symbol_hash)
        REFERENCES symbol_defs(symbol_hash) ON DELETE RESTRICT,
    FOREIGN KEY (target_symbol_hash)
        REFERENCES symbol_defs(symbol_hash) ON DELETE RESTRICT
);
CREATE INDEX IF NOT EXISTS idx_branch_edges_target
    ON branch_edges(namespace, branch_name, target_symbol_hash);
CREATE INDEX IF NOT EXISTS idx_branch_edges_source
    ON branch_edges(namespace, branch_name, source_symbol_hash);

CREATE VIRTUAL TABLE IF NOT EXISTS symbol_vectors USING vec0(
    embedding_key TEXT PRIMARY KEY,
    namespace     TEXT PARTITION KEY,
    embedding     FLOAT[{DIM}] distance_metric=cosine
);
"#;

/// Builds the DDL with a validated dimension.
#[must_use]
pub fn ddl_for_dimensions(dimensions: usize) -> String {
    SCHEMA_SQL_V2.replace("{DIM}", &dimensions.to_string())
}

/// Required `meta` keys (docs/03 ?5, docs/21 ?3).
pub const REQUIRED_META_KEYS: &[&str] = &[
    "schema_version",
    "content_version",
    "engine_version",
    "embedding_model",
    "embedding_revision",
    "embedding_dimensions",
    "embedding_template_version",
    "distance_metric",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_substitutes_dimensions() {
        let ddl = ddl_for_dimensions(768);
        assert!(ddl.contains("FLOAT[768]"));
        assert!(!ddl.contains("{DIM}"));
    }

    #[test]
    fn ddl_is_schema_v2() {
        assert_eq!(SUPPORTED_SCHEMA_VERSION, 2);
        assert!(SCHEMA_SQL_V2.contains("branch_refs"));
        assert!(SCHEMA_SQL_V2.contains("symbol_defs"));
        assert!(SCHEMA_SQL_V2.contains("embedding_key"));
        assert!(!SCHEMA_SQL_V2.contains("file_path_hint"));
    }
}
