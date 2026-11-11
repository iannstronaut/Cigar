//! SQLite connection setup and sqlite-vec registration.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};
use std::time::Duration;

use rusqlite::Connection;

use cigar_core::error::{EngineError, Result};

static REGISTER: Once = Once::new();

// SAFETY: `sqlite3_vec_init` has the SQLite extension entry-point ABI declared
// by `sqlite3_auto_extension`. The cast only restates that ABI for the Rust
// type system; it must stay bound to that specific symbol.
type AutoExtensionEntry = unsafe extern "C" fn(
    *mut rusqlite::ffi::sqlite3,
    *mut *mut std::os::raw::c_char,
    *const rusqlite::ffi::sqlite3_api_routines,
) -> std::os::raw::c_int;

/// Registers sqlite-vec as an auto-extension for every connection created in
/// this process afterwards. Safe to call multiple times; only the first call
/// installs the extension.
///
/// The registration is process-wide and intentionally required before any
/// connection is opened, so `vec0` tables and `vec_version()` are available.
#[allow(unsafe_code)]
pub fn register_sqlite_vec() {
    REGISTER.call_once(|| unsafe {
        let entry: AutoExtensionEntry =
            std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(entry));
    });
}

/// Store handle with a single writer connection.
pub struct SqliteStore {
    writer: Mutex<Connection>,
    path: PathBuf,
    read_only: bool,
    dimensions: usize,
    pub(crate) search: cigar_core::config::SearchConfig,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStore")
            .field("path", &self.path)
            .field("read_only", &self.read_only)
            .field("dimensions", &self.dimensions)
            .finish()
    }
}

impl SqliteStore {
    /// Opens (and optionally creates) the store.
    pub fn open(path: &Path, dimensions: usize, create: bool) -> Result<Self> {
        register_sqlite_vec();
        let connection = if create {
            Connection::open(path)
        } else {
            Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
        }
        .map_err(|err| EngineError::storage(format!("gagal membuka {}: {err}", path.display())))?;

        apply_pragmas(&connection, !create)?;
        let store = Self {
            writer: Mutex::new(connection),
            path: path.to_path_buf(),
            read_only: !create,
            dimensions,
            search: cigar_core::config::Config::default().search,
        };
        store.verify_vec_extension()?;
        Ok(store)
    }

    /// Applies the search settings used by indexing and lexical queries.
    #[must_use]
    pub fn with_search_config(mut self, search: &cigar_core::config::SearchConfig) -> Self {
        self.search = search.clone();
        self
    }

    fn verify_vec_extension(&self) -> Result<()> {
        let conn = self.conn()?;
        let version: String = conn
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .map_err(|err| EngineError::storage(format!("sqlite-vec tidak tersedia: {err}")))?;
        if !version.starts_with('v') {
            return Err(EngineError::storage(format!(
                "versi sqlite-vec tidak dikenal: {version}"
            )));
        }
        Ok(())
    }

    pub(crate) fn conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.writer
            .lock()
            .map_err(|_| EngineError::storage("writer lock rusak"))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Runs `PRAGMA wal_checkpoint(TRUNCATE)` when the WAL exceeds the
    /// configured threshold.
    pub fn checkpoint_if_needed(&self, threshold_bytes: u64) -> Result<()> {
        let wal = PathBuf::from(format!("{}-wal", self.path.display()));
        let Ok(metadata) = std::fs::metadata(&wal) else {
            return Ok(());
        };
        if metadata.len() <= threshold_bytes {
            return Ok(());
        }
        let conn = self.conn()?;
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .map_err(|err| EngineError::storage(format!("checkpoint gagal: {err}")))?;
        Ok(())
    }

    /// Size of the database file (and WAL) in bytes.
    #[must_use]
    pub fn on_disk_size(&self) -> u64 {
        let main = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
        let wal =
            std::fs::metadata(format!("{}-wal", self.path.display())).map_or(0, |meta| meta.len());
        main + wal
    }
}

fn apply_pragmas(connection: &Connection, read_only: bool) -> Result<()> {
    let busy_timeout = 5_000_i64;
    connection
        .busy_timeout(Duration::from_millis(busy_timeout as u64))
        .map_err(|err| EngineError::storage(format!("busy_timeout gagal: {err}")))?;
    let mut statements = vec![
        "PRAGMA foreign_keys = ON;",
        "PRAGMA temp_store = MEMORY;",
        "PRAGMA cache_size = -64000;",
    ];
    if !read_only {
        statements.push("PRAGMA journal_mode = WAL;");
        statements.push("PRAGMA synchronous = NORMAL;");
        statements.push("PRAGMA mmap_size = 268435456;");
        statements.push("PRAGMA wal_autocheckpoint = 1000;");
    }
    for statement in statements {
        connection
            .execute_batch(statement)
            .map_err(|err| EngineError::storage(format!("PRAGMA gagal: {statement}: {err}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn registration_is_idempotent() {
        register_sqlite_vec();
        register_sqlite_vec();
        let connection = Connection::open_in_memory().unwrap();
        let version: String = connection
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .unwrap();
        assert!(version.starts_with('v'), "{version}");
    }

    #[test]
    fn pragmas_are_applied() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let store = SqliteStore::open(&path, 8, true).unwrap();
        let conn = store.conn().unwrap();
        let foreign_keys: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
    }
}
