// SPDX-License-Identifier: MPL-2.0

//! Enumerations shared by the domain, storage, and CLI.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::domain::RepoPath;
use crate::error::{EngineError, Result};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            #[must_use]
            pub const fn all() -> &'static [Self] {
                &[$(Self::$variant),+]
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = EngineError;

            fn from_str(s: &str) -> Result<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(EngineError::InvalidInput {
                        message: format!(concat!("nilai ", stringify!($name), " tidak dikenal: {}"), other),
                    }),
                }
            }
        }
    };
}

string_enum!(Language {
    Rust => "rust",
    TypeScript => "typescript",
    Tsx => "tsx",
    Javascript => "javascript",
    Go => "go",
    Python => "python",
});

impl Language {
    /// Detects the language from a path extension. Extension comparison is
    /// case-insensitive per docs/07 ???????section1; the path itself stays case-sensitive.
    #[must_use]
    pub fn from_path(path: &RepoPath) -> Option<Self> {
        let ext = path.extension()?.to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" => Some(Self::Javascript),
            "jsx" => Some(Self::Javascript),
            "go" => Some(Self::Go),
            "py" => Some(Self::Python),
            _ => None,
        }
    }

    #[must_use]
    pub const fn supports_jsx(self) -> bool {
        matches!(self, Self::Tsx | Self::Javascript)
    }
}

string_enum!(SymbolKind {
    Function => "function",
    Method => "method",
    Struct => "struct",
    Interface => "interface",
    Class => "class",
    Trait => "trait",
    Enum => "enum",
});

string_enum!(EdgeRelation {
    Calls => "CALLS",
    UsesType => "USES_TYPE",
    Implements => "IMPLEMENTS",
});

string_enum!(SyncMode {
    Commit => "commit",
    Dirty => "dirty",
    Fs => "fs",
});

#[allow(clippy::derivable_impls)]
impl Default for SyncMode {
    fn default() -> Self {
        Self::Commit
    }
}

string_enum!(SyncStatus {
    Ready => "ready",
    Syncing => "syncing",
    Dirty => "dirty",
    Error => "error",
});

#[allow(clippy::derivable_impls)]
impl Default for SyncStatus {
    fn default() -> Self {
        Self::Ready
    }
}

string_enum!(SourceMode {
    GitWorktree => "git-worktree",
    GitBare => "git-bare",
    FsOnly => "fs-only",
});

string_enum!(ChangeStatus {
    Added => "added",
    Modified => "modified",
    Deleted => "deleted",
    Renamed => "renamed",
    TypeChanged => "type-changed",
});

string_enum!(EmbeddingProvider {
    OnnxLocal => "onnx-local",
    Fake => "fake",
});

string_enum!(Pooling {
    Mean => "mean",
    Cls => "cls",
});

string_enum!(GitMode {
    Auto => "auto",
    Worktree => "worktree",
    Bare => "bare",
    FsOnly => "fs-only",
});

string_enum!(DistanceMetric {
    Cosine => "cosine",
});
