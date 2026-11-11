//! Query pipeline: retrieval, graph expansion, and context packing
//! (docs/05 sections 4-8).
//!
//! Ranking is cosine-only; the graph adds context but never reorders primary
//! results. Packing is budget-aware and never truncates mid-line.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    EdgeRelation, GraphNeighborhood, Language, OccurrenceRef, RelatedOccurrence, RepoPath,
    ScoredOccurrence, SymbolKind, SymbolOccurrence,
};
use crate::error::{EngineError, Result};
use crate::traits::{ContextStore, Embedder};
use crate::SCHEMA_VERSION;

/// Query options after CLI/config resolution.
#[derive(Clone, Debug)]
pub struct QueryOptions {
    pub top_k: usize,
    pub depth: u8,
    pub max_tokens: usize,
    pub max_neighbors: usize,
    pub path_globs: Vec<String>,
    pub kinds: Vec<SymbolKind>,
    pub min_score: Option<f32>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            top_k: 5,
            depth: 1,
            max_tokens: 6_000,
            max_neighbors: 8,
            path_globs: Vec::new(),
            kinds: Vec::new(),
            min_score: None,
        }
    }
}

/// Index metadata included in every response.
#[derive(Clone, Debug)]
pub struct IndexInfo {
    pub namespace: String,
    pub branch: String,
    pub head: Option<String>,
    pub mode: String,
    pub status: String,
    pub model: String,
    pub stale: bool,
}

/// One ranked result with its relations and rendered code.
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub rank: usize,
    pub score: f32,
    pub distance: f32,
    pub occurrence: SymbolOccurrence,
    pub imports: String,
    pub code: String,
    pub code_truncated: bool,
    pub relations: Relations,
}

/// Relations for one result.
#[derive(Clone, Debug, Default)]
pub struct Relations {
    pub callers: Vec<RelatedView>,
    pub callees: Vec<RelatedView>,
    pub types_used: Vec<RelatedView>,
    pub implements: Vec<RelatedView>,
    pub implemented_by: Vec<RelatedView>,
    pub truncated: bool,
}

/// A neighbor rendered signature-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedView {
    pub symbol_hash: String,
    pub qualified_name: String,
    pub kind: String,
    pub signature: String,
    pub file: String,
    pub start_line: u32,
    pub relation: String,
}

/// Response statistics.
#[derive(Clone, Debug, Default)]
pub struct QueryStats {
    pub candidates: usize,
    pub results: usize,
    pub tokens_estimated: usize,
    pub elapsed_ms: u64,
    pub truncated: bool,
}

/// Full query response.
#[derive(Clone, Debug)]
pub struct QueryResponse {
    pub query: String,
    pub index: IndexInfo,
    pub results: Vec<QueryResult>,
    pub stats: QueryStats,
    pub warnings: Vec<String>,
}

impl QueryResponse {
    /// Stable JSON projection (schema v2, docs/05 section 8.2).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "query": self.query,
            "index": {
                "namespace": self.index.namespace,
                "branch": self.index.branch,
                "head": self.index.head,
                "mode": self.index.mode,
                "status": self.index.status,
                "model": self.index.model,
                "stale": self.index.stale,
            },
            "results": self.results.iter().map(result_json).collect::<Vec<_>>(),
            "stats": {
                "candidates": self.stats.candidates,
                "results": self.stats.results,
                "tokens_estimated": self.stats.tokens_estimated,
                "elapsed_ms": self.stats.elapsed_ms,
                "truncated": self.stats.truncated,
            },
            "warnings": self.warnings,
        })
    }
}

fn result_json(result: &QueryResult) -> serde_json::Value {
    serde_json::json!({
        "rank": result.rank,
        "score": round3(result.score),
        "distance": round3(result.distance),
        "embedding_key": result.occurrence.embedding_key.to_string(),
        "symbol": {
            "symbol_hash": result.occurrence.symbol.symbol_hash.to_string(),
            "name": result.occurrence.symbol.name,
            "qualified_name": result.occurrence.symbol.qualified_name,
            "kind": result.occurrence.symbol.kind.to_string(),
            "signature": result.occurrence.symbol.signature,
            "doc": result.occurrence.symbol.docstring,
            "file": result.occurrence.file_path.to_string(),
            "language": result.occurrence.symbol.language.to_string(),
            "start_line": result.occurrence.start_line,
            "end_line": result.occurrence.end_line,
            "code": result.code,
            "imports": result.imports,
        },
        "relations": {
            "callers": result.relations.callers.iter().map(related_json).collect::<Vec<_>>(),
            "callees": result.relations.callees.iter().map(related_json).collect::<Vec<_>>(),
            "types_used": result.relations.types_used.iter().map(related_json).collect::<Vec<_>>(),
            "implements": result.relations.implements.iter().map(related_json).collect::<Vec<_>>(),
            "implemented_by": result.relations.implemented_by.iter().map(related_json).collect::<Vec<_>>(),
            "truncated": result.relations.truncated,
        },
    })
}

fn related_json(view: &RelatedView) -> serde_json::Value {
    serde_json::json!({
        "symbol_hash": view.symbol_hash,
        "qualified_name": view.qualified_name,
        "kind": view.kind,
        "signature": view.signature,
        "file": view.file,
        "start_line": view.start_line,
        "relation": view.relation,
    })
}

fn round3(value: f32) -> f64 {
    (f64::from(value) * 1000.0).round() / 1000.0
}

/// Runs the full query pipeline.
pub struct QueryEngine<'a> {
    store: &'a dyn ContextStore,
    embedder: Option<&'a dyn Embedder>,
}

impl<'a> QueryEngine<'a> {
    /// Creates a query engine using an optional semantic backend. When absent,
    /// queries are served entirely by the deterministic lexical graph index.
    pub fn new(store: &'a dyn ContextStore, embedder: Option<&'a dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// Backwards-compatible constructor for callers that explicitly select a
    /// semantic embedder.
    pub fn with_embedder(store: &'a dyn ContextStore, embedder: &'a dyn Embedder) -> Self {
        Self::new(store, Some(embedder))
    }

    /// Executes a query against one branch.
    pub fn run(
        &self,
        namespace: &crate::domain::Namespace,
        branch: &crate::domain::BranchName,
        query: &str,
        options: &QueryOptions,
        index: IndexInfo,
    ) -> Result<QueryResponse> {
        let started = std::time::Instant::now();
        let query = validate_query(query)?;
        if options.max_tokens < 256 {
            return Err(EngineError::invalid(
                "max_tokens harus >= 256 agar konteks berguna",
            ));
        }
        if options.top_k == 0 {
            return Err(EngineError::invalid("top_k harus >= 1"));
        }

        let mut warnings = Vec::new();
        let filter = Filter::new(&options.path_globs, &options.kinds)?;

        let mut matches = if let Some(embedder) = self.embedder {
            let vector = embedder.embed_query(&query)?;
            if vector.len() != embedder.dimensions() {
                return Err(EngineError::DimensionMismatch {
                    expected: embedder.dimensions(),
                    actual: vector.len(),
                });
            }
            self.retrieve(namespace, branch, &vector, options, &filter, &mut warnings)?
        } else {
            self.retrieve_lexical(namespace, branch, &query, options, &filter, &mut warnings)?
        };
        if matches.len() < options.top_k && filter.is_active() {
            warnings.push("few_results".into());
        }

        // Graph expansion and imports for the primary results.
        let mut results = Vec::new();
        let mut imports_by_file: BTreeMap<RepoPath, String> = BTreeMap::new();
        for (rank, hit) in matches.iter_mut().enumerate() {
            let occurrence = self
                .store
                .get_occurrence(
                    namespace,
                    &OccurrenceRef {
                        branch: branch.clone(),
                        file_path: hit.file_path.clone(),
                        symbol_hash: hit.symbol_hash.clone(),
                        start_line: hit.start_line,
                    },
                )?
                .ok_or_else(|| EngineError::NotFound {
                    entity: "simbol".into(),
                    id: hit.symbol_hash.to_string(),
                })?;

            let imports = if let Some(existing) = imports_by_file.get(&hit.file_path) {
                let _ = existing;
                existing.clone()
            } else {
                let value = self.imports_for(namespace, branch, &hit.file_path)?;
                imports_by_file.insert(hit.file_path.clone(), value.clone());
                value
            };

            let neighborhood = if options.depth == 0 {
                GraphNeighborhood::default()
            } else {
                self.store.get_neighbors(
                    namespace,
                    branch,
                    &OccurrenceRef {
                        branch: branch.clone(),
                        file_path: hit.file_path.clone(),
                        symbol_hash: hit.symbol_hash.clone(),
                        start_line: hit.start_line,
                    },
                    options.depth,
                )?
            };

            results.push(QueryResult {
                rank: rank + 1,
                score: hit.score,
                distance: hit.distance,
                occurrence,
                imports,
                code: hit.pending_code.clone(),
                code_truncated: false,
                relations: self.relations_of(
                    namespace,
                    branch,
                    neighborhood,
                    options.max_neighbors,
                )?,
            });
        }

        let candidates = matches.len();
        let packed = pack_budget(results, options.max_tokens, self.embedder, &index, &query);

        let response = QueryResponse {
            query,
            index,
            results: packed.results,
            stats: QueryStats {
                candidates,
                results: 0,
                tokens_estimated: packed.tokens,
                elapsed_ms: started.elapsed().as_millis() as u64,
                truncated: packed.truncated,
            },
            warnings,
        };
        let mut response = response;
        response.stats.results = response.results.len();
        Ok(response)
    }

    #[allow(clippy::too_many_arguments)]
    fn retrieve(
        &self,
        namespace: &crate::domain::Namespace,
        branch: &crate::domain::BranchName,
        vector: &[f32],
        options: &QueryOptions,
        filter: &Filter,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<ScoredOccurrence>> {
        // Over-fetch before joining the branch manifest (docs/05 section 5.1).
        let mut k = (options.top_k * 8).clamp(64, 500);
        if filter.is_active() {
            k = (k * 2).min(500);
        }
        let mut accepted: Vec<ScoredOccurrence> = Vec::new();
        let mut seen: BTreeSet<(String, String, u32)> = BTreeSet::new();

        for round in 0..3 {
            let hits = self.store.search_symbols(namespace, branch, vector, k)?;
            for hit in hits {
                if !hit.distance.is_finite() {
                    warnings.push("dropped_non_finite_distance".into());
                    continue;
                }
                if let Some(min_score) = options.min_score {
                    if hit.score < min_score {
                        continue;
                    }
                }
                if !filter.accepts_path(&hit) {
                    continue;
                }
                // Kind lives on the symbol row, not in the KNN pool, so it is
                // resolved here before the hit can be accepted.
                let occurrence = match self.store.get_occurrence(
                    namespace,
                    &OccurrenceRef {
                        branch: branch.clone(),
                        file_path: hit.file_path.clone(),
                        symbol_hash: hit.symbol_hash.clone(),
                        start_line: hit.start_line,
                    },
                )? {
                    Some(occurrence) => occurrence,
                    None => {
                        warnings.push("dropped_ghost_occurrence".into());
                        continue;
                    }
                };
                if !filter.accepts_kind(occurrence.symbol.kind) {
                    continue;
                }
                let key = (
                    hit.file_path.to_string(),
                    hit.symbol_hash.to_string(),
                    hit.start_line,
                );
                if seen.insert(key) {
                    let mut hit = hit;
                    hit.pending_code = occurrence.symbol.code_snippet;
                    accepted.push(hit);
                }
            }
            if accepted.len() >= options.top_k || k >= 500 {
                break;
            }
            if round == 2 {
                break;
            }
            k = (k * 3).min(500);
        }

        accepted.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.symbol_hash.cmp(&b.symbol_hash))
        });
        accepted.truncate(options.top_k);

        Ok(accepted)
    }

    #[allow(clippy::too_many_arguments)]
    fn retrieve_lexical(
        &self,
        namespace: &crate::domain::Namespace,
        branch: &crate::domain::BranchName,
        query: &str,
        options: &QueryOptions,
        filter: &Filter,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<ScoredOccurrence>> {
        let hits = self.store.search_lexical(
            namespace,
            branch,
            query,
            (options.top_k * 8).clamp(64, 500),
        )?;
        let mut accepted = Vec::with_capacity(options.top_k);
        let mut seen = BTreeSet::new();
        for mut hit in hits {
            if !hit.score.is_finite() || !hit.distance.is_finite() {
                warnings.push("dropped_non_finite_score".into());
                continue;
            }
            if let Some(min_score) = options.min_score {
                if hit.score < min_score {
                    continue;
                }
            }
            if !filter.accepts_path(&hit) {
                continue;
            }
            let Some(occurrence) = self.store.get_occurrence(
                namespace,
                &OccurrenceRef {
                    branch: branch.clone(),
                    file_path: hit.file_path.clone(),
                    symbol_hash: hit.symbol_hash.clone(),
                    start_line: hit.start_line,
                },
            )?
            else {
                warnings.push("dropped_ghost_occurrence".into());
                continue;
            };
            if !filter.accepts_kind(occurrence.symbol.kind) {
                continue;
            }
            let key = (
                hit.file_path.to_string(),
                hit.symbol_hash.to_string(),
                hit.start_line,
            );
            if seen.insert(key) {
                hit.pending_code = occurrence.symbol.code_snippet;
                accepted.push(hit);
            }
            if accepted.len() >= options.top_k {
                break;
            }
        }
        Ok(accepted)
    }

    fn imports_for(
        &self,
        namespace: &crate::domain::Namespace,
        branch: &crate::domain::BranchName,
        path: &RepoPath,
    ) -> Result<String> {
        // Imports live on the blob row; read them through the manifest so a
        // branch that no longer contains the file cannot leak imports.
        let manifest = self.store.get_branch_manifest(namespace, branch)?;
        for file in manifest {
            if &file.file_path == path {
                let symbols = self.store.blob_symbols(&file.blob_hash, file.language)?;
                return Ok(symbols
                    .first()
                    .map(|row| row.imports.clone())
                    .unwrap_or_default());
            }
        }
        Ok(String::new())
    }

    fn relations_of(
        &self,
        namespace: &crate::domain::Namespace,
        _branch: &crate::domain::BranchName,
        neighborhood: GraphNeighborhood,
        max_neighbors: usize,
    ) -> Result<Relations> {
        let mut relations = Relations {
            truncated: neighborhood.truncated,
            ..Relations::default()
        };

        let add = |target: &mut Vec<RelatedView>,
                   list: Vec<RelatedOccurrence>,
                   relation: &str|
         -> Result<()> {
            for item in list.into_iter().take(max_neighbors) {
                let Some(occurrence) = self.store.get_occurrence(namespace, &item.occurrence)?
                else {
                    continue;
                };
                target.push(RelatedView {
                    symbol_hash: occurrence.symbol.symbol_hash.to_string(),
                    qualified_name: occurrence.symbol.qualified_name.clone(),
                    kind: occurrence.symbol.kind.to_string(),
                    signature: occurrence.symbol.signature.clone(),
                    file: occurrence.file_path.to_string(),
                    start_line: occurrence.start_line,
                    relation: relation.to_owned(),
                });
            }
            Ok(())
        };

        add(
            &mut relations.callers,
            neighborhood.callers,
            EdgeRelation::Calls.as_str(),
        )?;
        add(
            &mut relations.callees,
            neighborhood.callees,
            EdgeRelation::Calls.as_str(),
        )?;
        add(
            &mut relations.types_used,
            neighborhood.types_used,
            EdgeRelation::UsesType.as_str(),
        )?;
        add(
            &mut relations.implements,
            neighborhood.implements,
            EdgeRelation::Implements.as_str(),
        )?;
        add(
            &mut relations.implemented_by,
            neighborhood.implemented_by,
            EdgeRelation::Implements.as_str(),
        )?;

        // Dedupe across directions, keeping the first relation seen.
        let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
        for list in [
            &mut relations.callers,
            &mut relations.callees,
            &mut relations.types_used,
            &mut relations.implements,
            &mut relations.implemented_by,
        ] {
            list.retain(|view| seen.insert((view.file.clone(), view.symbol_hash.clone())));
        }
        Ok(relations)
    }
}

fn validate_query(query: &str) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(EngineError::invalid("query tidak boleh kosong"));
    }
    if trimmed.chars().count() > 2_000 {
        return Err(EngineError::invalid("query melebihi 2000 karakter"));
    }
    Ok(trimmed.replace("\r\n", "\n"))
}

struct Filter {
    globs: Option<globset::GlobSet>,
    kinds: Vec<SymbolKind>,
}

impl Filter {
    fn new(patterns: &[String], kinds: &[SymbolKind]) -> Result<Self> {
        let globs = if patterns.is_empty() {
            None
        } else {
            let mut builder = globset::GlobSetBuilder::new();
            for pattern in patterns {
                let glob = globset::Glob::new(pattern).map_err(|err| {
                    EngineError::invalid(format!("glob `{pattern}` tidak valid: {err}"))
                })?;
                builder.add(glob);
            }
            Some(
                builder
                    .build()
                    .map_err(|err| EngineError::invalid(format!("glob tidak valid: {err}")))?,
            )
        };
        Ok(Self {
            globs,
            kinds: kinds.to_vec(),
        })
    }

    fn is_active(&self) -> bool {
        self.globs.is_some() || !self.kinds.is_empty()
    }

    /// Path-only predicate. Kind is checked against the loaded occurrence
    /// because the KNN pool has no kind column (docs/05 section 5.3).
    fn accepts_path(&self, hit: &ScoredOccurrence) -> bool {
        match &self.globs {
            Some(globs) => globs.is_match(hit.file_path.as_str()),
            None => true,
        }
    }

    fn accepts_kind(&self, kind: SymbolKind) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind)
    }
}

/// Result of budget packing.
struct Packed {
    results: Vec<QueryResult>,
    tokens: usize,
    truncated: bool,
}

/// Applies the 70/20/10 budget (docs/05 section 8.4).
fn pack_budget(
    results: Vec<QueryResult>,
    max_tokens: usize,
    embedder: Option<&dyn Embedder>,
    index: &IndexInfo,
    query: &str,
) -> Packed {
    let count = |text: &str| {
        embedder.map_or_else(|| text.split_whitespace().count(), |e| e.count_tokens(text))
    };
    let header = render_header(query, index, 0, max_tokens);
    let header_tokens = count(&header);
    let remaining = max_tokens.saturating_sub(header_tokens);
    let primary_budget = remaining * 70 / 100;
    let relation_budget = remaining * 20 / 100;

    let mut packed = Vec::new();
    let mut used = 0_usize;
    let mut truncated = false;

    for mut result in results {
        let relation_cost = count(&render_relations_plain(&result.relations));
        let full_cost = count(&render_result(&result));

        if packed.is_empty() {
            // The top result always appears, truncated if necessary.
            let mut budget_for_relations = relation_budget.min(relation_cost);
            if used + full_cost + budget_for_relations <= primary_budget + relation_budget {
                used += full_cost + budget_for_relations;
                packed.push(result);
                continue;
            }
            // Shrink relations first, then code, then signature only.
            while budget_for_relations > 0
                && used + full_cost + budget_for_relations > primary_budget + relation_budget
            {
                budget_for_relations = budget_for_relations.saturating_sub(8);
            }
            trim_relations(&mut result.relations, budget_for_relations, &count);
            let mut cost = count(&render_result(&result));
            if used + cost > primary_budget + relation_budget {
                truncate_code(&mut result, primary_budget.saturating_sub(used), &count);
                result.code_truncated = true;
                cost = count(&render_result(&result));
            }
            if used + cost > primary_budget + relation_budget && !packed.is_empty() {
                truncated = true;
                break;
            }
            used += cost;
            packed.push(result);
            continue;
        }

        if used + full_cost > primary_budget + relation_budget {
            truncated = true;
            continue;
        }
        used += full_cost;
        packed.push(result);
    }

    Packed {
        results: packed,
        tokens: used + header_tokens,
        truncated,
    }
}

fn trim_relations<F>(relations: &mut Relations, budget: usize, count: &F)
where
    F: Fn(&str) -> usize,
{
    loop {
        let cost = count(&render_relations_plain(relations));
        if cost <= budget {
            return;
        }
        if !relations.callees.is_empty() {
            relations.callees.pop();
        } else if !relations.callers.is_empty() {
            relations.callers.pop();
        } else if !relations.types_used.is_empty() {
            relations.types_used.pop();
        } else if !relations.implements.is_empty() {
            relations.implements.pop();
        } else if !relations.implemented_by.is_empty() {
            relations.implemented_by.pop();
        } else {
            return;
        }
        relations.truncated = true;
    }
}

fn truncate_code<F>(result: &mut QueryResult, budget: usize, count: &F)
where
    F: Fn(&str) -> usize,
{
    let mut lines: Vec<&str> = result.code.lines().collect();
    while !lines.is_empty() && count(&lines.join("\n")) > budget {
        lines.pop();
    }
    let mut code = lines.join("\n");
    if !code.is_empty() {
        code.push_str("\n// ... (truncated; read file for full body)");
    } else {
        code = "// ... (truncated; read file for full body)".to_owned();
    }
    result.code = code;
}

/// Renders the human-readable header.
#[must_use]
pub fn render_header(query: &str, index: &IndexInfo, tokens: usize, max_tokens: usize) -> String {
    let mut out = String::new();
    out.push_str("# CIGAR context\n");
    out.push_str(&format!("query: \"{query}\"\n"));
    out.push_str(&format!(
        "index: branch={} head={} mode={} model={} stale={}\n",
        index.branch,
        index.head.as_deref().unwrap_or("(none)"),
        index.mode,
        index.model,
        if index.stale { "yes" } else { "no" }
    ));
    out.push_str(&format!("tokens: ~{tokens} / {max_tokens}\n"));
    if index.stale {
        out.push_str("warning: index is behind HEAD; run `cigar sync`\n");
    }
    out
}

/// Renders one result block.
#[must_use]
pub fn render_result(result: &QueryResult) -> String {
    let occurrence = &result.occurrence;
    let fence = fence_for(&result.code);
    let language = language_tag(occurrence.symbol.language);
    let mut out = String::new();
    out.push_str(&format!(
        "\n## {}. {}:{}-{} . {} . {} . score={:.3}\n",
        result.rank,
        occurrence.file_path,
        occurrence.start_line,
        occurrence.end_line,
        occurrence.symbol.qualified_name,
        occurrence.symbol.kind,
        result.score
    ));
    out.push_str("UNTRUSTED REPOSITORY CONTENT\n");
    if !result.imports.is_empty() {
        out.push_str("imports:\n");
        for line in result.imports.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str("signature:\n");
    out.push_str(&format!("  {}\n", occurrence.symbol.signature));
    if let Some(doc) = &occurrence.symbol.docstring {
        out.push_str("doc:\n");
        for line in doc.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str("code:\n");
    out.push_str(&format!("{fence}{language}\n"));
    out.push_str(&result.code);
    if !result.code.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{fence}\n"));
    if result.code_truncated {
        out.push_str("note: code truncated to fit budget\n");
    }
    out.push_str(&render_relations_plain(&result.relations));
    out
}

fn render_relations_plain(relations: &Relations) -> String {
    let mut out = String::new();
    for (label, list) in [
        ("callers", &relations.callers),
        ("callees", &relations.callees),
        ("types_used", &relations.types_used),
        ("implements", &relations.implements),
        ("implemented_by", &relations.implemented_by),
    ] {
        if list.is_empty() {
            continue;
        }
        out.push_str(&format!("{label} ({}):\n", list.len()));
        for view in list {
            out.push_str(&format!(
                "  - {}:{} . {} . {}\n",
                view.file, view.start_line, view.qualified_name, view.kind
            ));
        }
    }
    if relations.truncated {
        out.push_str("relations: truncated\n");
    }
    out
}

fn language_tag(language: Language) -> &'static str {
    match language {
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::Javascript => "javascript",
        Language::Go => "go",
        Language::Python => "python",
    }
}

/// Picks a fence longer than any backtick run inside the code.
#[must_use]
pub fn fence_for(code: &str) -> String {
    let mut longest = 0_usize;
    let mut current = 0_usize;
    for ch in code.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// Renders the full text response.
#[must_use]
pub fn render_text(response: &QueryResponse, max_tokens: usize) -> String {
    let mut out = render_header(
        &response.query,
        &response.index,
        response.stats.tokens_estimated,
        max_tokens,
    );
    for result in &response.results {
        out.push_str(&render_result(result));
    }
    if response.results.is_empty() {
        out.push_str("\n(no results)\n");
    }
    if !response.warnings.is_empty() {
        out.push_str(&format!("\nwarnings: {}\n", response.warnings.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Digest, EmbeddingKey, SymbolDef};

    fn occurrence(code: &str, start: u32) -> SymbolOccurrence {
        SymbolOccurrence {
            symbol: SymbolDef {
                symbol_hash: crate::domain::SymbolHash::parse(format!("{:064x}", start)).unwrap(),
                language: Language::Rust,
                name: "authenticate".into(),
                qualified_name: "AuthMiddleware::authenticate".into(),
                kind: SymbolKind::Method,
                signature: "fn authenticate(req: Request) -> Result<Claims, AuthError>".into(),
                docstring: Some("Memeriksa bearer token JWT".into()),
                code_snippet: code.into(),
                body_hash: Digest::parse(format!("{:064x}", 1)).unwrap(),
            },
            branch: crate::domain::BranchName::parse("main").unwrap(),
            file_path: RepoPath::parse("src/auth.rs").unwrap(),
            start_line: start,
            end_line: start + 3,
            embedding_key: EmbeddingKey::parse(format!("{:064x}", 2)).unwrap(),
        }
    }

    #[test]
    fn query_validation_rejects_empty_and_long() {
        assert!(validate_query("   ").is_err());
        assert!(validate_query(&"x".repeat(2_001)).is_err());
        assert_eq!(validate_query("  jwt  ").unwrap(), "jwt");
    }

    #[test]
    fn fence_escapes_backticks() {
        assert_eq!(fence_for("let x = 1;"), "```");
        assert_eq!(fence_for("```rust\ncode\n```"), "````");
        assert_eq!(fence_for("a ` b"), "```");
    }

    #[test]
    fn header_includes_stale_warning() {
        let index = IndexInfo {
            namespace: "default".into(),
            branch: "main".into(),
            head: Some("abc1234".into()),
            mode: "commit".into(),
            status: "ready".into(),
            model: "granite".into(),
            stale: true,
        };
        let header = render_header("q", &index, 10, 100);
        assert!(header.contains("warning: index is behind HEAD"));
        assert!(header.contains("stale=yes"));
    }

    #[test]
    fn result_renders_header_signature_doc_code() {
        let result = QueryResult {
            rank: 1,
            score: 0.873,
            distance: 0.127,
            occurrence: occurrence("fn authenticate() {\n    check()\n}", 42),
            imports: "use axum::http::Request;".into(),
            code: "fn authenticate() {\n    check()\n}".into(),
            code_truncated: false,
            relations: Relations::default(),
        };
        let text = render_result(&result);
        assert!(text.contains("src/auth.rs:42-45"));
        assert!(text.contains("AuthMiddleware::authenticate"));
        assert!(text.contains("score=0.873"));
        assert!(text.contains("UNTRUSTED REPOSITORY CONTENT"));
        assert!(text.contains("```rust"));
        assert!(text.contains("Memeriksa bearer token JWT"));
    }

    #[test]
    fn relations_omit_empty_and_show_present() {
        let mut relations = Relations::default();
        relations.callers.push(RelatedView {
            symbol_hash: "a".into(),
            qualified_name: "login_handler".into(),
            kind: "function".into(),
            signature: "fn login_handler()".into(),
            file: "src/routes/login.rs".into(),
            start_line: 15,
            relation: "CALLS".into(),
        });
        let text = render_relations_plain(&relations);
        assert!(text.contains("callers (1):"));
        assert!(text.contains("src/routes/login.rs:15"));
        assert!(!text.contains("callees"));
    }
}
