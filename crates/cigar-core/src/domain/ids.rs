// SPDX-License-Identifier: MPL-2.0

//! Domain newtypes.
//!
//! Every identifier validates at construction so that invalid values cannot
//! travel through the engine. Display yields the canonical wire form.

use std::fmt;
use std::str::FromStr;

use crate::error::{EngineError, Result};

const HASH_HEX_LEN: usize = 64;
const MAX_NAMESPACE_LEN: usize = 120;
const MAX_BRANCH_LEN: usize = 255;
const MAX_PATH_LEN: usize = 4096;

fn invalid(message: impl Into<String>) -> EngineError {
    EngineError::InvalidInput {
        message: message.into(),
    }
}

fn validate_hash_hex(value: &str) -> Result<()> {
    if value.len() != HASH_HEX_LEN {
        return Err(invalid(format!(
            "hash harus {HASH_HEX_LEN} karakter hex, ditemukan {}",
            value.len()
        )));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid("hash harus hex lowercase"));
    }
    Ok(())
}

macro_rules! string_serde {
    ($name:ident) => {
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                let value = String::deserialize(d)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

macro_rules! hash_newtype {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn parse(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                validate_hash_hex(&value)?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn from_hex_unchecked(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = EngineError;

            fn from_str(s: &str) -> Result<Self> {
                Self::parse(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(
                &self,
                s: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(
                d: D,
            ) -> std::result::Result<Self, D::Error> {
                let value = String::deserialize(d)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

hash_newtype!(BlobHash, "blob");
hash_newtype!(SymbolHash, "symbol");
hash_newtype!(EmbeddingKey, "embedding");
hash_newtype!(ConfigFingerprint, "config fingerprint");

/// A free-standing BLAKE3 hex digest used for non-identity values
/// (worktree fingerprint, commit-ish snapshots, integrity checks).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(String);

impl Digest {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate_hash_hex(&value)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn from_hex_unchecked(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Git commit id. Length is validated as 40 (SHA-1) or 64 (SHA-256); the
/// engine never assumes a fixed length beyond that.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitId(String);

impl CommitId {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() != 40 && value.len() != 64 {
            return Err(invalid(format!(
                "commit id harus 40 atau 64 hex, ditemukan {}",
                value.len()
            )));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(invalid("commit id harus hex lowercase"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Abbreviated form for human output.
    #[must_use]
    pub fn short(&self) -> &str {
        let end = self.0.len().min(7);
        &self.0[..end]
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for CommitId {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl serde::Serialize for CommitId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for CommitId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Namespace name. A safe label, never a path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Namespace(String);

impl Namespace {
    pub const DEFAULT: &'static str = "default";

    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(invalid("namespace tidak boleh kosong"));
        }
        if value.len() > MAX_NAMESPACE_LEN {
            return Err(invalid("namespace terlalu panjang"));
        }
        if !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid("namespace hanya boleh alfanumerik, '-', '_', '.'"));
        }
        if value.starts_with('.') || value == "." || value == ".." {
            return Err(invalid("namespace tidak boleh berupa path"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Namespace {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Namespace {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

string_serde!(Namespace);

/// Branch virtual name used for detached HEAD. Git itself forbids refs that
/// start with `@`, so this cannot collide with a user branch.
pub const DETACHED_BRANCH: &str = "@detached";

/// Git branch name as stored in `branch_refs`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

impl BranchName {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(invalid("nama branch tidak boleh kosong"));
        }
        if value.len() > MAX_BRANCH_LEN {
            return Err(invalid("nama branch terlalu panjang (maks 255)"));
        }
        if value == DETACHED_BRANCH {
            return Ok(Self(value));
        }
        if value.starts_with('/') || value.ends_with('/') {
            return Err(invalid("nama branch tidak boleh diawali/diakhiri '/'"));
        }
        if value.contains("//") {
            return Err(invalid("nama branch tidak boleh mengandung '//'"));
        }
        if value.contains("..") {
            return Err(invalid("nama branch tidak boleh mengandung '..'"));
        }
        if value.contains('\\') {
            return Err(invalid("nama branch tidak boleh mengandung '\\'"));
        }
        if value.starts_with('@') {
            return Err(invalid("nama branch tidak boleh diawali '@'"));
        }
        if value.ends_with(".lock") || value.ends_with('.') {
            return Err(invalid("nama branch tidak valid"));
        }
        if value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
        {
            return Err(invalid("komponen nama branch tidak valid"));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(invalid("nama branch mengandung karakter kontrol"));
        }
        if value.contains([' ', '~', '^', ':', '?', '*', '[', ']']) {
            return Err(invalid("nama branch mengandung karakter terlarang"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn detached() -> Self {
        Self(DETACHED_BRANCH.to_owned())
    }

    #[must_use]
    pub fn is_detached(&self) -> bool {
        self.0 == DETACHED_BRANCH
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BranchName {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

string_serde!(BranchName);

/// Repository-relative path, always `/`-separated, never leading `./`,
/// never containing `..`, no trailing slash, no control characters.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let raw = value.into();
        let unified = raw.replace('\\', "/");
        let mut normalized = String::with_capacity(unified.len());
        let mut previous_slash = false;
        for ch in unified.chars() {
            if ch == '/' {
                if previous_slash {
                    continue;
                }
                previous_slash = true;
            } else {
                previous_slash = false;
            }
            normalized.push(ch);
        }
        if normalized.is_empty() {
            return Err(invalid("path tidak boleh kosong"));
        }
        if normalized.len() > MAX_PATH_LEN {
            return Err(invalid("path terlalu panjang"));
        }
        if normalized.starts_with('/') || normalized.starts_with("./") {
            return Err(invalid("path harus relatif terhadap root repo"));
        }
        if normalized.contains('\0') || normalized.chars().any(|c| c.is_control()) {
            return Err(invalid("path mengandung karakter kontrol"));
        }
        let trimmed = normalized.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(invalid("path tidak valid"));
        }
        if trimmed.split('/').any(|part| part == ".." || part == ".") {
            return Err(invalid("path mengandung '.' atau '..'"));
        }
        Ok(Self(trimmed.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn extension(&self) -> Option<&str> {
        let file = self.0.rsplit('/').next()?;
        let (_, ext) = file.rsplit_once('.')?;
        if ext.is_empty() {
            return None;
        }
        Some(ext)
    }

    #[must_use]
    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    #[must_use]
    pub fn parent(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Lexically joins a relative child under this path.
    pub fn join(&self, child: &str) -> Result<Self> {
        if child.is_empty() {
            return Ok(self.clone());
        }
        let joined = format!("{}/{}", self.0, child);
        Self::parse(joined)
    }

    #[must_use]
    pub fn starts_with_path(&self, prefix: &RepoPath) -> bool {
        self.0 == prefix.0 || self.0.starts_with(&format!("{}/", prefix.0))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for RepoPath {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

string_serde!(RepoPath);

/// A symbol occurrence inside a specific branch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OccurrenceRef {
    pub branch: BranchName,
    pub file_path: RepoPath,
    pub symbol_hash: SymbolHash,
    pub start_line: u32,
}

impl OccurrenceRef {
    #[must_use]
    pub fn new(
        branch: BranchName,
        file_path: RepoPath,
        symbol_hash: SymbolHash,
        start_line: u32,
    ) -> Self {
        Self {
            branch,
            file_path,
            symbol_hash,
            start_line,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_rejects_paths() {
        assert!(Namespace::parse("../etc").is_err());
        assert!(Namespace::parse("a/b").is_err());
        assert!(Namespace::parse("").is_err());
        assert!(Namespace::parse("team.alpha").is_ok());
    }

    #[test]
    fn repo_path_normalizes_separators() {
        let path = RepoPath::parse("src\\auth//middleware.rs").unwrap();
        assert_eq!(path.as_str(), "src/auth/middleware.rs");
        assert_eq!(path.extension(), Some("rs"));
    }

    #[test]
    fn repo_path_rejects_escapes() {
        assert!(RepoPath::parse("/abs.rs").is_err());
        assert!(RepoPath::parse("../x.rs").is_err());
        assert!(RepoPath::parse("a/../b.rs").is_err());
        assert!(RepoPath::parse("./x.rs").is_err());
    }

    #[test]
    fn branch_validation() {
        assert!(BranchName::parse("feat/auth").is_ok());
        assert!(BranchName::parse("@detached").unwrap().is_detached());
        assert!(BranchName::parse("a..b").is_err());
        assert!(BranchName::parse("a//b").is_err());
        assert!(BranchName::parse("a b").is_err());
        assert!(BranchName::parse("@oops").is_err());
    }

    #[test]
    fn hash_format() {
        let hex = "a".repeat(64);
        assert!(BlobHash::parse(hex.clone()).is_ok());
        assert!(BlobHash::parse("abc").is_err());
        assert!(BlobHash::parse(hex.to_uppercase()).is_err());
    }

    #[test]
    fn commit_ids_allow_both_widths() {
        assert!(CommitId::parse("a".repeat(40)).is_ok());
        assert!(CommitId::parse("a".repeat(64)).is_ok());
        assert!(CommitId::parse("a".repeat(41)).is_err());
    }
}
