//! Schema migration and backup helpers (docs/21 ?7).
//!
//! v1 stores cannot be upgraded in place: v2 changes identity and ownership, so
//! the only correct migration is `reindex`. This module therefore provides the
//! backup primitive used before any destructive rebuild, and a v1 detector that
//! returns an actionable error instead of silently corrupting data.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use cigar_core::error::{EngineError, Result};

use crate::sqlite::SqliteStore;

/// Copies the store to `store.db.bak-<timestamp>` using `VACUUM INTO`, which is
/// consistent even while WAL is active.
pub fn backup_store(store: &SqliteStore, timestamp: i64) -> Result<PathBuf> {
    let target = PathBuf::from(format!("{}.bak-{timestamp}", store.path().display()));
    if target.exists() {
        fs::remove_file(&target).map_err(|err| EngineError::io(&target, err))?;
    }
    let conn = store.conn()?;
    let sql = format!(
        "VACUUM INTO '{}'",
        escape_sql_string(&target.display().to_string())
    );
    conn.execute_batch(&sql)
        .map_err(|err| EngineError::storage(format!("backup gagal: {err}")))?;
    Ok(target)
}

fn escape_sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

/// Detects a v1 database by the presence of `files`/`symbols` tables and a
/// missing `branch_refs` table.
pub fn is_schema_v1(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let mut header = [0_u8; 16];
    let mut file = fs::File::open(path).map_err(|err| EngineError::io(path, err))?;
    let read = file
        .read(&mut header)
        .map_err(|err| EngineError::io(path, err))?;
    if read < 16 || &header[..16] != b"SQLite format 3\0" {
        return Err(EngineError::storage(format!(
            "{} bukan database SQLite yang valid",
            path.display()
        )));
    }
    let store = SqliteStore::open(path, 768, false)?;
    let conn = store.conn()?;
    let has_v1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('files','symbols')",
            [],
            |row| row.get(0),
        )
        .map_err(|err| EngineError::storage(err.to_string()))?;
    let has_v2: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='branch_refs'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| EngineError::storage(err.to_string()))?;
    Ok(has_v1 > 0 && has_v2 == 0)
}

/// Errors with the migration instruction required by docs/21 ?7.
pub fn require_reindex_for_v1(path: &Path) -> Result<()> {
    if is_schema_v1(path)? {
        return Err(EngineError::storage(format!(
            "{} menggunakan schema v1; hentikan proses cigar, pindahkan seluruh direktori .cig dan database atau konfigurasi di luar .cig jika ada ke lokasi cadangan yang belum ada; jalankan `cigar init` dengan konfigurasi yang sesuai lalu `cigar sync`; simpan cadangan sampai data yang diperlukan dipulihkan",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    #[test]
    fn detects_v1_layout() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE files(file_id TEXT PRIMARY KEY);
             CREATE TABLE symbols(symbol_id TEXT PRIMARY KEY);",
        )
        .unwrap();
        drop(conn);
        assert!(is_schema_v1(&path).unwrap());
    }

    #[test]
    fn rejects_non_sqlite_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        fs::write(&path, b"not a database").unwrap();
        assert!(is_schema_v1(&path).is_err());
    }

    #[test]
    fn backup_creates_copy() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let store = crate::open_or_init_store(&path, 8).unwrap();
        let backup = backup_store(&store, 1_700_000_000_000).unwrap();
        assert!(backup.exists());
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("bak-"));
    }
}
