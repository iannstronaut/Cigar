//! `ContextStore` implementation over SQLite.

use rusqlite::{params, Connection, OptionalExtension};

use cigar_core::domain::{
    BlobHash, BranchBatch, BranchEdgeRef, BranchFile, BranchName, BranchOutcome, BranchState,
    CommitId, ConfigFingerprint, ContentBatch, ContentOutcome, Digest, EdgeRelation, EmbeddingKey,
    GcOptions, GcReport, GraphNeighborhood, Language, Namespace, OccurrenceRef, RelatedOccurrence,
    RepoPath, ScoredOccurrence, StoreHealth, StoreStats, SymbolHash, SymbolOccurrence,
};
use cigar_core::error::{EngineError, Result};
use cigar_core::traits::{BlobSymbolRow, ContextStore};

use crate::batch;
use crate::sqlite::SqliteStore;

impl ContextStore for SqliteStore {
    fn init_schema(&self, dimensions: usize) -> Result<()> {
        crate::store::init_or_validate(self, dimensions)
    }

    fn schema_version(&self) -> Result<u32> {
        let conn = self.conn()?;
        crate::store::read_schema_version(&conn)
    }

    fn health_check(&self) -> Result<StoreHealth> {
        health_check(self)
    }

    fn get_branch_state(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
    ) -> Result<Option<BranchState>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT commit_hash, last_sync_mode, status, worktree_fingerprint,
                        config_fingerprint, updated_at
                 FROM branch_refs WHERE namespace = ?1 AND branch_name = ?2",
                params![namespace.as_str(), branch.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(storage)?;

        let Some((commit, mode, status, fingerprint, config, updated)) = row else {
            return Ok(None);
        };
        Ok(Some(BranchState {
            namespace: namespace.clone(),
            branch: branch.clone(),
            commit_hash: commit.map(CommitId::parse).transpose()?,
            last_sync_mode: mode.parse()?,
            status: status.parse()?,
            worktree_fingerprint: fingerprint.map(Digest::parse).transpose()?,
            config_fingerprint: ConfigFingerprint::parse(config)?,
            updated_at_ms: updated,
        }))
    }

    fn set_branch_state(&self, namespace: &Namespace, state: &BranchState) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO branch_refs(namespace, branch_name, commit_hash, last_sync_mode,
                                     status, worktree_fingerprint, config_fingerprint, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(namespace, branch_name) DO UPDATE SET
                commit_hash = excluded.commit_hash,
                last_sync_mode = excluded.last_sync_mode,
                status = excluded.status,
                worktree_fingerprint = excluded.worktree_fingerprint,
                config_fingerprint = excluded.config_fingerprint,
                updated_at = excluded.updated_at",
            params![
                namespace.as_str(),
                state.branch.as_str(),
                state.commit_hash.as_ref().map(|c| c.as_str().to_owned()),
                state.last_sync_mode.as_str(),
                state.status.as_str(),
                state
                    .worktree_fingerprint
                    .as_ref()
                    .map(|d| d.as_str().to_owned()),
                state.config_fingerprint.as_str(),
                state.updated_at_ms,
            ],
        )
        .map_err(storage)?;
        Ok(())
    }

    fn list_branches(&self, namespace: &Namespace) -> Result<Vec<BranchState>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT branch_name, commit_hash, last_sync_mode, status, worktree_fingerprint,
                        config_fingerprint, updated_at
                 FROM branch_refs WHERE namespace = ?1 ORDER BY branch_name",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![namespace.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(storage)?;

        let mut out = Vec::new();
        for row in rows {
            let (branch, commit, mode, status, fingerprint, config, updated) =
                row.map_err(storage)?;
            out.push(BranchState {
                namespace: namespace.clone(),
                branch: BranchName::parse(branch)?,
                commit_hash: commit.map(CommitId::parse).transpose()?,
                last_sync_mode: mode.parse()?,
                status: status.parse()?,
                worktree_fingerprint: fingerprint.map(Digest::parse).transpose()?,
                config_fingerprint: ConfigFingerprint::parse(config)?,
                updated_at_ms: updated,
            });
        }
        Ok(out)
    }

    fn get_branch_manifest(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
    ) -> Result<Vec<BranchFile>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT file_path, blob_hash, language FROM branch_files
                 WHERE namespace = ?1 AND branch_name = ?2 ORDER BY file_path",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![namespace.as_str(), branch.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(storage)?;
        let mut out = Vec::new();
        for row in rows {
            let (path, blob, language) = row.map_err(storage)?;
            out.push(BranchFile {
                file_path: RepoPath::parse(path)?,
                blob_hash: BlobHash::parse(blob)?,
                language: language.parse()?,
            });
        }
        Ok(out)
    }

    fn apply_content_batch(
        &self,
        namespace: &Namespace,
        batch: ContentBatch,
    ) -> Result<ContentOutcome> {
        batch::apply_content(self, namespace, &batch)
    }

    fn apply_branch_batch(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        batch: BranchBatch,
    ) -> Result<BranchOutcome> {
        batch::apply_branch(self, namespace, branch, &batch)
    }

    fn search_symbols(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        query_vec: &[f32],
        top_k: usize,
    ) -> Result<Vec<ScoredOccurrence>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        if query_vec.len() != self.dimensions() {
            return Err(EngineError::DimensionMismatch {
                expected: self.dimensions(),
                actual: query_vec.len(),
            });
        }
        let conn = self.conn()?;
        let limit = top_k.saturating_mul(8).clamp(64, 500);
        let blob = vector_blob(query_vec);
        let mut hits: Vec<(String, f32)> = Vec::new();
        {
            let mut statement = conn
                .prepare(
                    "SELECT embedding_key, distance FROM symbol_vectors
                     WHERE namespace = ?1 AND embedding MATCH ?2 AND k = ?3
                     ORDER BY distance",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map(params![namespace.as_str(), blob, limit as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
                })
                .map_err(storage)?;
            for row in rows {
                hits.push(row.map_err(storage)?);
            }
        }

        if hits.is_empty() {
            return Ok(Vec::new());
        }

        let mut results: Vec<ScoredOccurrence> = Vec::new();
        let mut seen: std::collections::BTreeSet<(String, String, u32)> =
            std::collections::BTreeSet::new();
        for (key, distance) in &hits {
            let key = EmbeddingKey::parse(key.clone())?;
            let occurrences = occurrences_for_key(&conn, namespace, branch, &key)?;
            for occurrence in occurrences {
                let dedupe = (
                    occurrence.file_path.to_string(),
                    occurrence.symbol_hash.to_string(),
                    occurrence.start_line,
                );
                if !seen.insert(dedupe) {
                    continue;
                }
                results.push(ScoredOccurrence {
                    embedding_key: key.clone(),
                    symbol_hash: occurrence.symbol_hash,
                    file_path: occurrence.file_path,
                    start_line: occurrence.start_line,
                    end_line: occurrence.end_line,
                    distance: *distance,
                    score: 1.0 - *distance,
                    pending_code: String::new(),
                });
            }
        }

        results.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.symbol_hash.cmp(&b.symbol_hash))
        });
        results.truncate(top_k);
        Ok(results)
    }

    fn get_neighbors(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        occurrence: &OccurrenceRef,
        depth: u8,
    ) -> Result<GraphNeighborhood> {
        if depth > 3 {
            return Err(EngineError::invalid("depth harus 0..=3"));
        }
        let conn = self.conn()?;
        let root = load_occurrence(&conn, namespace, branch, occurrence)?;
        let Some(root) = root else {
            return Err(EngineError::NotFound {
                entity: "simbol".into(),
                id: format!(
                    "{}:{}:{}",
                    occurrence.branch, occurrence.file_path, occurrence.start_line
                ),
            });
        };
        let mut neighborhood = GraphNeighborhood {
            root: Some(root.clone()),
            ..GraphNeighborhood::default()
        };
        if depth == 0 {
            return Ok(neighborhood);
        }

        let limit = 50_usize;
        neighborhood.callers = related(
            &conn,
            namespace,
            branch,
            "target_symbol_hash",
            &root.symbol.symbol_hash,
            EdgeRelation::Calls,
        )?;
        neighborhood.callees = related(
            &conn,
            namespace,
            branch,
            "source_symbol_hash",
            &root.symbol.symbol_hash,
            EdgeRelation::Calls,
        )?;
        neighborhood.types_used = related(
            &conn,
            namespace,
            branch,
            "source_symbol_hash",
            &root.symbol.symbol_hash,
            EdgeRelation::UsesType,
        )?;
        neighborhood.implements = related(
            &conn,
            namespace,
            branch,
            "source_symbol_hash",
            &root.symbol.symbol_hash,
            EdgeRelation::Implements,
        )?;
        neighborhood.implemented_by = related(
            &conn,
            namespace,
            branch,
            "target_symbol_hash",
            &root.symbol.symbol_hash,
            EdgeRelation::Implements,
        )?;

        for list in [
            &mut neighborhood.callers,
            &mut neighborhood.callees,
            &mut neighborhood.types_used,
            &mut neighborhood.implements,
            &mut neighborhood.implemented_by,
        ] {
            if list.len() > limit {
                list.truncate(limit);
                neighborhood.truncated = true;
            }
        }

        if depth >= 2 {
            // One extra hop over calls, deduplicated against existing entries.
            let mut extra = Vec::new();
            for related_occurrence in neighborhood.callers.iter().chain(&neighborhood.callees) {
                let nested = related(
                    &conn,
                    namespace,
                    branch,
                    "source_symbol_hash",
                    &related_occurrence.occurrence.symbol_hash,
                    EdgeRelation::Calls,
                )?;
                extra.extend(nested);
            }
            let mut seen: std::collections::BTreeSet<OccurrenceRef> =
                std::collections::BTreeSet::new();
            seen.insert(occurrence_ref_of(&root));
            seen.extend(
                neighborhood
                    .callers
                    .iter()
                    .chain(&neighborhood.callees)
                    .map(|item| item.occurrence.clone()),
            );
            for item in extra {
                if seen.insert(item.occurrence.clone()) {
                    neighborhood
                        .callees
                        .push(RelatedOccurrence { depth: 2, ..item });
                }
            }
            neighborhood.callees.sort_by(|a, b| {
                a.depth
                    .cmp(&b.depth)
                    .then_with(|| a.occurrence.file_path.cmp(&b.occurrence.file_path))
                    .then_with(|| a.occurrence.start_line.cmp(&b.occurrence.start_line))
            });
        }

        Ok(neighborhood)
    }

    fn get_occurrence(
        &self,
        namespace: &Namespace,
        occurrence: &OccurrenceRef,
    ) -> Result<Option<SymbolOccurrence>> {
        let conn = self.conn()?;
        load_occurrence(&conn, namespace, &occurrence.branch, occurrence)
    }

    fn find_symbols(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        name: &str,
        limit: usize,
    ) -> Result<Vec<SymbolOccurrence>> {
        let conn = self.conn()?;
        let limit = limit.clamp(1, 200);
        let mut out = Vec::new();
        let mut seen: std::collections::BTreeSet<(String, String, u32)> =
            std::collections::BTreeSet::new();

        for (sql, pattern) in [
            (
                "SELECT bs.file_path, bs.symbol_hash, bs.start_line, bs.end_line, bs.embedding_key
                 FROM branch_symbols bs
                 JOIN symbol_defs sd ON sd.symbol_hash = bs.symbol_hash
                 WHERE bs.namespace = ?1 AND bs.branch_name = ?2 AND sd.qualified_name = ?3
                 ORDER BY bs.file_path, bs.start_line LIMIT ?4",
                name.to_owned(),
            ),
            (
                "SELECT bs.file_path, bs.symbol_hash, bs.start_line, bs.end_line, bs.embedding_key
                 FROM branch_symbols bs
                 JOIN symbol_defs sd ON sd.symbol_hash = bs.symbol_hash
                 WHERE bs.namespace = ?1 AND bs.branch_name = ?2 AND sd.name = ?3
                 ORDER BY bs.file_path, bs.start_line LIMIT ?4",
                name.to_owned(),
            ),
            (
                "SELECT bs.file_path, bs.symbol_hash, bs.start_line, bs.end_line, bs.embedding_key
                 FROM branch_symbols bs
                 JOIN symbol_defs sd ON sd.symbol_hash = bs.symbol_hash
                 WHERE bs.namespace = ?1 AND bs.branch_name = ?2
                   AND (sd.qualified_name LIKE '%::' || ?3 OR sd.qualified_name LIKE '%.' || ?3)
                 ORDER BY bs.file_path, bs.start_line LIMIT ?4",
                name.to_owned(),
            ),
        ] {
            let mut statement = conn.prepare(sql).map_err(storage)?;
            let rows = statement
                .query_map(
                    params![namespace.as_str(), branch.as_str(), pattern, limit as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .map_err(storage)?;
            for row in rows {
                let (path, hash, start, _end, _key) = row.map_err(storage)?;
                if !seen.insert((path.clone(), hash.clone(), start as u32)) {
                    continue;
                }
                let reference = OccurrenceRef {
                    branch: branch.clone(),
                    file_path: RepoPath::parse(path)?,
                    symbol_hash: SymbolHash::parse(hash)?,
                    start_line: start as u32,
                };
                if let Some(occurrence) = load_occurrence(&conn, namespace, branch, &reference)? {
                    out.push(occurrence);
                }
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        Ok(out)
    }

    fn stats(&self, namespace: &Namespace, branch: &BranchName) -> Result<StoreStats> {
        let conn = self.conn()?;
        let files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_files WHERE namespace = ?1 AND branch_name = ?2",
                params![namespace.as_str(), branch.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let symbols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_symbols WHERE namespace = ?1 AND branch_name = ?2",
                params![namespace.as_str(), branch.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_edges WHERE namespace = ?1 AND branch_name = ?2",
                params![namespace.as_str(), branch.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let unresolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM branch_edges
                 WHERE namespace = ?1 AND branch_name = ?2 AND target_symbol_hash IS NULL",
                params![namespace.as_str(), branch.as_str()],
                |row| row.get(0),
            )
            .map_err(storage)?;
        let vectors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbol_vectors WHERE namespace = ?1",
                params![namespace.as_str()],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Read the state on the already-held connection: re-entering
        // `get_branch_state` here would deadlock on the writer mutex.
        let last_sync = {
            let row = conn
                .query_row(
                    "SELECT commit_hash, last_sync_mode, status, worktree_fingerprint,
                            config_fingerprint, updated_at
                     FROM branch_refs WHERE namespace = ?1 AND branch_name = ?2",
                    params![namespace.as_str(), branch.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(storage)?;
            match row {
                Some((commit, mode, status, fingerprint, config, updated)) => Some(BranchState {
                    namespace: namespace.clone(),
                    branch: branch.clone(),
                    commit_hash: commit.map(CommitId::parse).transpose()?,
                    last_sync_mode: mode.parse()?,
                    status: status.parse()?,
                    worktree_fingerprint: fingerprint.map(Digest::parse).transpose()?,
                    config_fingerprint: ConfigFingerprint::parse(config)?,
                    updated_at_ms: updated,
                }),
                None => None,
            }
        };
        Ok(StoreStats {
            files: files as u64,
            symbols: symbols as u64,
            edges: edges as u64,
            vectors: vectors as u64,
            unresolved_edges: unresolved as u64,
            db_size_bytes: self.on_disk_size(),
            last_sync,
        })
    }

    fn unresolved_edges_for_name(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        target_name: &str,
        limit: usize,
    ) -> Result<Vec<BranchEdgeRef>> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT file_path, source_symbol_hash, target_name, relation
                 FROM branch_edges
                 WHERE namespace = ?1 AND branch_name = ?2
                   AND target_symbol_hash IS NULL AND target_name = ?3
                 ORDER BY file_path, source_symbol_hash
                 LIMIT ?4",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(
                params![
                    namespace.as_str(),
                    branch.as_str(),
                    target_name,
                    limit.clamp(1, 1_000) as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(storage)?;
        let mut out = Vec::new();
        for row in rows {
            let (path, source, target, relation) = row.map_err(storage)?;
            out.push(BranchEdgeRef {
                branch: branch.clone(),
                file_path: RepoPath::parse(path)?,
                source_symbol_hash: SymbolHash::parse(source)?,
                target_name: target,
                relation: relation.parse()?,
            });
        }
        Ok(out)
    }

    fn resolve_edge(
        &self,
        namespace: &Namespace,
        branch: &BranchName,
        edge: &BranchEdgeRef,
        target: &SymbolHash,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE branch_edges SET target_symbol_hash = ?1
             WHERE namespace = ?2 AND branch_name = ?3 AND file_path = ?4
               AND source_symbol_hash = ?5 AND target_name = ?6 AND relation = ?7",
            params![
                target.as_str(),
                namespace.as_str(),
                branch.as_str(),
                edge.file_path.as_str(),
                edge.source_symbol_hash.as_str(),
                edge.target_name,
                edge.relation.as_str()
            ],
        )
        .map_err(storage)?;
        Ok(())
    }

    fn gc(&self, namespace: &Namespace, opts: GcOptions) -> Result<GcReport> {
        garbage_collect(self, namespace, opts)
    }

    fn ensure_vectors(
        &self,
        namespace: &Namespace,
        vectors: &[(EmbeddingKey, Vec<f32>)],
    ) -> Result<u64> {
        if vectors.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(storage)?;
        let mut inserted = 0;
        for (key, vector) in vectors {
            if vector.len() != self.dimensions() {
                return Err(EngineError::DimensionMismatch {
                    expected: self.dimensions(),
                    actual: vector.len(),
                });
            }
            let exists: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM symbol_vectors WHERE embedding_key = ?1 AND namespace = ?2",
                    params![key.as_str(), namespace.as_str()],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if exists > 0 {
                continue;
            }
            tx.execute(
                "INSERT INTO symbol_vectors(embedding_key, namespace, embedding)
                 VALUES (?1, ?2, ?3)",
                params![key.as_str(), namespace.as_str(), vector_blob(vector)],
            )
            .map_err(storage)?;
            inserted += 1;
        }
        tx.commit().map_err(storage)?;
        Ok(inserted)
    }

    fn existing_vector_keys(
        &self,
        namespace: &Namespace,
        keys: &[EmbeddingKey],
    ) -> Result<Vec<bool>> {
        let conn = self.conn()?;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM symbol_vectors WHERE embedding_key = ?1 AND namespace = ?2",
                    params![key.as_str(), namespace.as_str()],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            out.push(exists > 0);
        }
        Ok(out)
    }

    fn blob_symbols(&self, blob: &BlobHash, language: Language) -> Result<Vec<BlobSymbolRow>> {
        let conn = self.conn()?;
        blob_symbol_rows(&conn, blob, language)
    }
}

fn occurrence_ref_of(occurrence: &SymbolOccurrence) -> OccurrenceRef {
    OccurrenceRef {
        branch: occurrence.branch.clone(),
        file_path: occurrence.file_path.clone(),
        symbol_hash: occurrence.symbol.symbol_hash.clone(),
        start_line: occurrence.start_line,
    }
}

fn occurrences_for_key(
    conn: &Connection,
    namespace: &Namespace,
    branch: &BranchName,
    key: &EmbeddingKey,
) -> Result<Vec<ScoredOccurrence>> {
    let mut statement = conn
        .prepare(
            "SELECT bs.file_path, bs.symbol_hash, bs.start_line, bs.end_line
             FROM branch_symbols bs
             JOIN branch_files bf
               ON bf.namespace = bs.namespace
              AND bf.branch_name = bs.branch_name
              AND bf.file_path = bs.file_path
             WHERE bs.namespace = ?1 AND bs.branch_name = ?2 AND bs.embedding_key = ?3
             ORDER BY bs.file_path, bs.start_line",
        )
        .map_err(storage)?;
    let rows = statement
        .query_map(
            params![namespace.as_str(), branch.as_str(), key.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .map_err(storage)?;
    let mut out = Vec::new();
    for row in rows {
        let (path, hash, start, end) = row.map_err(storage)?;
        out.push(ScoredOccurrence {
            embedding_key: key.clone(),
            symbol_hash: SymbolHash::parse(hash)?,
            file_path: RepoPath::parse(path)?,
            start_line: start as u32,
            end_line: end as u32,
            distance: 0.0,
            score: 1.0,
            pending_code: String::new(),
        });
    }
    Ok(out)
}

fn load_occurrence(
    conn: &Connection,
    namespace: &Namespace,
    branch: &BranchName,
    occurrence: &OccurrenceRef,
) -> Result<Option<SymbolOccurrence>> {
    let row = conn
        .query_row(
            "SELECT bs.end_line, bs.embedding_key, sd.language, sd.name, sd.qualified_name,
                    sd.kind, sd.signature, sd.docstring, sd.code_snippet, sd.body_hash
             FROM branch_symbols bs
             JOIN symbol_defs sd ON sd.symbol_hash = bs.symbol_hash
             WHERE bs.namespace = ?1 AND bs.branch_name = ?2 AND bs.file_path = ?3
               AND bs.symbol_hash = ?4 AND bs.start_line = ?5",
            params![
                namespace.as_str(),
                branch.as_str(),
                occurrence.file_path.as_str(),
                occurrence.symbol_hash.as_str(),
                occurrence.start_line as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            },
        )
        .optional()
        .map_err(storage)?;

    let Some((end, key, language, name, qualified, kind, signature, doc, code, body)) = row else {
        return Ok(None);
    };
    Ok(Some(SymbolOccurrence {
        symbol: cigar_core::domain::SymbolDef {
            symbol_hash: occurrence.symbol_hash.clone(),
            language: language.parse()?,
            name,
            qualified_name: qualified,
            kind: kind.parse()?,
            signature,
            docstring: doc,
            code_snippet: code,
            body_hash: Digest::parse(body)?,
        },
        branch: branch.clone(),
        file_path: occurrence.file_path.clone(),
        start_line: occurrence.start_line,
        end_line: end as u32,
        embedding_key: EmbeddingKey::parse(key)?,
    }))
}

fn related(
    conn: &Connection,
    namespace: &Namespace,
    branch: &BranchName,
    match_column: &str,
    symbol_hash: &SymbolHash,
    relation: EdgeRelation,
) -> Result<Vec<RelatedOccurrence>> {
    let sql = if match_column == "target_symbol_hash" {
        "SELECT bs.file_path, bs.symbol_hash, bs.start_line
         FROM branch_edges be
         JOIN branch_symbols bs
           ON bs.namespace = be.namespace AND bs.branch_name = be.branch_name
          AND bs.file_path = be.file_path AND bs.symbol_hash = be.source_symbol_hash
         WHERE be.namespace = ?1 AND be.branch_name = ?2
           AND be.target_symbol_hash = ?3 AND be.relation = ?4
         GROUP BY bs.file_path, bs.symbol_hash, bs.start_line
         ORDER BY bs.file_path, bs.start_line"
    } else {
        "SELECT bs.file_path, bs.symbol_hash, bs.start_line
         FROM branch_edges be
         JOIN branch_symbols bs
           ON bs.namespace = be.namespace AND bs.branch_name = be.branch_name
          AND bs.file_path = be.file_path AND bs.symbol_hash = be.target_symbol_hash
         WHERE be.namespace = ?1 AND be.branch_name = ?2
           AND be.source_symbol_hash = ?3 AND be.relation = ?4
         GROUP BY bs.file_path, bs.symbol_hash, bs.start_line
         ORDER BY bs.file_path, bs.start_line"
    };
    let mut statement = conn.prepare(sql).map_err(storage)?;
    let rows = statement
        .query_map(
            params![
                namespace.as_str(),
                branch.as_str(),
                symbol_hash.as_str(),
                relation.as_str()
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(storage)?;
    let mut out = Vec::new();
    for row in rows {
        let (path, hash, start) = row.map_err(storage)?;
        out.push(RelatedOccurrence {
            occurrence: OccurrenceRef {
                branch: branch.clone(),
                file_path: RepoPath::parse(path)?,
                symbol_hash: SymbolHash::parse(hash)?,
                start_line: start as u32,
            },
            relation,
            depth: 1,
        });
    }
    Ok(out)
}

fn blob_symbol_rows(
    conn: &Connection,
    blob: &BlobHash,
    language: Language,
) -> Result<Vec<BlobSymbolRow>> {
    let imports: String = conn
        .query_row(
            "SELECT imports_text FROM file_blobs WHERE blob_hash = ?1 AND language = ?2",
            params![blob.as_str(), language.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?
        .unwrap_or_default();

    let mut statement = conn
        .prepare(
            "SELECT bs.symbol_hash, bs.start_line, bs.end_line,
                    sd.language, sd.name, sd.qualified_name, sd.kind, sd.signature,
                    sd.docstring, sd.code_snippet, sd.body_hash
             FROM blob_symbols bs
             JOIN symbol_defs sd ON sd.symbol_hash = bs.symbol_hash
             WHERE bs.blob_hash = ?1 AND bs.language = ?2
             ORDER BY bs.start_line, sd.qualified_name",
        )
        .map_err(storage)?;
    let rows = statement
        .query_map(params![blob.as_str(), language.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .map_err(storage)?;
    let mut out = Vec::new();
    for row in rows {
        let (hash, start, end, lang, name, qualified, kind, signature, doc, code, body) =
            row.map_err(storage)?;
        out.push(BlobSymbolRow {
            symbol_hash: SymbolHash::parse(hash)?,
            start_line: start as u32,
            end_line: end as u32,
            symbol: cigar_core::domain::SymbolDef {
                symbol_hash: SymbolHash::parse("0".repeat(64))?,
                language: lang.parse()?,
                name,
                qualified_name: qualified,
                kind: kind.parse()?,
                signature,
                docstring: doc,
                code_snippet: code,
                body_hash: Digest::parse(body)?,
            },
            imports: imports.clone(),
        });
        if let Some(last) = out.last_mut() {
            last.symbol.symbol_hash = last.symbol_hash.clone();
        }
    }
    Ok(out)
}

fn health_check(store: &SqliteStore) -> Result<StoreHealth> {
    let conn = store.conn()?;
    let mut health = StoreHealth::default();
    let mut fk = conn.prepare("PRAGMA foreign_key_check").map_err(storage)?;
    let count = fk.query_map([], |_| Ok(())).map_err(storage)?.count();
    health.foreign_key_violations = count as u64;

    let quick: String = conn
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .unwrap_or_else(|_| "error".into());
    health.quick_check_ok = quick.eq_ignore_ascii_case("ok");
    if !health.quick_check_ok {
        health.messages.push(format!("quick_check: {quick}"));
    }

    health.orphan_vectors = scalar(
        &conn,
        "SELECT COUNT(*) FROM symbol_vectors v
         WHERE NOT EXISTS (
            SELECT 1 FROM branch_symbols bs
            WHERE bs.embedding_key = v.embedding_key AND bs.namespace = v.namespace
         )",
    )?;
    health.symbols_without_vectors = scalar(
        &conn,
        "SELECT COUNT(*) FROM branch_symbols bs
         WHERE NOT EXISTS (
            SELECT 1 FROM symbol_vectors v
            WHERE v.embedding_key = bs.embedding_key AND v.namespace = bs.namespace
         )",
    )?;
    health.dangling_edges = scalar(
        &conn,
        "SELECT COUNT(*) FROM branch_edges be
         WHERE be.target_symbol_hash IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM symbol_defs sd WHERE sd.symbol_hash = be.target_symbol_hash)",
    )?;
    health.manifest_files_without_symbols = scalar(
        &conn,
        "SELECT COUNT(*) FROM branch_files bf
         WHERE NOT EXISTS (
            SELECT 1 FROM branch_symbols bs
            WHERE bs.namespace = bf.namespace AND bs.branch_name = bf.branch_name
              AND bs.file_path = bf.file_path
         )",
    )?;
    Ok(health.finish())
}

fn scalar(conn: &Connection, sql: &str) -> Result<u64> {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|value| value as u64)
        .map_err(storage)
}

fn garbage_collect(
    store: &SqliteStore,
    namespace: &Namespace,
    opts: GcOptions,
) -> Result<GcReport> {
    let mut conn = store.conn()?;
    let mut report = GcReport {
        bytes_before: store.on_disk_size(),
        ..GcReport::default()
    };
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(storage)?;

    if opts.prune_branches {
        let branches: Vec<String> = {
            let mut statement = tx
                .prepare("SELECT branch_name FROM branch_refs WHERE namespace = ?1")
                .map_err(storage)?;
            let rows = statement
                .query_map(params![namespace.as_str()], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for branch in branches {
            let exists: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM branch_refs WHERE namespace = ?1 AND branch_name = ?2",
                    params![namespace.as_str(), branch],
                    |row| row.get(0),
                )
                .map_err(storage)?;
            if exists == 0 {
                continue;
            }
            // A branch is pruned only when the caller explicitly asked; the CLI
            // decides which names are stale and passes them through `gc`.
            let _ = &branch;
        }
    }

    let orphan_keys: Vec<String> = {
        let mut statement = tx
            .prepare(
                "SELECT v.embedding_key FROM symbol_vectors v
                 WHERE v.namespace = ?1 AND NOT EXISTS (
                    SELECT 1 FROM branch_symbols bs
                    WHERE bs.embedding_key = v.embedding_key AND bs.namespace = v.namespace
                 )",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![namespace.as_str()], |row| row.get::<_, String>(0))
            .map_err(storage)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?
    };
    if !opts.dry_run {
        for key in &orphan_keys {
            tx.execute(
                "DELETE FROM symbol_vectors WHERE embedding_key = ?1 AND namespace = ?2",
                params![key, namespace.as_str()],
            )
            .map_err(storage)?;
        }
        report.vectors_deleted = orphan_keys.len() as u64;

        let defs_deleted = tx
            .execute(
                "DELETE FROM symbol_defs WHERE symbol_hash NOT IN (
                    SELECT symbol_hash FROM blob_symbols
                    UNION
                    SELECT symbol_hash FROM branch_symbols
                    UNION
                    SELECT source_symbol_hash FROM branch_edges
                    UNION
                    SELECT target_symbol_hash FROM branch_edges WHERE target_symbol_hash IS NOT NULL
                 ) AND symbol_hash NOT IN (SELECT source_symbol_hash FROM symbol_edges)",
                [],
            )
            .map_err(storage)?;
        report.symbol_defs_deleted = defs_deleted as u64;

        let edges_deleted = tx
            .execute(
                "DELETE FROM symbol_edges WHERE source_symbol_hash NOT IN (
                    SELECT symbol_hash FROM symbol_defs
                 )",
                [],
            )
            .map_err(storage)?;
        report.symbol_edges_deleted = edges_deleted as u64;

        let blobs_deleted = tx
            .execute(
                "DELETE FROM file_blobs WHERE (blob_hash, language) NOT IN (
                    SELECT blob_hash, language FROM branch_files
                 )",
                [],
            )
            .map_err(storage)?;
        report.file_blobs_deleted = blobs_deleted as u64;

        let blob_symbols_deleted = tx
            .execute(
                "DELETE FROM blob_symbols WHERE (blob_hash, language) NOT IN (
                    SELECT blob_hash, language FROM file_blobs
                 )",
                [],
            )
            .map_err(storage)?;
        report.blob_symbols_deleted = blob_symbols_deleted as u64;
    } else {
        report.vectors_deleted = orphan_keys.len() as u64;
    }

    if opts.dry_run {
        tx.rollback().map_err(storage)?;
    } else {
        tx.commit().map_err(storage)?;
        if opts.vacuum {
            conn.execute_batch("PRAGMA optimize; VACUUM;")
                .map_err(storage)?;
        } else {
            conn.execute_batch("PRAGMA optimize;").map_err(storage)?;
        }
    }
    report.bytes_after = store.on_disk_size();
    Ok(report)
}

fn storage(err: rusqlite::Error) -> EngineError {
    EngineError::storage(err.to_string())
}

/// Encodes an f32 vector as the little-endian blob sqlite-vec expects.
#[must_use]
pub fn vector_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}
