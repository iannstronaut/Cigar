//! Configuration model, defaults, validation, and precedence (docs/10 section16).
//!
//! Precedence is CLI flag > environment (`CIGAR_*`) > `.cig/config.toml` >
//! built-in default. Validation runs once and produces a `Config` whose
//! fingerprint can be compared with `meta` to force a reindex on drift.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{EmbeddingProvider, GitMode, Namespace, Pooling, RepoPath};
use crate::error::{EngineError, Result};

/// Largest supported embedding dimension (docs/10 section16.3).
pub const MAX_DIMENSIONS: usize = 65_536;

/// Root configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub engine: EngineConfig,
    pub git: GitConfig,
    pub embedding: EmbeddingConfig,
    pub storage: StorageConfig,
    pub search: SearchConfig,
    pub query: QueryConfig,
    pub logging: LoggingConfig,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EngineConfig {
    pub version: String,
    pub namespace: Namespace,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GitConfig {
    pub mode: GitMode,
    pub include_untracked: bool,
    pub max_file_bytes: u64,
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchConfig {
    pub mode: String,
    pub split_identifiers: bool,
    pub weights: SearchWeights,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchWeights {
    pub qualified_name: f64,
    pub signature: f64,
    pub docstring: f64,
    pub code_tokens: f64,
}

impl Default for SearchWeights {
    fn default() -> Self {
        Self {
            qualified_name: 10.0,
            signature: 5.0,
            docstring: 3.0,
            code_tokens: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddingConfig {
    pub provider: EmbeddingProvider,
    pub model: String,
    pub revision: String,
    pub dimensions: usize,
    pub model_dir: PathBuf,
    pub batch_size: usize,
    pub max_tokens: usize,
    pub max_code_chars: usize,
    pub template_version: u32,
    pub pooling: Pooling,
    pub threads: usize,
    pub normalize: bool,
    pub query_prefix: String,
    pub document_prefix: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StorageConfig {
    pub driver: String,
    /// Absolute store path after resolution.
    pub path: PathBuf,
    pub busy_timeout_ms: u64,
    pub wal_checkpoint_threshold_mb: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueryConfig {
    pub top_k: usize,
    pub depth: u8,
    pub max_tokens: usize,
    pub max_neighbors: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoggingConfig {
    pub level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: EngineConfig {
                version: crate::ENGINE_VERSION.to_owned(),
                namespace: Namespace::default(),
            },
            git: GitConfig {
                mode: GitMode::Auto,
                include_untracked: true,
                max_file_bytes: 1_048_576,
                exclude: vec![
                    "target/**".into(),
                    "node_modules/**".into(),
                    "vendor/**".into(),
                    ".next/**".into(),
                ],
            },
            search: SearchConfig {
                mode: "lexical-graph".into(),
                split_identifiers: true,
                weights: SearchWeights::default(),
            },
            embedding: EmbeddingConfig {
                provider: EmbeddingProvider::OnnxLocal,
                model: "granite-embedding-125m-english".into(),
                revision: String::new(),
                dimensions: 768,
                model_dir: default_model_dir(),
                batch_size: 32,
                max_tokens: 512,
                max_code_chars: 12_000,
                template_version: crate::TEMPLATE_VERSION,
                pooling: Pooling::Mean,
                threads: 0,
                normalize: true,
                query_prefix: String::new(),
                document_prefix: String::new(),
            },
            storage: StorageConfig {
                driver: "sqlite".into(),
                path: PathBuf::from(".cig/store.db"),
                busy_timeout_ms: 5_000,
                wal_checkpoint_threshold_mb: 64,
            },
            query: QueryConfig {
                top_k: 5,
                depth: 1,
                max_tokens: 6_000,
                max_neighbors: 8,
            },
            logging: LoggingConfig {
                level: "warn".into(),
            },
        }
    }
}

/// `~/.cigar/models`, expanding `~` on every platform.
#[must_use]
pub fn default_model_dir() -> PathBuf {
    home_dir().join(".cigar").join("models")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Expands a leading `~` and joins relative paths onto `root`.
#[must_use]
pub fn expand_path(raw: &str, root: &Path) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return home_dir();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return home_dir().join(rest);
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// The TOML shape on disk. Unknown keys are tolerated for forward
/// compatibility (docs/10 section16.3).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub engine: Option<EngineFile>,
    #[serde(default)]
    pub git: Option<GitFile>,
    #[serde(default)]
    pub search: Option<SearchFile>,
    #[serde(default)]
    pub embedding: Option<EmbeddingFile>,
    #[serde(default)]
    pub storage: Option<StorageFile>,
    #[serde(default)]
    pub query: Option<QueryFile>,
    #[serde(default)]
    pub logging: Option<LoggingFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineFile {
    pub version: Option<String>,
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitFile {
    pub mode: Option<String>,
    pub include_untracked: Option<bool>,
    pub max_file_bytes: Option<u64>,
    pub exclude: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchFile {
    pub mode: Option<String>,
    pub split_identifiers: Option<bool>,
    pub weights: Option<SearchWeightsFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchWeightsFile {
    pub qualified_name: Option<f64>,
    pub signature: Option<f64>,
    pub docstring: Option<f64>,
    pub code_tokens: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmbeddingFile {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub dimensions: Option<usize>,
    pub model_dir: Option<String>,
    pub batch_size: Option<usize>,
    pub max_tokens: Option<usize>,
    pub max_code_chars: Option<usize>,
    pub template_version: Option<u32>,
    pub pooling: Option<String>,
    pub threads: Option<usize>,
    pub normalize: Option<bool>,
    pub query_prefix: Option<String>,
    pub document_prefix: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StorageFile {
    pub driver: Option<String>,
    pub uri: Option<String>,
    pub busy_timeout_ms: Option<u64>,
    pub wal_checkpoint_threshold_mb: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryFile {
    pub top_k: Option<usize>,
    pub depth: Option<u8>,
    pub max_tokens: Option<usize>,
    pub max_neighbors: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoggingFile {
    pub level: Option<String>,
}

impl Config {
    /// Parses TOML text, applying defaults for absent fields.
    pub fn from_toml(text: &str, root: &Path, cig_dir: &Path) -> Result<Self> {
        let file: ConfigFile = toml::from_str(text).map_err(|err| EngineError::Config {
            message: format!("gagal membaca config.toml: {err}"),
        })?;
        Self::from_file(file, root, cig_dir)
    }

    /// Builds a validated config from the on-disk shape.
    pub fn from_file(file: ConfigFile, root: &Path, cig_dir: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(engine) = file.engine {
            if let Some(version) = engine.version {
                config.engine.version = version;
            }
            if let Some(namespace) = engine.namespace {
                config.engine.namespace = Namespace::parse(namespace)?;
            }
        }
        if let Some(git) = file.git {
            if let Some(mode) = git.mode {
                config.git.mode = mode.parse()?;
            }
            if let Some(include) = git.include_untracked {
                config.git.include_untracked = include;
            }
            if let Some(max) = git.max_file_bytes {
                config.git.max_file_bytes = max;
            }
            if let Some(exclude) = git.exclude {
                config.git.exclude = exclude;
            }
        }
        if let Some(search) = file.search {
            if let Some(mode) = search.mode {
                config.search.mode = mode;
            }
            if let Some(value) = search.split_identifiers {
                config.search.split_identifiers = value;
            }
            if let Some(weights) = search.weights {
                if let Some(value) = weights.qualified_name {
                    config.search.weights.qualified_name = value;
                }
                if let Some(value) = weights.signature {
                    config.search.weights.signature = value;
                }
                if let Some(value) = weights.docstring {
                    config.search.weights.docstring = value;
                }
                if let Some(value) = weights.code_tokens {
                    config.search.weights.code_tokens = value;
                }
            }
        }
        if let Some(embedding) = file.embedding {
            apply_embedding(&mut config.embedding, embedding, root);
        }
        if let Some(storage) = file.storage {
            if let Some(driver) = storage.driver {
                config.storage.driver = driver;
            }
            if let Some(uri) = storage.uri {
                config.storage.path = expand_path(&uri, root);
            }
            if let Some(timeout) = storage.busy_timeout_ms {
                config.storage.busy_timeout_ms = timeout;
            }
            if let Some(threshold) = storage.wal_checkpoint_threshold_mb {
                config.storage.wal_checkpoint_threshold_mb = threshold;
            }
        }
        if let Some(query) = file.query {
            if let Some(value) = query.top_k {
                config.query.top_k = value;
            }
            if let Some(value) = query.depth {
                config.query.depth = value;
            }
            if let Some(value) = query.max_tokens {
                config.query.max_tokens = value;
            }
            if let Some(value) = query.max_neighbors {
                config.query.max_neighbors = value;
            }
        }
        if let Some(logging) = file.logging {
            if let Some(level) = logging.level {
                config.logging.level = level;
            }
        }

        // Default store path follows --cig-dir unless explicitly configured.
        if config.storage.path == Path::new(".cig/store.db") {
            config.storage.path = cig_dir.join("store.db");
        }

        config.validate()?;
        Ok(config)
    }

    /// Applies environment overrides (`CIGAR_*`).
    pub fn apply_env(
        &mut self,
        env: &BTreeMap<String, String>,
        root: &Path,
        cig_dir: &Path,
    ) -> Result<()> {
        if let Some(value) = env.get("CIGAR_NAMESPACE") {
            self.engine.namespace = Namespace::parse(value.clone())?;
        }
        if let Some(value) = env.get("CIGAR_GIT_MODE") {
            self.git.mode = value.parse()?;
        }
        if let Some(value) = env.get("CIGAR_EMBEDDING") {
            self.embedding.provider = value.parse()?;
        }
        if let Some(value) = env.get("CIGAR_MODEL") {
            self.embedding.model = value.clone();
        }
        if let Some(value) = env.get("CIGAR_MODEL_DIR") {
            self.embedding.model_dir = expand_path(value, root);
        }
        if let Some(value) = env.get("CIGAR_DIMENSIONS") {
            self.embedding.dimensions = value.parse().map_err(|_| {
                EngineError::config(format!("CIGAR_DIMENSIONS bukan integer: {value}"))
            })?;
        }
        if let Some(value) = env.get("CIGAR_STORE") {
            self.storage.path = expand_path(value, root);
        }
        if let Some(value) = env.get("CIGAR_LOG") {
            self.logging.level = value.clone();
        }
        if env.get("CIGAR_OFFLINE").is_some_and(|v| v == "1") {
            // Recorded by callers; no field to set here.
        }
        if self.storage.path == Path::new(".cig/store.db") {
            self.storage.path = cig_dir.join("store.db");
        }
        self.validate()
    }

    /// Validates every constraint in docs/10 section16.3.
    pub fn validate(&self) -> Result<()> {
        if self.search.mode != "lexical-graph" {
            return Err(EngineError::config(format!(
                "search.mode tidak didukung: {}",
                self.search.mode
            )));
        }
        for (name, value) in [
            ("qualified_name", self.search.weights.qualified_name),
            ("signature", self.search.weights.signature),
            ("docstring", self.search.weights.docstring),
            ("code_tokens", self.search.weights.code_tokens),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(EngineError::config(format!(
                    "search.weights.{name} harus non-negatif dan finite"
                )));
            }
        }
        if self.storage.driver != "sqlite" && self.storage.driver != "sqlite-vec" {
            return Err(EngineError::config(format!(
                "storage.driver tidak didukung: {}",
                self.storage.driver
            )));
        }
        if self.embedding.dimensions == 0 || self.embedding.dimensions > MAX_DIMENSIONS {
            return Err(EngineError::config(format!(
                "embedding.dimensions harus 1..={MAX_DIMENSIONS}, ditemukan {}",
                self.embedding.dimensions
            )));
        }
        if self.embedding.batch_size == 0 || self.embedding.batch_size > 256 {
            return Err(EngineError::config("embedding.batch_size harus 1..=256"));
        }
        if self.embedding.max_tokens == 0 || self.embedding.max_tokens > 8_192 {
            return Err(EngineError::config("embedding.max_tokens harus 1..=8192"));
        }
        if self.embedding.max_code_chars == 0 || self.embedding.max_code_chars > 100_000 {
            return Err(EngineError::config(
                "embedding.max_code_chars harus 1..=100000",
            ));
        }
        if self.embedding.template_version == 0 {
            return Err(EngineError::config("embedding.template_version harus >= 1"));
        }
        if self.query.top_k == 0 || self.query.top_k > 1_000 {
            return Err(EngineError::config("query.top_k harus 1..=1000"));
        }
        if self.query.depth > 3 {
            return Err(EngineError::config("query.depth harus 0..=3"));
        }
        if self.query.max_neighbors == 0 || self.query.max_neighbors > 200 {
            return Err(EngineError::config("query.max_neighbors harus 1..=200"));
        }
        if self.git.max_file_bytes == 0 {
            return Err(EngineError::config("git.max_file_bytes harus > 0"));
        }
        for pattern in &self.git.exclude {
            if pattern.trim().is_empty() {
                return Err(EngineError::config("git.exclude tidak boleh kosong"));
            }
        }
        if self.storage.path.as_os_str().is_empty() {
            return Err(EngineError::config("storage.uri tidak boleh kosong"));
        }
        if !matches!(
            self.logging.level.as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            return Err(EngineError::config(format!(
                "logging.level tidak dikenal: {}",
                self.logging.level
            )));
        }
        Ok(())
    }

    /// Stable config fingerprint used to detect drift that requires reindex.
    pub fn fingerprint(&self) -> Result<crate::domain::ConfigFingerprint> {
        let base = crate::hash::config_fingerprint(
            &self.embedding.model,
            &self.embedding.revision,
            self.embedding.dimensions,
            self.embedding.template_version,
            if self.embedding.normalize {
                "l2"
            } else {
                "none"
            },
        )?;
        if self.search.split_identifiers {
            return Ok(base);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"lexical-config\0");
        hasher.update(base.as_str().as_bytes());
        hasher.update(b"\0split-identifiers=false");
        crate::domain::ConfigFingerprint::parse(hasher.finalize().to_hex().to_string())
    }

    /// Validates a storage URI is a local path inside the repository or CIG
    /// directory, rejecting schemes and escapes (docs/10 section16.3).
    pub fn validate_store_uri(uri: &str, root: &Path) -> Result<PathBuf> {
        let trimmed = uri.trim();
        if trimmed.contains("://") || trimmed.starts_with("file:") {
            return Err(EngineError::config(
                "storage.uri harus path lokal, bukan URL",
            ));
        }
        let expanded = expand_path(trimmed, root);
        let normalized = normalize_lexically(&expanded);
        if normalized
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(EngineError::config(
                "storage.uri tidak boleh keluar dari root repo",
            ));
        }
        Ok(normalized)
    }

    /// Repo paths excluded by config plus fixed internal directories.
    pub fn excludes(&self) -> Result<Vec<RepoPath>> {
        let mut out = Vec::new();
        for pattern in &self.git.exclude {
            let cleaned = pattern.trim().trim_end_matches("/**").trim_end_matches('*');
            let cleaned = cleaned.trim_end_matches('/');
            if cleaned.is_empty() {
                continue;
            }
            if let Ok(path) = RepoPath::parse(cleaned.to_owned()) {
                out.push(path);
            }
        }
        Ok(out)
    }
}

fn apply_embedding(config: &mut EmbeddingConfig, file: EmbeddingFile, root: &Path) {
    if let Some(provider) = file.provider {
        if let Ok(parsed) = provider.parse() {
            config.provider = parsed;
        }
    }
    if let Some(model) = file.model {
        config.model = model;
    }
    if let Some(revision) = file.revision {
        config.revision = revision;
    }
    if let Some(dimensions) = file.dimensions {
        config.dimensions = dimensions;
    }
    if let Some(dir) = file.model_dir {
        config.model_dir = expand_path(&dir, root);
    }
    if let Some(value) = file.batch_size {
        config.batch_size = value;
    }
    if let Some(value) = file.max_tokens {
        config.max_tokens = value;
    }
    if let Some(value) = file.max_code_chars {
        config.max_code_chars = value;
    }
    if let Some(value) = file.template_version {
        config.template_version = value;
    }
    if let Some(value) = file.pooling {
        if let Ok(parsed) = value.parse() {
            config.pooling = parsed;
        }
    }
    if let Some(value) = file.threads {
        config.threads = value;
    }
    if let Some(value) = file.normalize {
        config.normalize = value;
    }
    if let Some(value) = file.query_prefix {
        config.query_prefix = value;
    }
    if let Some(value) = file.document_prefix {
        config.document_prefix = value;
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Escapes a Windows-style path for a TOML basic string.
#[must_use]
pub fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the default `config.toml` written by `cigar init`.
#[must_use]
pub fn render_default_config(config: &Config) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("# CIGAR configuration. Generated by `cigar init`.\n");
    out.push_str("# Precedence: CLI flag > CIGAR_* environment > this file > default.\n\n");
    out.push_str("[engine]\n");
    let _ = writeln!(out, "version = \"{}\"", config.engine.version);
    let _ = writeln!(out, "namespace = \"{}\"", config.engine.namespace);
    out.push_str("\n[git]\n");
    let _ = writeln!(out, "mode = \"{}\"", config.git.mode);
    let _ = writeln!(out, "include_untracked = {}", config.git.include_untracked);
    let _ = writeln!(out, "max_file_bytes = {}", config.git.max_file_bytes);
    let exclude = config
        .git
        .exclude
        .iter()
        .map(|pattern| format!("\"{pattern}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(out, "exclude = [{exclude}]");
    out.push_str("\n[search]\n");
    let _ = writeln!(out, "mode = \"{}\"", config.search.mode);
    let _ = writeln!(
        out,
        "split_identifiers = {}",
        config.search.split_identifiers
    );
    out.push_str("\n[search.weights]\n");
    let _ = writeln!(
        out,
        "qualified_name = {:?}",
        config.search.weights.qualified_name
    );
    let _ = writeln!(out, "signature = {:?}", config.search.weights.signature);
    let _ = writeln!(out, "docstring = {:?}", config.search.weights.docstring);
    let _ = writeln!(out, "code_tokens = {:?}", config.search.weights.code_tokens);
    out.push_str("\n[embedding]\n");
    let _ = writeln!(out, "provider = \"{}\"", config.embedding.provider);
    let _ = writeln!(out, "model = \"{}\"", config.embedding.model);
    let _ = writeln!(out, "revision = \"{}\"", config.embedding.revision);
    let _ = writeln!(out, "dimensions = {}", config.embedding.dimensions);
    let model_dir = escape_toml_string(&config.embedding.model_dir.display().to_string());
    let _ = writeln!(out, "model_dir = \"{model_dir}\"");
    let _ = writeln!(out, "batch_size = {}", config.embedding.batch_size);
    let _ = writeln!(out, "max_tokens = {}", config.embedding.max_tokens);
    let _ = writeln!(out, "max_code_chars = {}", config.embedding.max_code_chars);
    let _ = writeln!(
        out,
        "template_version = {}",
        config.embedding.template_version
    );
    let _ = writeln!(out, "pooling = \"{}\"", config.embedding.pooling);
    let _ = writeln!(out, "threads = {}", config.embedding.threads);
    let _ = writeln!(out, "normalize = {}", config.embedding.normalize);
    let query_prefix = escape_toml_string(&config.embedding.query_prefix);
    let _ = writeln!(out, "query_prefix = \"{query_prefix}\"");
    let document_prefix = escape_toml_string(&config.embedding.document_prefix);
    let _ = writeln!(out, "document_prefix = \"{document_prefix}\"");
    out.push_str("\n[storage]\n");
    let _ = writeln!(out, "driver = \"{}\"", config.storage.driver);
    out.push_str("uri = \".cig/store.db\"\n");
    let _ = writeln!(out, "busy_timeout_ms = {}", config.storage.busy_timeout_ms);
    let _ = writeln!(
        out,
        "wal_checkpoint_threshold_mb = {}",
        config.storage.wal_checkpoint_threshold_mb
    );
    out.push_str("\n[query]\n");
    let _ = writeln!(out, "top_k = {}", config.query.top_k);
    let _ = writeln!(out, "depth = {}", config.query.depth);
    let _ = writeln!(out, "max_tokens = {}", config.query.max_tokens);
    let _ = writeln!(out, "max_neighbors = {}", config.query.max_neighbors);
    out.push_str("\n[logging]\n");
    let _ = writeln!(out, "level = \"{}\"", config.logging.level);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn defaults_are_valid() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.search.mode, "lexical-graph");
        assert_eq!(config.storage.driver, "sqlite");
        assert_eq!(config.embedding.dimensions, 768);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let text = "[embedding]\nmodel = \"x\"\nfuture_key = 1\n";
        let config = Config::from_toml(text, &root(), &root().join(".cig")).unwrap();
        assert_eq!(config.embedding.model, "x");
    }

    #[test]
    fn rejects_bad_dimensions() {
        let text = "[embedding]\ndimensions = 0\n";
        assert!(Config::from_toml(text, &root(), &root().join(".cig")).is_err());
    }

    #[test]
    fn rejects_url_store() {
        assert!(Config::validate_store_uri("https://example.com/db", &root()).is_err());
        assert!(Config::validate_store_uri("file:///tmp/db", &root()).is_err());
    }

    #[test]
    fn store_path_is_under_cig_dir() {
        let cig = root().join(".cig");
        let config = Config::from_toml("", &root(), &cig).unwrap();
        assert_eq!(config.storage.path, cig.join("store.db"));
    }

    #[test]
    fn fingerprint_changes_with_model() {
        let mut a = Config::default();
        let b = a.clone();
        assert_eq!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
        a.embedding.model = "other".into();
        assert_ne!(a.fingerprint().unwrap(), b.fingerprint().unwrap());
    }

    #[test]
    fn env_overrides_win() {
        let mut config = Config::default();
        let mut env = BTreeMap::new();
        env.insert("CIGAR_NAMESPACE".to_owned(), "team-a".to_owned());
        env.insert("CIGAR_DIMENSIONS".to_owned(), "384".to_owned());
        config
            .apply_env(&env, &root(), &root().join(".cig"))
            .unwrap();
        assert_eq!(config.engine.namespace.as_str(), "team-a");
        assert_eq!(config.embedding.dimensions, 384);
    }

    #[test]
    fn renders_parseable_config() {
        let config = Config::default();
        let text = render_default_config(&config);
        let parsed = Config::from_toml(&text, &root(), &root().join(".cig")).unwrap();
        assert_eq!(parsed.embedding.model, config.embedding.model);
        assert_eq!(parsed.search.mode, "lexical-graph");
        assert_eq!(parsed.storage.driver, "sqlite");
    }

    #[test]
    fn search_settings_round_trip_and_validate() {
        let text = "[search]\nsplit_identifiers = false\n[search.weights]\nsignature = 8.5\n";
        let config = Config::from_toml(text, &root(), &root().join(".cig")).unwrap();
        assert!(!config.search.split_identifiers);
        assert_eq!(config.search.weights.signature, 8.5);
        assert_eq!(config.search.weights.qualified_name, 10.0);
        let parsed = Config::from_toml(
            &render_default_config(&config),
            &root(),
            &root().join(".cig"),
        )
        .unwrap();
        assert_eq!(parsed.search, config.search);
        for value in ["-1.0", "nan", "inf"] {
            let text = format!("[search.weights]\ncode_tokens = {value}\n");
            assert!(Config::from_toml(&text, &root(), &root().join(".cig")).is_err());
        }
        let mut defaults = Config::default();
        let fingerprint = defaults.fingerprint().unwrap();
        defaults.search.weights.signature = 42.0;
        assert_eq!(defaults.fingerprint().unwrap(), fingerprint);
        defaults.search.split_identifiers = false;
        assert_ne!(defaults.fingerprint().unwrap(), fingerprint);
    }

    #[test]
    fn unsupported_search_mode_is_rejected() {
        let text = "[search]\nmode = \"hybrid\"\n";
        assert!(Config::from_toml(text, &root(), &root().join(".cig")).is_err());
    }
}
