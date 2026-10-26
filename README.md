# CIGAR

**C**ode-**I**ndexed **G**raph **A**ugmented **R**etrieval. A local, branch-aware
semantic index of a repository, serving compact and relevant code context to
coding agents over a CLI and an MCP server.

CIGAR is deliberately small: one SQLite file, one local embedding model, no
server to run. Everything stays on the machine.

## Install

```bash
cargo build --release -p cigar-cli
```

The binary is `target/release/cigar` (`cigar.exe` on Windows).

For real embeddings you also need the ONNX Runtime shared library next to the
binary (or `ORT_DYLIB_PATH` pointing at it):

- Windows: `onnxruntime.dll` (v1.31.0 CPU, from the Microsoft ONNX Runtime release)
- Linux: `libonnxruntime.so`
- macOS: `libonnxruntime.dylib`

The fake embedding backend (`--embedding fake`) needs neither the model nor the
runtime and is intended for tests and offline experiments.

## Quick start

```bash
cigar init                 # create .cig/, config, schema, model manifest
cigar model download       # fetch + checksum-verify the embedding model
cigar sync                 # index HEAD of the current branch
cigar query "jwt expiry validation"
cigar inspect AuthMiddleware::authenticate
cigar status --verify
```

Switch branches freely: `sync` and `query` follow the Git HEAD automatically.
Each branch keeps its own manifest, while files and symbols shared by branches
are stored once.

## How it works

1. **Delta.** Commit mode diffs the last indexed commit against HEAD with `gix`.
   Dirty mode (`cigar sync --dirty`) hashes the working tree instead, ignoring
   mtimes, and never advances the stored commit.
2. **Parse.** tree-sitter extracts functions, methods, types, imports, and edges
   for Rust, TypeScript/TSX, JavaScript, Go, and Python.
3. **Identify.** Content is hashed with domain-separated BLAKE3:
   - `blob_hash` over normalized file bytes (BOM stripped, CRLF → LF),
   - `symbol_hash` over language, kind, qualified name, signature, docstring,
     normalized body, and a content version,
   - `embedding_key` over the exact embedding template text and template version.
4. **Embed.** Templates (path, imports, signature, docstring, code) are embedded
   by a local ONNX model. Vectors are keyed by `embedding_key`, so unchanged
   symbols are never re-embedded — across commits *and* across branches.
5. **Query.** Cosine KNN over the vector pool, joined to the active branch
   manifest, then one-hop graph expansion. Results are packed into a token
   budget with a stable text format and a versioned JSON shape.

## Commands

| Command | Purpose |
|---|---|
| `cigar init` | Create `.cig`, config, schema, model metadata |
| `cigar sync` | Incremental index of the current branch |
| `cigar sync --dirty` | Index the working tree without advancing the commit |
| `cigar sync --all-branches` | Index every local branch |
| `cigar query <text>` | Retrieve ranked context |
| `cigar inspect <symbol>` | Symbol detail plus relations |
| `cigar status [--verify]` | Index state, counts, integrity check |
| `cigar reindex` | Backup and rebuild from scratch |
| `cigar branch list` / `prune` | Inspect or prune indexed branches |
| `cigar gc [--dry-run] [--prune-branches] [--vacuum]` | Drop unreferenced data |
| `cigar hook install` / `uninstall` | Manage post-commit/merge/checkout hooks |
| `cigar model download` / `status` | Manage the embedding model |
| `cigar repair [--verify] [--head] [--drop-orphans] [--rebuild-vectors]` | Verify and repair |
| `cigar mcp [--allow-write]` | MCP server over stdio |
| `cigar version` | Build and schema information |

Global flags: `--cwd`, `--cig-dir`, `--config`, `--namespace`, `--branch`,
`--json`, `--quiet`, `--verbose`, `--offline`, `--strict`, `--wait`.

Exit codes: `0` success, `1` general, `2` usage, `3` not initialized,
`4` lock busy, `5` model unavailable, `6` schema/config mismatch, `130` cancelled.

## MCP server

```bash
cigar mcp                 # read-only tools
cigar mcp --allow-write   # also exposes cigar_sync
```

Tools: `cigar_query`, `cigar_inspect`, `cigar_status`, and (opt-in)
`cigar_sync`. Read-only tools carry `readOnlyHint: true`. `cigar_sync` is absent
from `tools/list` unless `--allow-write` was given at startup.

Example client config:

```json
{
  "mcpServers": {
    "cigar": {
      "command": "/path/to/cigar",
      "args": ["mcp", "--cwd", "/path/to/repo"]
    }
  }
}
```

## Configuration

`.cig/config.toml` is written by `cigar init`. Precedence is
CLI flag > `CIGAR_*` environment > config file > built-in default.

Key settings:

```toml
[embedding]
provider = "onnx-local"                          # or "fake"
model = "granite-embedding-125m-english"
dimensions = 768
max_tokens = 512
pooling = "cls"                                  # Granite uses CLS, not mean

[query]
top_k = 5
depth = 1
max_tokens = 6000
max_neighbors = 8

[git]
max_file_bytes = 1048576
exclude = ["target/**", "node_modules/**"]
```

Changing `model`, `dimensions`, `template_version`, or normalization changes the
config fingerprint. A mismatch is refused with exit code `6` and an instruction
to run `cigar reindex`; it is never silently ignored.

### Models

| Model | Dims | Context | Pooling | License |
|---|---|---|---|---|
| `granite-embedding-125m-english` | 768 | 512 | CLS | Apache-2.0 |
| `granite-embedding-97m-multilingual-r2` | 384 | 512 (32k capable) | CLS | Apache-2.0 |

The 97M multilingual model is **384-dimensional**; the 125M English model is
768-dimensional. Both use CLS pooling, which is why pooling is data in the model
manifest rather than hardcoded. Pick one deliberately and reindex when switching.

## Storage

Everything lives in `.cig/`:

```text
.cig/
  config.toml
  store.db        # SQLite + sqlite-vec
  HEAD            # mirror of the current branch
  refs/heads/…    # mirror of indexed commits
  lock            # writer lock
```

`store.db` uses the content-addressed v2 schema: immutable blobs and symbol
definitions are shared, while branch manifests and occurrences are per branch.
`.cig/` is added to `.git/info/exclude` by `cigar init`, so it never appears in
`git status`.

## Guarantees

- **Incremental equals full.** A sync after changes produces the same manifest,
  symbol count, and edge count as `sync --full` on a fresh store.
- **No ghost results.** Query results always join the active branch manifest; a
  symbol deleted on this branch cannot appear, even if another branch uses it.
- **No stale work.** `sync --dirty` never advances the stored commit, so a later
  commit sync still diffs from the correct base.
- **No silent model drift.** Vector dimension and model identity are recorded and
  checked; mismatch is an error, not a guess.
- **No partial writes.** Content and branch batches are single SQLite
  transactions; a crash leaves a recoverable state, and the next sync recomputes.

## Testing

```bash
cargo test --workspace          # offline, uses the fake embedder
cargo clippy --all-targets --workspace
cargo fmt --all -- --check
```

Real-model tests are ignored by default:

```bash
ORT_DYLIB_PATH=/path/to/onnxruntime.dll \
CIGAR_TEST_MODEL_DIR=$HOME/.cigar/models \
CIGAR_MODEL_DIR=$HOME/.cigar/models \
  cargo test --workspace -- --ignored --nocapture
```

These verify that the ONNX path produces correct, semantically meaningful
vectors and that a query for "jwt expiry validation" ranks JWT code above UI
code.

## License

Engine code: MIT OR Apache-2.0. Models are Apache-2.0 and are **not** bundled;
`cigar model download` fetches them from their published revision.
