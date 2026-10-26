//! Store factory with schema validation (docs/04 ??section3).

use std::path::Path;

use rusqlite::OptionalExtension;

use cigar_core::domain::ConfigFingerprint;
use cigar_core::error::{EngineError, Result};
use cigar_core::SCHEMA_VERSION;

use crate::schema::{ddl_for_dimensions, REQUIRED_META_KEYS, SUPPORTED_SCHEMA_VERSION};
use crate::sqlite::{register_sqlite_vec, SqliteStore};

/// Opens an existing store and validates schema/model compatibility.
pub fn open_store(path: &Path, dimensions: usize) -> Result<SqliteStore> {
    register_sqlite_vec();
    if !path.exists() {
        return Err(EngineError::NotInitialized {
            path: path.display().to_string(),
        });
    }
    let store = SqliteStore::open(path, dimensions, false)?;
    validate_schema(&store, dimensions)?;
    Ok(store)
}

/// Creates the store if needed and validates it.
pub fn open_or_init_store(path: &Path, dimensions: usize) -> Result<SqliteStore> {
    register_sqlite_vec();
    let store = SqliteStore::open(path, dimensions, true)?;
    init_or_validate(&store, dimensions)?;
    Ok(store)
}

fn validate_schema(store: &SqliteStore, dimensions: usize) -> Result<()> {
    let conn = store.conn()?;
    let version = read_schema_version(&conn)?;
    if version > SUPPORTED_SCHEMA_VERSION {
        return Err(EngineError::SchemaTooNew {
            found: version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    if version != SUPPORTED_SCHEMA_VERSION {
        return Err(EngineError::SchemaMismatch {
            found: version,
            expected: SUPPORTED_SCHEMA_VERSION,
        });
    }
    let stored_dimensions =
        read_meta(&conn, "embedding_dimensions")?.and_then(|value| value.parse::<usize>().ok());
    match stored_dimensions {
        Some(stored) if stored != dimensions => Err(EngineError::DimensionMismatch {
            expected: stored,
            actual: dimensions,
        }),
        Some(_) => Ok(()),
        None => Err(EngineError::storage(
            "meta.embedding_dimensions hilang; jalankan `cigar reindex`",
        )),
    }
}

/// Reads `meta.schema_version`.
pub fn read_schema_version(connection: &rusqlite::Connection) -> Result<u32> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| EngineError::storage(format!("gagal membaca schema_version: {err}")))?;
    value
        .ok_or_else(|| EngineError::storage("meta.schema_version hilang"))?
        .parse::<u32>()
        .map_err(|_| EngineError::storage("meta.schema_version bukan integer"))
}

/// Reads one meta value.
pub fn read_meta(connection: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|err| EngineError::storage(format!("gagal membaca meta.{key}: {err}")))
}

/// Writes one meta value, preserving existing values.
pub fn write_meta_if_absent(
    connection: &rusqlite::Connection,
    key: &str,
    value: &str,
) -> Result<()> {
    connection
        .execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING",
            rusqlite::params![key, value],
        )
        .map_err(|err| EngineError::storage(format!("gagal menulis meta.{key}: {err}")))?;
    Ok(())
}

/// Creates every table, index, and the vec0 table if absent.
pub fn init_or_validate(store: &SqliteStore, dimensions: usize) -> Result<()> {
    let conn = store.conn()?;
    let tables_exist: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| EngineError::storage(format!("gagal membaca sqlite_master: {err}")))?;
    if tables_exist == 0 {
        conn.execute_batch(&ddl_for_dimensions(dimensions))
            .map_err(|err| EngineError::storage(format!("gagal membuat schema: {err}")))?;
        write_meta_if_absent(&conn, "schema_version", &SCHEMA_VERSION.to_string())?;
        write_meta_if_absent(&conn, "embedding_dimensions", &dimensions.to_string())?;
        return Ok(());
    }

    let version: Option<String> = {
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| EngineError::storage(format!("gagal membaca schema_version: {err}")))?
    };
    let Some(version) = version else {
        return Err(EngineError::storage(
            "database ada tetapi meta.schema_version hilang; jalankan `cigar reindex`",
        ));
    };
    let version: u32 = version
        .parse()
        .map_err(|_| EngineError::storage("meta.schema_version bukan integer"))?;
    if version > SUPPORTED_SCHEMA_VERSION {
        return Err(EngineError::SchemaTooNew {
            found: version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    if version < SUPPORTED_SCHEMA_VERSION {
        return Err(EngineError::SchemaMismatch {
            found: version,
            expected: SUPPORTED_SCHEMA_VERSION,
        });
    }

    let stored_dimensions =
        read_meta(&conn, "embedding_dimensions")?.and_then(|value| value.parse::<usize>().ok());
    if let Some(stored) = stored_dimensions {
        if stored != dimensions {
            return Err(EngineError::DimensionMismatch {
                expected: stored,
                actual: dimensions,
            });
        }
    } else {
        write_meta_if_absent(&conn, "embedding_dimensions", &dimensions.to_string())?;
    }

    // Ensure all statements are present (idempotent) so a partially created
    // schema from an interrupted init is repaired.
    conn.execute_batch(&ddl_for_dimensions(dimensions))
        .map_err(|err| EngineError::storage(format!("gagal melengkapi schema: {err}")))?;
    Ok(())
}

/// Fills model metadata on first init without clobbering existing values.
pub fn record_model_meta(store: &SqliteStore, values: &[(&str, String)]) -> Result<()> {
    let conn = store.conn()?;
    for (key, value) in values {
        write_meta_if_absent(&conn, key, value)?;
    }
    for key in REQUIRED_META_KEYS {
        if key == &"schema_version" || key == &"embedding_dimensions" {
            continue;
        }
        debug_assert!(
            read_meta(&conn, key)?.is_some() || *key == "content_version",
            "meta key {key} belum ditulis"
        );
    }
    Ok(())
}

/// Returns the config fingerprint stored in `meta`, if any.
pub fn stored_config_fingerprint(store: &SqliteStore) -> Result<Option<ConfigFingerprint>> {
    let conn = store.conn()?;
    let value = read_meta(&conn, "config_fingerprint")?;
    value.map(ConfigFingerprint::parse).transpose()
}

/// Verifies that stored model metadata matches the active configuration.
pub fn verify_model_compat(
    store: &SqliteStore,
    model: &str,
    revision: &str,
    dimensions: usize,
    template_version: u32,
    metric: &str,
) -> Result<()> {
    let conn = store.conn()?;
    let checks = [
        ("embedding_model", model.to_owned()),
        ("embedding_revision", revision.to_owned()),
        ("embedding_dimensions", dimensions.to_string()),
        ("embedding_template_version", template_version.to_string()),
        ("distance_metric", metric.to_owned()),
    ];
    for (key, expected) in checks {
        let Some(actual) = read_meta(&conn, key)? else {
            continue;
        };
        if actual != expected {
            return Err(EngineError::ConfigMismatch {
                field: key.to_owned(),
                expected: actual,
                actual: expected,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn schema_version(store: &SqliteStore) -> u32 {
        let conn = store.conn().unwrap();
        read_schema_version(&conn).unwrap()
    }

    #[test]
    fn init_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let store = open_or_init_store(&path, 8).unwrap();
        drop(store);
        let store = open_or_init_store(&path, 8).unwrap();
        assert_eq!(schema_version(&store), 2);
    }

    #[test]
    fn dimension_mismatch_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let _store = open_or_init_store(&path, 8).unwrap();
        let error = open_store(&path, 16).unwrap_err();
        assert!(matches!(error, EngineError::DimensionMismatch { .. }));
    }

    #[test]
    fn schema_too_new_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("store.db");
        let store = open_or_init_store(&path, 8).unwrap();
        {
            let conn = store.conn().unwrap();
            conn.execute("UPDATE meta SET value='99' WHERE key='schema_version'", [])
                .unwrap();
        }
        drop(store);
        let error = open_store(&path, 8).unwrap_err();
        assert!(matches!(error, EngineError::SchemaTooNew { .. }));
    }
}
