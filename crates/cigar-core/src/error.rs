//! Error taxonomy (docs/06 ?8).
//!
//! `EngineError` is the only public error type. It is `#[non_exhaustive]` so new
//! variants are not breaking, and it maps deterministically to process exit
//! codes (docs/10 ?15).

use std::path::PathBuf;

/// Process exit codes as documented for scripting.
pub mod exit {
    pub const SUCCESS: i32 = 0;
    pub const GENERAL: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const NOT_INITIALIZED: i32 = 3;
    pub const LOCK_BUSY: i32 = 4;
    pub const MODEL: i32 = 5;
    pub const MISMATCH: i32 = 6;
    pub const CANCELLED: i32 = 130;
}

/// Engine result alias.
pub type Result<T> = std::result::Result<T, EngineError>;

/// Every failure the engine can report.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EngineError {
    #[error("CIGAR belum diinisialisasi di {path}; jalankan `cigar init`")]
    NotInitialized { path: String },

    #[error("konfigurasi tidak valid: {message}")]
    Config { message: String },

    #[error("I/O gagal pada {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("operasi Git gagal: {message}")]
    Git { message: String },

    #[error("gagal memparse {path}: {message}")]
    Parse { path: String, message: String },

    #[error("model embedding tidak tersedia: {message}")]
    ModelUnavailable { message: String },

    #[error("inference embedding gagal: {message}")]
    Embedding { message: String },

    #[error("dimensi vektor tidak cocok: diharapkan {expected}, diterima {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("storage error: {message}")]
    Storage { message: String },

    #[error("lock sedang dipegang: {path} (pid {pid:?})")]
    LockBusy { path: String, pid: Option<u32> },

    #[error("schema {found} lebih baru dari yang didukung ({supported}); perbarui CIGAR")]
    SchemaTooNew { found: u32, supported: u32 },

    #[error("schema tidak didukung: {found} (diharapkan {expected}); untuk membangun ulang indeks dengan aman, hentikan proses cigar, pindahkan seluruh direktori .cig dan database atau konfigurasi di luar .cig jika ada ke lokasi cadangan yang belum ada; jalankan `cigar init` dengan konfigurasi yang sesuai lalu `cigar sync`; simpan cadangan sampai data yang diperlukan dipulihkan")]
    SchemaMismatch { found: u32, expected: u32 },

    #[error(
        "konfigurasi berubah pada {field}: diharapkan {expected}, ditemukan {actual}; jalankan `cigar reindex`"
    )]
    ConfigMismatch {
        field: String,
        expected: String,
        actual: String,
    },

    #[error("kolisi ID untuk {id}")]
    IdCollision { id: String },

    #[error("{entity} tidak ditemukan: {id}")]
    NotFound { entity: String, id: String },

    #[error("nama simbol ambigu: {name} ({count} kandidat)")]
    AmbiguousSymbol { name: String, count: usize },

    #[error("input tidak valid: {message}")]
    InvalidInput { message: String },

    #[error("operasi dibatalkan")]
    Cancelled,

    #[error("error internal: {message}")]
    Internal { message: String },
}

impl EngineError {
    /// Convenience constructor for I/O failures that keeps the path in context.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into().display().to_string(),
            source,
        }
    }

    /// Convenience constructor for storage failures.
    pub fn storage(message: impl Into<String>) -> Self {
        Self::Storage {
            message: message.into(),
        }
    }

    /// Convenience constructor for invalid user input.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    /// Convenience constructor for config validation failures.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
        }
    }

    /// Stable process exit code for this error (docs/10 ?15).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NotInitialized { .. } => exit::NOT_INITIALIZED,
            Self::Config { .. } | Self::InvalidInput { .. } | Self::AmbiguousSymbol { .. } => {
                exit::USAGE
            }
            Self::LockBusy { .. } => exit::LOCK_BUSY,
            Self::ModelUnavailable { .. } | Self::Embedding { .. } => exit::MODEL,
            Self::SchemaTooNew { .. }
            | Self::SchemaMismatch { .. }
            | Self::ConfigMismatch { .. }
            | Self::DimensionMismatch { .. } => exit::MISMATCH,
            Self::Cancelled => exit::CANCELLED,
            Self::Io { .. }
            | Self::Git { .. }
            | Self::Parse { .. }
            | Self::Storage { .. }
            | Self::IdCollision { .. }
            | Self::NotFound { .. }
            | Self::Internal { .. } => exit::GENERAL,
        }
    }

    /// True when retrying the same operation can reasonably succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::LockBusy { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_contract() {
        assert_eq!(
            EngineError::NotInitialized {
                path: ".cig".into()
            }
            .exit_code(),
            exit::NOT_INITIALIZED
        );
        assert_eq!(
            EngineError::LockBusy {
                path: ".cig/lock".into(),
                pid: None
            }
            .exit_code(),
            exit::LOCK_BUSY
        );
        assert_eq!(
            EngineError::SchemaTooNew {
                found: 3,
                supported: 2
            }
            .exit_code(),
            exit::MISMATCH
        );
        assert_eq!(
            EngineError::ModelUnavailable {
                message: "missing".into()
            }
            .exit_code(),
            exit::MODEL
        );
        assert_eq!(EngineError::Cancelled.exit_code(), exit::CANCELLED);
        assert_eq!(
            EngineError::InvalidInput {
                message: "bad".into()
            }
            .exit_code(),
            exit::USAGE
        );
    }
}
