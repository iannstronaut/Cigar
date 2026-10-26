# CIGAR — Rencana Implementasi (Fase 0–6)

Sumber kebenaran: `docs.md` (ruang lingkup) + `docs/` (detail teknis). Bila konflik: `docs.md` menang untuk ruang lingkup; `docs/` menang untuk detail implementasi; penyimpangan dicatat sebagai ADR di `docs/18-keputusan-arsitektur.md` (tidak diubah pada implementasi ini).

Status skema: **v2 multi-branch content-addressable** (`docs/21`), `schema_version = 2`.

---

## 0. Aturan kerja yang mengikat

1. Empat crate: `cigar-core`, `cigar-embed`, `cigar-storage`, `cigar-cli`. Arah dependensi hanya `cli → {embed, storage} → core`.
2. Core tidak boleh meng-import `rusqlite`, `ort`, `clap`. Tidak ada async runtime.
3. Semua error publik `thiserror`; tidak ada `anyhow` di library. Tidak ada `unwrap` pada input eksternal.
4. ID/newtype wajib: `Namespace`, `RepoPath`, `BranchName`, `BlobHash`, `SymbolHash`, `EmbeddingKey`, `OccurrenceRef`.
5. Hash domain-separated, byte `0x00` sebagai pemisah (`docs/21 §2`):
   - `blob_hash = BLAKE3(normalized_file_bytes)` (LF, tanpa BOM)
   - `symbol_hash = BLAKE3("symbol\0" || language || "\0" || kind || "\0" || qualified_name || "\0" || signature || "\0" || docstring_or_empty || "\0" || normalized_body || "\0" || content_version)`
   - `embedding_key = BLAKE3("embed\0" || template_version || "\0" || exact_template_text)`
6. Vector key = `embedding_key`, **bukan** `symbol_hash`. Vektor dibersihkan eksplisit (vec0 tidak punya FK).
7. `sync --dirty` tidak memajukan `commit_hash`/refs. `sync` commit memajukan `branch_refs` lalu mirror `.cig/refs/heads/<branch>`.
8. MCP read-only default; `cigar_sync` hanya ada dengan `--allow-write`.
9. Test offline default (fake embedder). Test model nyata `#[ignore]`.
10. Setiap fase selesai: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, smoke CLI; lalu commit **tanpa** `docs.md`, `docs/`, `.agent/`.

---

## 1. Fase 0 — M0 Spike dan keputusan (gate)

Deliverable:

- [x] Baca seluruh `docs/` + `docs.md`; kunci ruang lingkup v0.1 dan skema v2.
- [ ] Spike `gix`: worktree/bare/linked, HEAD+tree, tree diff tanpa checkout, baca blob, missing object, submodule/symlink mode.
- [ ] Spike `sqlite-vec`: registrasi statis, TEXT PK, partition, KNN (`k=` vs `LIMIT`), delete/upsert, rollback, join, WAL.
- [ ] Spike `tree-sitter`: versi grammar kompatibel 0.27.x untuk Rust/TS/TSX/Go/Python, node type aktual, query compile.
- [ ] Spike `ort` + `tokenizers`: model Granite 97M, 768-d, pooling, normalisasi, ukuran/unduh, cold start.
- [ ] Catat hasil di `plans/spike-report.md`; keputusan fallback bila ada yang tidak didukung.

Gate: bila `sqlite-vec` atau model tidak layak tanpa workaround, hentikan dan revisi arsitektur (ADR baru). Jika layak, lanjut Fase 1.

---

## 2. Fase 1 — M1 Skeleton, trait, storage (`docs/17 §M1`)

### Deliverable

1. Workspace Cargo 4 crate + `rust-toolchain.toml` + shared lints + profile rilis.
2. `cigar-core`:
   - `domain`: newtype ID + validasi (`Namespace`, `RepoPath`, `BranchName`, `BlobHash`, `SymbolHash`, `EmbeddingKey`, `OccurrenceRef`), `SymbolKind`, `EdgeRelation`, `Language`, `SyncMode`, `SyncStatus`, `BranchState`, `BranchFile`, `SymbolDef`, `BlobSymbol`, `BranchSymbol`, `BranchEdge`, `ScoredOccurrence`, `GraphNeighborhood`, `StoreStats`, `GcReport`.
   - `error`: `EngineError` lengkap + pemetaan exit code (0,1,2,3,4,5,6,130).
   - `traits`: `Embedder`, `ContextStore` (branch-aware v2), `GitSource`/`SourceProvider`, `LanguageParser`, `ProgressSink`, `Clock`.
   - `config`: parse/validasi `config.toml`, precedence CLI > env > file > default, fingerprint.
   - `hash`: BLAKE3 domain-separated, normalisasi BOM/CRLF, `body_hash`, `symbol_hash`, `embedding_key`, `config_fingerprint`.
   - `fake` embedder deterministik? (di `cigar-embed`, bukan core — core hanya trait.)
3. `cigar-storage`:
   - Registrasi `sqlite-vec` sekali per proses (`Once`, `// SAFETY:`).
   - PRAGMA wajib per koneksi; writer tunggal `Mutex<Connection>`; reader read-only.
   - Schema v2 DDL persis `docs/21 §3` + index; `meta` (`schema_version=2`, `content_version`, `engine_version`, model fields).
   - `apply_content_batch` + `apply_branch_batch` atomik; delete eksplisit vektor; `get_branch_state`, `set_branch_state`, `get_branch_manifest`, `stats`, `health_check`, `schema_version`.
   - Lock advisory `.cig/lock` (file lock + metadata pid), stale detection.
   - Backup helper (`VACUUM INTO`) + migrasi v1→v2 = reindex (tidak ada migrasi baris).
4. `cigar-embed`: `FakeEmbedder` deterministik (BLAKE3 seed → LCG → L2 normalize), `dimensions`, `max_tokens`.
5. `cigar-cli`: `init`, `status`, `version`; global flags (`--cwd`, `--cig-dir`, `--config`, `--namespace`, `--branch`, `--json`, `--quiet`, `--verbose`, `--no-color`, `--no-progress`, `--wait`, `--offline`, `--strict`).
   - `init`: `.cig/` 0700, config atomik, schema, `HEAD` (`ref: refs/heads/<branch>`), `refs/heads/<branch>` kosong, `.git/info/exclude` idempoten, `--no-model`/`--offline`, `--force` tidak menghapus DB.
   - `status`: branch state, counts, DB size, model info, stale/behind, `--verify` (v1: foreign_keys, orphan checks), `--json`.
6. Contract test storage subset + unit test hash/config/domain; e2e `init`/`status`/idempotensi.

### Acceptance (dari `docs/17`)

- `cigar init` idempoten; `.cig` layout v2; schema v2 dibuat; `vec_version()` bekerja; `foreign_keys=ON`.
- Insert/delete shared + branch + vector atomik; tidak ada orphan.
- `status` belum sync/0 file tanpa crash.
- `ConfigMismatch` dan `SchemaTooNew` teruji.
- Test offline lulus.

### Verifikasi

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -p cigar-cli -- init --no-model --cwd <tmp>
cargo run -p cigar-cli -- status --json --cwd <tmp>
```

---

## 3. Fase 2 — M2 Git, parser, sync (`docs/17 §M2`)

### Deliverable

1. `cigar-core/git`: `SourceProvider` gix (worktree), HEAD OID, tree diff base→head, blob read dengan limit ukuran, skip submodule/symlink/LFS/binary/non-UTF-8, SHA-256 commit id aman (40/64), shallow/partial → error actionable.
2. Parser Rust + TypeScript/TSX via tree-sitter: imports, symbols, edges (CALLS/USES_TYPE/IMPLEMENTS), signature/doc/code/line range, naming per `docs/07`.
3. Hash: `body_hash` dari AST ternormalisasi; `symbol_hash`; `embedding_key` dari template v1 (`path/imports/symbol/doc/---/code`).
4. Sync pipeline per branch: delta commit, content dedup (`file_blobs`, `symbol_defs`, `blob_symbols`), batch planning, parse paralel (`rayon`, parser per thread), fake embed, `apply_content_batch` + `apply_branch_batch`, edge resolution per branch + backfill, publish `branch_refs` + refs mirror.
5. Mode: `sync`, `--dirty`, `--full`, `--branch`, `--all-branches`; no-op path.
6. Recovery: `status=syncing` → recover; crash di tengah batch → sync berikutnya == full; `status --verify` dasar.
7. Fixture Git multi-branch; snapshot parser; property test `incremental == reindex` (fake).

### Acceptance (dari `docs/17` + `docs/21 §10`)

- Initial sync 1k file (fake) selesai; counts benar.
- `main` + `feat/auth` berbagi blob/simbol; query/occurrence per branch tidak ghost.
- No-op sync 0 embedding; modify 1 fungsi → 1 `embedding_key`; ubah imports → semua simbol file.
- Delete file di branch → occurrence hilang; shared object tetap.
- Backfill edge saat simbol baru muncul.
- Dirty tidak menulis HEAD/ref; commit sync memajukan `branch_refs`.
- Crash recovery == full, per branch.

### Verifikasi

```powershell
cargo test -p cigar-core git parse sync
cargo test -p cigar-storage
# e2e fixture: init --embedding fake; sync main; sync feat/auth; status --all-branches; branch list
```

---

## 4. Fase 3 — M3 Embedding nyata + Go/Python (`docs/17 §M3`)

### Deliverable

1. `cigar-embed`: model manager (layout `~/.cigar/models/<model>/<revision>`, manifest compile-time, HTTPS download, SHA-256, size, `READY`, lock, offline), tokenizer, ONNX session (CPU, intra-op, Mutex), pooling (mean/cls), L2 normalize, finite check, batch.
2. Template embedding versioned; `embedding_key` cache reuse.
3. Integrasi sync; lazy model load; no-op sync tidak memuat model.
4. `reindex` (validasi config sebelum hapus, backup `store.db.bak-*`, `--yes`, `--no-backup`, `--dirty`, `--strict`, `--wait`).
5. `model download` / `model status`.
6. Parser Go + Python (snapshot).
7. Benchmark embedding + evaluasi template awal (10 query, `#[ignore]`).

### Acceptance

- Checksum salah ditolak; offline tidak mengunduh; vektor 768 finite normalized; batch count cocok.
- Sync 1k file model nyata selesai; no-op tidak memuat model.
- Ganti model/dimensi → `ConfigMismatch`; `reindex` berhasil.
- Parser Go/Python snapshot lulus.

---

## 5. Fase 4 — M4 Query, graph, inspect (`docs/17 §M4`)

### Deliverable

1. KNN shared pool + over-fetch (`K = clamp(max(top_k*8,64),64,500)`) + widening + join `branch_symbols`/`branch_files` + dedupe `(path,symbol,start_line)`.
2. Ranking cosine murni, tie-break `path,start_line,symbol_hash`; score = 1-distance; buang non-finite.
3. Graph expansion 1-hop via `branch_edges` (callers/callees/types_used/implements/implemented_by), batas neighbor, signature-only untuk neighbor, `depth 2` signature+path.
4. Context packing: header `# CIGAR context`, label `UNTRUSTED REPOSITORY CONTENT`, fence aman, budget 70/20/10, truncation per baris, dedupe imports, `~tokens`.
5. `query` teks/JSON (`schema_version:2`), `--top-k/--depth/--max-tokens/--max-neighbors/--path/--kind/--min-score/--branch/--wait`.
6. `inspect` resolusi qualified→name→suffix, `--file`, `--depth 0..3`, ambiguitas = daftar kandidat + exit 1.

### Acceptance

- Top-5 relevan ≥80% dataset uji; JSON deterministik (abaikan timing).
- Budget 6k dihormati; top-1 selalu ada; fence aman triple backtick.
- `--branch` tidak bocor lintas branch; callers/callees benar lintas file per branch.
- Warm p95 < 1 dtk pada 100k simbol/20 branch (benchmark).

---

## 6. Fase 5 — M5 Hook, MCP, mode Git lanjutan (`docs/17 §M5`)

### Deliverable

1. `hook install/uninstall` (post-commit/post-merge/post-checkout) marker-managed, idempoten, `--bin`, `--dry-run`, `--log`, `core.hooksPath`, non-worktree gagal jelas.
2. MCP stdio: framing JSON-RPC (Content-Length), initialize/initialized/ping, tools/list, tools/call `cigar_query`/`cigar_inspect`/`cigar_status`, opt-in `cigar_sync`; `branch` param; structuredContent; tool error `isError` + kode stabil; stdout hanya JSON-RPC.
3. `branch list`, `branch prune`, `gc --dry-run/--prune-branches/--vacuum`, urutan delete RESTRICT-safe.
4. Bare repo, fs-only, detached HEAD (`@detached`).
5. Locking shared/exclusive di CLI/MCP; `--quiet`/`--json` konsisten; Windows paths.
6. E2E: MCP client script (initialize→tools/list→query→inspect), hook integration.

### Acceptance

- Hook tidak merusak hook lain; uninstall bersih; commit memicu sync.
- MCP stdout bersih; `cigar_sync` absen tanpa `--allow-write`.
- Bare/fs-only/detached sync+query.
- GC dry-run aman; prune tidak menghapus objek branch lain; lock contention exit 4.

---

## 7. Fase 6 — M6 Hardening dan rilis (`docs/17 §M6`)

### Deliverable

1. Audit keamanan checklist `docs/13`: path confinement, no symlink follow, no secrets in log, MCP input validation, hook quoting, SQL parameterized.
2. Full crash/failpoint, fuzz parser input, compatibility matrix (Windows/macOS/Linux), performance regression W1–W10 + multi-branch 20.
3. `status --verify` lengkap + `repair` (`--verify`, `--drop-orphans`, `--rebuild-vectors`, `--head`, `--yes`).
4. README pengguna, troubleshooting, contoh config, MCP setup, lisensi/NOTICE model.
5. CI: fmt, clippy, test, build 3 OS, `cargo deny`.
6. Release build + checksum + changelog.

### Acceptance (release gate `docs/15 §16`)

- Semua gate lulus; tidak ada open question kritis.
- `incremental == reindex` per branch; GC invariant lulus.
- Benchmark W1–W10 + 20 branch tercatat.
- Binary berjalan di mesin tanpa Rust; model license/NOTICE benar.

---

## 8. Pemetaan dokumen → kode

| Dokumen | Modul/crate |
|---|---|
| 02 arsitektur | layout workspace, dependency rule |
| 03/21 skema | `cigar-storage/schema.rs`, `store.rs` |
| 04 sync | `cigar-core/sync/*` |
| 05 query | `cigar-core/query/*` |
| 06 trait/error | `cigar-core/traits/*`, `error.rs` |
| 07 parser | `cigar-core/parse/*` |
| 08 embedding | `cigar-embed/*`, `cigar-core/hash/template.rs` |
| 09 storage | `cigar-storage/sqlite.rs`, `vec.rs` |
| 10 CLI | `cigar-cli/commands/*` |
| 11 Git | `cigar-core/git/*` |
| 12 MCP | `cigar-cli/mcp/*` |
| 13 security | seluruh boundary + test |
| 14 performa | benchmark `scripts/`, `benches/` |
| 15 testing | unit/contract/integration/e2e |
| 16 observability | `progress.rs`, logging stderr |
| 17 milestone | urutan fase ini |
| 18 ADR | keputusan tetap; perubahan baru butuh ADR |
| 19 risiko | daftar cek + fallback |
| 20 standar | fmt/clippy/layout/commit |
| 21 v2 | sumber skema + query/GC multi-branch |

## 9. Definition of done per fase

1. Semua acceptance fase lulus dengan bukti command output.
2. `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, `cargo test` hijau.
3. Smoke test CLI nyata (bukan hanya unit test).
4. Tidak ada file `docs.md`, `docs/`, `.agent/` di commit.
5. Commit message konvensional (`feat(m1): ...`), satu commit per fase (boleh beberapa commit kecil di dalam fase, tetap tanpa docs).
