// SPDX-License-Identifier: MPL-2.0

//! Namespace-scoped lexical sync. Parsing finishes before the single mutation.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    ChangeStatus, CommitId, Namespace, SourceMode, SyncMode, TargetBatch, TargetEdge, TargetFile,
    TargetSymbol,
};
use crate::error::{EngineError, Result};
use crate::git::worktree::{SkipReason, WorktreeScan};
use crate::parse::ParserRegistry;
use crate::traits::{Revision, SourceProvider, TargetStorePort};

#[derive(Clone, Debug, Default)]
pub struct TargetSyncSummary {
    pub files_written: u64,
    pub files_deleted: u64,
    pub symbols_written: u64,
    pub head: Option<String>,
}

/// Build a complete batch and publish files, edges and head in one transaction.
pub fn sync_target(
    store: &dyn TargetStorePort,
    source: &dyn SourceProvider,
    registry: &ParserRegistry,
    config: &crate::config::Config,
    namespace: &str,
    mode: SyncMode,
    full: bool,
    updated_at: i64,
    include_untracked: bool,
    strict: bool,
) -> Result<TargetSyncSummary> {
    Namespace::parse(namespace.to_owned())?;
    let old: BTreeMap<String, String> = store
        .files(namespace)?
        .into_iter()
        .map(|file| (file.file_path, file.blob_hash))
        .collect();
    // Existing v4 indexes without a recorded setting were built with splitting enabled.
    // Rebuild them as well so subsequent queries can verify the setting.
    let tokenizer_changed = store.split_identifiers(namespace)?
        != Some(config.search.split_identifiers)
        && !old.is_empty();
    let full = full || tokenizer_changed;
    let head = source.head()?.map(|id| id.to_string());
    let commit_mode = mode == SyncMode::Commit && source.mode() != SourceMode::FsOnly;
    let mut deleted = BTreeSet::new();
    let mut inputs = Vec::new();
    if commit_mode {
        let previous = store.sync_head(namespace)?;
        let base = if full {
            None
        } else {
            previous
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| CommitId::parse(s.to_owned()))
                .transpose()?
        };
        let changes = source.changes(base.as_ref(), SyncMode::Commit)?;
        if strict && !changes.warnings.is_empty() {
            return Err(EngineError::invalid(format!(
                "strict mode: {} warning",
                changes.warnings.len()
            )));
        }
        if base.is_none() {
            deleted.extend(old.keys().cloned());
        }
        let excludes = config.excludes()?;
        for change in changes.changes {
            if let Some(path) = change.old_path {
                deleted.insert(path.to_string());
            }
            if change.status == ChangeStatus::Deleted {
                deleted.insert(change.path.to_string());
                continue;
            }
            if excludes.iter().any(|prefix| {
                change.path.as_str() == prefix.as_str()
                    || change
                        .path
                        .as_str()
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }) {
                deleted.insert(change.path.to_string());
                continue;
            }
            let Some(language) = registry.for_path(&change.path) else {
                deleted.insert(change.path.to_string());
                continue;
            };
            let bytes = source.read(&change.path, Revision::Head)?;
            if bytes.len() as u64 > config.git.max_file_bytes {
                if strict {
                    return Err(EngineError::invalid(format!(
                        "file terlalu besar: {}",
                        change.path
                    )));
                }
                deleted.insert(change.path.to_string());
                continue;
            }
            let normalized = crate::hash::normalize_file_bytes(&bytes);
            if normalized.iter().take(8000).any(|b| *b == 0) {
                if strict {
                    return Err(EngineError::invalid(format!("file biner: {}", change.path)));
                }
                deleted.insert(change.path.to_string());
                continue;
            }
            let blob = crate::hash::blob_hash(&normalized).to_string();
            let text = match String::from_utf8(normalized) {
                Ok(text) => text,
                Err(_) if !strict => {
                    deleted.insert(change.path.to_string());
                    continue;
                }
                Err(_) => {
                    return Err(EngineError::invalid(format!(
                        "file bukan UTF-8: {}",
                        change.path
                    )))
                }
            };
            inputs.push((change.path, language, blob, text));
        }
    } else {
        let root = source
            .workdir()
            .ok_or_else(|| EngineError::invalid("mode dirty/fs membutuhkan worktree"))?;
        let excludes = config.excludes()?;
        let scan = WorktreeScan::scan_with(
            &root,
            include_untracked,
            config.git.max_file_bytes,
            &excludes,
        )?;
        if tokenizer_changed
            && scan
                .skipped
                .iter()
                .any(|item| item.reason != SkipReason::Unsupported)
        {
            return Err(EngineError::invalid("perubahan split_identifiers membutuhkan reindex lengkap; ada file yang gagal dipindai"));
        }
        if strict
            && scan
                .skipped
                .iter()
                .any(|item| item.reason != SkipReason::Unsupported)
        {
            return Err(EngineError::invalid(
                "strict mode: ada file yang gagal dipindai",
            ));
        }
        let tracked = if !include_untracked && source.mode() != SourceMode::FsOnly {
            Some(
                source
                    .list(Revision::Head)?
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            )
        } else {
            None
        };
        let mut present = BTreeSet::new();
        for item in scan.entries {
            if tracked
                .as_ref()
                .is_some_and(|paths| !paths.contains(&item.path))
            {
                continue;
            }
            present.insert(item.path.to_string());
            let Some(language) = item.language else {
                continue;
            };
            let Some(text) = item.text else { continue };
            let blob = item.blob_hash.to_string();
            if !full && old.get(item.path.as_str()) == Some(&blob) {
                continue;
            }
            inputs.push((item.path, language, blob, text));
        }
        // Preserve skipped files rather than deleting an otherwise valid index on read errors.
        present.extend(
            scan.skipped
                .into_iter()
                .filter(|s| s.reason != SkipReason::Unsupported)
                .map(|s| s.path),
        );
        deleted.extend(old.keys().filter(|path| !present.contains(*path)).cloned());
    }
    let mut files = Vec::with_capacity(inputs.len());
    for (path, language, blob, text) in inputs {
        let parsed =
            registry
                .parse(language, &path, &text)
                .map_err(|error| EngineError::Parse {
                    path: error.path,
                    message: error.message,
                })?;
        let file_id = stable_id(&["file", namespace, path.as_str()]);
        let mut symbols = Vec::with_capacity(parsed.symbols.len());
        for symbol in parsed.symbols {
            let id = stable_id(&[
                "symbol",
                namespace,
                path.as_str(),
                &symbol.qualified_name,
                &symbol.kind.to_string(),
                &symbol.start_line.to_string(),
            ]);
            let mut seen_edges = BTreeSet::new();
            let edges = parsed
                .edges
                .iter()
                .filter(|edge| edge.source_qualified_name == symbol.qualified_name)
                .filter(|edge| {
                    seen_edges.insert((edge.target_name.as_str(), edge.relation.to_string()))
                })
                .map(|edge| TargetEdge {
                    target_name: edge.target_name.clone(),
                    target_symbol_id: None,
                    relation: edge.relation.to_string(),
                })
                .collect();
            symbols.push(TargetSymbol {
                symbol_id: id,
                name: symbol.name.clone(),
                qualified_name: symbol.qualified_name.clone(),
                tokenized_name: normalize_identifiers(
                    &symbol.qualified_name,
                    config.search.split_identifiers,
                ),
                kind: symbol.kind.to_string(),
                signature: symbol.signature,
                docstring: symbol.docstring,
                code_snippet: symbol.code,
                code_tokens: normalize_identifiers(
                    &symbol.body_stream,
                    config.search.split_identifiers,
                ),
                start_line: symbol.start_line,
                end_line: symbol.end_line,
                body_hash: crate::hash::body_hash(&symbol.body_stream).to_string(),
                edges,
            });
        }
        files.push(TargetFile {
            file_id,
            file_path: path.to_string(),
            language: language.to_string(),
            blob_hash: blob,
            imports_text: parsed.imports,
            symbols,
        });
    }
    let summary = TargetSyncSummary {
        files_written: files.len() as u64,
        files_deleted: deleted
            .iter()
            .filter(|path| {
                old.contains_key(*path) && !files.iter().any(|file| &file.file_path == *path)
            })
            .count() as u64,
        symbols_written: files.iter().map(|file| file.symbols.len() as u64).sum(),
        head: head.clone(),
    };
    let batch = TargetBatch {
        namespace: namespace.to_owned(),
        deleted_paths: deleted.into_iter().collect(),
        files,
        commit_hash: if commit_mode {
            head.unwrap_or_default()
        } else {
            String::new()
        },
        updated_at,
        split_identifiers: Some(config.search.split_identifiers),
    };
    store.apply_batch(&batch)?;
    Ok(summary)
}

fn stable_id(parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}

pub(crate) fn normalize_identifiers(text: &str, split: bool) -> String {
    if split {
        return split_identifier(text);
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out
}

pub(crate) fn split_identifier(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (index, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            out.push(' ');
            continue;
        }
        if index > 0 {
            let prev = chars[index - 1];
            let next = chars.get(index + 1).copied();
            if prev.is_alphanumeric()
                && (prev.is_numeric() != c.is_numeric()
                    || (c.is_uppercase() && prev.is_lowercase())
                    || (c.is_uppercase()
                        && prev.is_uppercase()
                        && next.is_some_and(char::is_lowercase)))
            {
                out.push(' ');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TargetFileState;
    use crate::git::worktree::FsSource;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingStore(Mutex<Vec<TargetBatch>>);
    impl TargetStorePort for RecordingStore {
        fn sync_head(&self, _: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn files(&self, _: &str) -> Result<Vec<TargetFileState>> {
            Ok(Vec::new())
        }
        fn apply_batch(&self, batch: &TargetBatch) -> Result<()> {
            self.0.lock().unwrap().push(batch.clone());
            Ok(())
        }
    }

    #[test]
    fn splits_identifiers_acronyms_and_digits() {
        assert_eq!(split_identifier("parseHTML2JSON"), "parse html 2 json");
        assert_eq!(split_identifier("snake_case"), "snake case");
    }

    #[test]
    fn normalization_respects_split_setting() {
        assert_eq!(
            normalize_identifiers("parseHTML2JSON snake_case", true),
            "parse html 2 json snake case"
        );
        assert_eq!(
            normalize_identifiers("parseHTML2JSON snake_case", false),
            "parsehtml2json snake_case"
        );
    }

    #[test]
    fn changed_split_setting_reindexes_unchanged_files() {
        struct ExistingStore(Mutex<Vec<TargetBatch>>, String);
        impl TargetStorePort for ExistingStore {
            fn sync_head(&self, _: &str) -> Result<Option<String>> {
                Ok(None)
            }
            fn files(&self, _: &str) -> Result<Vec<TargetFileState>> {
                Ok(vec![TargetFileState {
                    file_path: "lib.rs".into(),
                    blob_hash: self.1.clone(),
                }])
            }
            fn split_identifiers(&self, _: &str) -> Result<Option<bool>> {
                Ok(Some(true))
            }
            fn apply_batch(&self, batch: &TargetBatch) -> Result<()> {
                self.0.lock().unwrap().push(batch.clone());
                Ok(())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let code = "fn validateToken() {}\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();
        let store = ExistingStore(
            Mutex::new(Vec::new()),
            crate::hash::blob_hash(code.as_bytes()).to_string(),
        );
        let source = FsSource::open(dir.path(), 1024).unwrap();
        let mut config = crate::config::Config::default();
        config.search.split_identifiers = false;
        let result = sync_target(
            &store,
            &source,
            &ParserRegistry::with_defaults(),
            &config,
            "demo",
            SyncMode::Fs,
            false,
            123,
            true,
            false,
        )
        .unwrap();
        assert_eq!(result.files_written, 1);
        let batches = store.0.lock().unwrap();
        assert_eq!(batches[0].split_identifiers, Some(false));
        assert_eq!(
            batches[0].files[0].symbols[0].tokenized_name,
            "validatetoken"
        );
    }

    #[test]
    fn sync_publishes_symbols_and_edges_in_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn caller() { target(); }\nfn target() {}\n",
        )
        .unwrap();
        let source = FsSource::open(dir.path(), 1024).unwrap();
        let store = RecordingStore::default();
        let summary = sync_target(
            &store,
            &source,
            &ParserRegistry::with_defaults(),
            &crate::config::Config::default(),
            "demo",
            SyncMode::Fs,
            false,
            123,
            true,
            false,
        )
        .unwrap();
        let batches = store.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].namespace, "demo");
        assert_eq!(batches[0].updated_at, 123);
        assert_eq!(summary.files_written, 1);
        assert!(!batches[0].files[0].symbols.is_empty());
        assert!(batches[0].files[0]
            .symbols
            .iter()
            .any(|s| s.edges.iter().any(|e| e.target_name == "target")));
        assert!(batches[0].files[0]
            .symbols
            .iter()
            .all(|s| !s.body_hash.is_empty()));
    }
}
