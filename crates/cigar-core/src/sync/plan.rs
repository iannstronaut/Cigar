//! Plan building: turn a delta into the content and branch batches.
//!
//! This is the pure step between parsing and writing: it decides which shared
//! rows are new, renders embedding templates, and de-duplicates vectors by
//! `embedding_key` (docs/04 section 4.5, docs/21 section 4).

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    BlobHash, BranchBatch, BranchEdgeRow, BranchFileContent, BranchName, BranchSymbolRow,
    ContentBatch, ContentFile, EmbeddingKey, Language, Namespace, ParsedSymbolContent, RepoPath,
    SymbolDef, SymbolHash,
};
use crate::hash::{symbol_hash, EmbeddingTemplate};
use crate::parse::ParserRegistry;
use crate::traits::Embedder;

/// One parsed file waiting to be written.
#[derive(Clone, Debug)]
pub struct PlannedFile {
    pub path: RepoPath,
    pub blob_hash: BlobHash,
    pub language: Language,
    pub imports: String,
    pub size_bytes: u64,
    pub symbols: Vec<PlannedSymbol>,
    pub edges: Vec<PlannedEdge>,
}

/// One symbol with its rendered template.
#[derive(Clone, Debug)]
pub struct PlannedSymbol {
    pub definition: SymbolDef,
    pub start_line: u32,
    pub end_line: u32,
    pub embedding_key: EmbeddingKey,
    pub embedding_text: String,
    pub template_truncated: bool,
}

/// One edge keyed by qualified source name until hashes are known.
#[derive(Clone, Debug)]
pub struct PlannedEdge {
    pub source_qualified_name: String,
    pub target_name: String,
    pub relation: crate::domain::EdgeRelation,
}

/// Everything needed to write one branch.
#[derive(Clone, Debug, Default)]
pub struct BranchPlan {
    pub content: Vec<PlannedFile>,
    pub removed_paths: Vec<RepoPath>,
    /// Every `(path, blob, language)` that must be present after the sync.
    pub manifest: Vec<crate::domain::BranchFile>,
    /// Vectors that are not yet in the store.
    pub pending_vectors: Vec<(EmbeddingKey, String)>,
    pub reused_vectors: u64,
    pub warnings: Vec<String>,
}

impl BranchPlan {
    #[must_use]
    pub fn content_batch(&self) -> ContentBatch {
        ContentBatch {
            files: self
                .content
                .iter()
                .map(|file| ContentFile {
                    blob_hash: file.blob_hash.clone(),
                    language: file.language,
                    imports: file.imports.clone(),
                    size_bytes: file.size_bytes,
                    symbols: file
                        .symbols
                        .iter()
                        .map(|symbol| ParsedSymbolContent {
                            symbol: symbol.definition.clone(),
                            start_line: symbol.start_line,
                            end_line: symbol.end_line,
                        })
                        .collect(),
                    edges: file
                        .edges
                        .iter()
                        .filter_map(|edge| {
                            let source = file
                                .symbols
                                .iter()
                                .find(|symbol| {
                                    symbol.definition.qualified_name == edge.source_qualified_name
                                })
                                .map(|symbol| symbol.definition.symbol_hash.clone())?;
                            Some(crate::domain::SymbolEdge {
                                source_symbol_hash: source,
                                target_name: edge.target_name.clone(),
                                relation: edge.relation,
                            })
                        })
                        .collect(),
                })
                .collect(),
            removed_blobs: Vec::new(),
        }
    }

    /// Builds the branch batch, resolving edge sources to symbol hashes.
    #[must_use]
    pub fn branch_batch(&self, removed_from_other_sources: Vec<RepoPath>) -> BranchBatch {
        let mut removed = self.removed_paths.clone();
        removed.extend(removed_from_other_sources);
        removed.sort();
        removed.dedup();

        let files = self
            .content
            .iter()
            .map(|file| {
                let by_name: BTreeMap<&str, &SymbolHash> = file
                    .symbols
                    .iter()
                    .map(|symbol| {
                        (
                            symbol.definition.qualified_name.as_str(),
                            &symbol.definition.symbol_hash,
                        )
                    })
                    .collect();
                let symbols = file
                    .symbols
                    .iter()
                    .map(|symbol| BranchSymbolRow {
                        symbol_hash: symbol.definition.symbol_hash.clone(),
                        start_line: symbol.start_line,
                        end_line: symbol.end_line,
                        embedding_key: symbol.embedding_key.clone(),
                        embedding_text: symbol.embedding_text.clone(),
                    })
                    .collect();
                let edges = file
                    .edges
                    .iter()
                    .filter_map(|edge| {
                        let source = by_name.get(edge.source_qualified_name.as_str())?;
                        Some(BranchEdgeRow {
                            source_symbol_hash: (*source).clone(),
                            target_name: edge.target_name.clone(),
                            relation: edge.relation,
                            target_symbol_hash: None,
                        })
                    })
                    .collect();
                BranchFileContent {
                    file_path: file.path.clone(),
                    blob_hash: file.blob_hash.clone(),
                    language: file.language,
                    symbols,
                    edges,
                }
            })
            .collect();

        BranchBatch {
            files,
            removed_files: removed,
            state: None,
        }
    }
}

/// Renders templates and computes symbol hashes for one parsed file.
pub fn plan_file(
    path: &RepoPath,
    blob_hash: BlobHash,
    language: Language,
    imports: String,
    size_bytes: u64,
    parsed: &crate::traits::ParsedFile,
    embedder: &dyn Embedder,
    template_version: u32,
    content_version: u32,
) -> PlannedFile {
    let mut symbols = Vec::with_capacity(parsed.symbols.len());
    let mut seen: BTreeSet<(String, crate::domain::SymbolKind)> = BTreeSet::new();
    for parsed_symbol in &parsed.symbols {
        // Duplicate qualified names inside one file are disambiguated by kind;
        // identical pairs are merged (docs/07 section 4.6).
        if !seen.insert((parsed_symbol.qualified_name.clone(), parsed_symbol.kind)) {
            continue;
        }
        let hash = symbol_hash(
            language,
            parsed_symbol.kind,
            &parsed_symbol.qualified_name,
            &parsed_symbol.signature,
            parsed_symbol.docstring.as_deref(),
            &parsed_symbol.body_stream,
            content_version,
        );
        let definition = SymbolDef {
            symbol_hash: hash,
            language,
            name: parsed_symbol.name.clone(),
            qualified_name: parsed_symbol.qualified_name.clone(),
            kind: parsed_symbol.kind,
            signature: parsed_symbol.signature.clone(),
            docstring: parsed_symbol.docstring.clone(),
            code_snippet: parsed_symbol.code.clone(),
            body_hash: crate::hash::body_hash(&parsed_symbol.body_stream),
        };
        let (template, key) = embedder.template_for(path, &imports, &definition, template_version);
        symbols.push(PlannedSymbol {
            definition,
            start_line: parsed_symbol.start_line,
            end_line: parsed_symbol.end_line,
            embedding_key: key,
            embedding_text: template.text().to_owned(),
            template_truncated: template.truncated_code() || template.truncated_header(),
        });
    }
    PlannedFile {
        path: path.clone(),
        blob_hash,
        language,
        imports,
        size_bytes,
        symbols,
        edges: parsed
            .edges
            .iter()
            .map(|edge| PlannedEdge {
                source_qualified_name: edge.source_qualified_name.clone(),
                target_name: edge.target_name.clone(),
                relation: edge.relation,
            })
            .collect(),
    }
}

/// Parses one file's source and renders its symbols.
#[allow(clippy::too_many_arguments)]
pub fn plan_source(
    registry: &ParserRegistry,
    path: &RepoPath,
    blob_hash: BlobHash,
    language: Language,
    source: &str,
    size_bytes: u64,
    embedder: &dyn Embedder,
    template_version: u32,
    content_version: u32,
) -> crate::Result<PlannedFile> {
    let parsed =
        registry
            .parse(language, path, source)
            .map_err(|err| crate::EngineError::Parse {
                path: err.path,
                message: err.message,
            })?;
    Ok(plan_file(
        path,
        blob_hash,
        language,
        parsed.imports.clone(),
        size_bytes,
        &parsed,
        embedder,
        template_version,
        content_version,
    ))
}

/// Collects pending vectors for a set of planned files, skipping keys that are
/// already stored.
pub fn pending_vectors(
    files: &[PlannedFile],
    existing: &dyn Fn(&EmbeddingKey) -> bool,
) -> (Vec<(EmbeddingKey, String)>, u64) {
    let mut seen: BTreeSet<&EmbeddingKey> = BTreeSet::new();
    let mut pending = Vec::new();
    let mut reused = 0_u64;
    for file in files {
        for symbol in &file.symbols {
            if !seen.insert(&symbol.embedding_key) {
                continue;
            }
            if existing(&symbol.embedding_key) {
                reused += 1;
            } else {
                pending.push((symbol.embedding_key.clone(), symbol.embedding_text.clone()));
            }
        }
    }
    (pending, reused)
}

/// Builds a manifest entry per planned file plus unchanged entries.
#[must_use]
pub fn build_manifest(
    planned: &[PlannedFile],
    unchanged: &[crate::domain::BranchFile],
) -> Vec<crate::domain::BranchFile> {
    let planned_paths: BTreeSet<&RepoPath> = planned.iter().map(|file| &file.path).collect();
    let mut manifest: Vec<crate::domain::BranchFile> = unchanged
        .iter()
        .filter(|file| !planned_paths.contains(&file.file_path))
        .cloned()
        .collect();
    manifest.extend(planned.iter().map(|file| crate::domain::BranchFile {
        file_path: file.path.clone(),
        blob_hash: file.blob_hash.clone(),
        language: file.language,
    }));
    manifest.sort_by(|a, b| a.file_path.cmp(&b.file_path));
    manifest
}

/// Convenience for tests and callers that only need the template text.
#[must_use]
pub fn render_template(path: &RepoPath, imports: &str, symbol: &SymbolDef) -> EmbeddingTemplate {
    EmbeddingTemplate::render(path, imports, symbol)
}

/// Marker used by the pipeline to keep the namespace type in signatures.
pub type PlanNamespace = Namespace;
/// Marker for branch type in plan signatures.
pub type PlanBranch = BranchName;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BranchFile, SymbolDef, SymbolKind};
    use crate::parse::ParserRegistry;
    use std::sync::Mutex;

    /// Local fake embedder so core tests never depend on `cigar-embed`.
    struct Fake {
        dimensions: usize,
        seen: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                dimensions: 8,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Embedder for Fake {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dimensions(&self) -> usize {
            self.dimensions
        }
        fn max_tokens(&self) -> usize {
            512
        }
        fn embed_batch(&self, texts: &[String]) -> crate::Result<Vec<Vec<f32>>> {
            self.seen.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts
                .iter()
                .map(|text| {
                    let mut out = vec![0.0_f32; self.dimensions];
                    let mut hash = blake3::Hasher::new();
                    hash.update(text.as_bytes());
                    let digest = hash.finalize();
                    for (index, byte) in digest.as_bytes().iter().take(self.dimensions).enumerate()
                    {
                        out[index] = f32::from(*byte) / 255.0;
                    }
                    out
                })
                .collect())
        }
    }

    #[test]
    fn plan_file_hashes_and_renders() {
        let registry = ParserRegistry::with_defaults();
        let path = RepoPath::parse("src/auth.rs").unwrap();
        let source = "use axum::http::Request;\n/// Checks the token.\npub fn authenticate(req: Request) -> bool {\n    validate(req)\n}\nfn validate(_r: Request) -> bool { true }\n";
        let embedder = Fake::new();
        let planned = plan_source(
            &registry,
            &path,
            BlobHash::parse(format!("{:064x}", 1)).unwrap(),
            Language::Rust,
            source,
            128,
            &embedder,
            1,
            1,
        )
        .unwrap();
        assert_eq!(planned.symbols.len(), 2);
        let auth = planned
            .symbols
            .iter()
            .find(|symbol| symbol.definition.name == "authenticate")
            .unwrap();
        assert!(auth.embedding_text.contains("path: src/auth.rs"));
        assert!(
            auth.embedding_text
                .contains("imports: use axum::http::Request;"),
            "template: {}",
            auth.embedding_text
        );
        assert!(auth.embedding_text.contains("doc: Checks the token."));
        // login: `authenticate` -> validate (CALLS); plus USES_TYPE for the\n        // Request parameter and return-less body reference.\n        assert!(\n            planned.edges.iter().any(|edge| {\n                edge.source_qualified_name == "authenticate"\n                    && edge.target_name == "validate"\n                    && edge.relation == crate::domain::EdgeRelation::Calls\n            }),\n            "edges: {:?}",\n            planned.edges\n        );
        assert_eq!(planned.edges[0].source_qualified_name, "authenticate");
        assert_eq!(planned.edges[0].target_name, "validate");
    }

    #[test]
    fn pending_vectors_dedupes_and_skips_existing() {
        let registry = ParserRegistry::with_defaults();
        let path = RepoPath::parse("src/a.rs").unwrap();
        let embedder = Fake::new();
        let planned = plan_source(
            &registry,
            &path,
            BlobHash::parse(format!("{:064x}", 2)).unwrap(),
            Language::Rust,
            "fn a() {}\n",
            10,
            &embedder,
            1,
            1,
        )
        .unwrap();
        let duplicate = planned.clone();
        let (pending, reused) = pending_vectors(&[planned, duplicate], &|_key| false);
        assert_eq!(pending.len(), 1);
        assert_eq!(reused, 0);

        let again = plan_source(
            &registry,
            &path,
            BlobHash::parse(format!("{:064x}", 3)).unwrap(),
            Language::Rust,
            "fn a() {}\n",
            10,
            &embedder,
            1,
            1,
        )
        .unwrap();
        let (pending, reused) = pending_vectors(&[again], &|_key| true);
        assert!(pending.is_empty());
        assert_eq!(reused, 1);
    }

    #[test]
    fn manifest_merges_unchanged_and_planned() {
        let planned = PlannedFile {
            path: RepoPath::parse("src/new.rs").unwrap(),
            blob_hash: BlobHash::parse(format!("{:064x}", 3)).unwrap(),
            language: Language::Rust,
            imports: String::new(),
            size_bytes: 1,
            symbols: vec![],
            edges: vec![],
        };
        let unchanged = vec![BranchFile {
            file_path: RepoPath::parse("src/old.rs").unwrap(),
            blob_hash: BlobHash::parse(format!("{:064x}", 4)).unwrap(),
            language: Language::Rust,
        }];
        let manifest = build_manifest(&[planned], &unchanged);
        assert_eq!(manifest.len(), 2);
        assert_eq!(manifest[0].file_path.as_str(), "src/new.rs");
        assert_eq!(manifest[1].file_path.as_str(), "src/old.rs");
    }

    #[test]
    fn template_hash_matches_key() {
        let symbol = SymbolDef {
            symbol_hash: SymbolHash::parse(format!("{:064x}", 5)).unwrap(),
            language: Language::Rust,
            name: "f".into(),
            qualified_name: "f".into(),
            kind: SymbolKind::Function,
            signature: "fn f()".into(),
            docstring: None,
            code_snippet: "fn f() {}".into(),
            body_hash: crate::domain::Digest::parse(format!("{:064x}", 6)).unwrap(),
        };
        let path = RepoPath::parse("src/f.rs").unwrap();
        let template = render_template(&path, "", &symbol);
        let key = template.key(1);
        assert_eq!(key, crate::hash::embedding_key(1, template.text()));
    }

    // Keep the unused import visible to readers: it documents that core tests
    // intentionally avoid depending on `cigar-embed`.
    #[allow(dead_code)]
    type _Unused = Fake;
}
