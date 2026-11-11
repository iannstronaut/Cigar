# CIGAR

CIGAR is a local code-indexing engine and CLI for supplying compact repository
context to developers and coding agents. It indexes code structure and serves
queries through a command line and an MCP server over stdio.

## Design target and implementation status

The authoritative design is [docs.md](docs.md), version `0.2.0-engine`;
[docs/](docs/README.md) expands on that design. These documents describe the
**target**, not necessarily the behavior of the current code. The workspace is
currently version `0.1.0` and uses lexical search by default.

| Area | Target design | Current implementation |
|---|---|---|
| Default retrieval | SQLite FTS5 weighted BM25, identifier splitting, graph centrality, and one-hop context | FTS5 lexical ranking with graph centrality and one-hop context |
| Models | No model or heavy AI runtime for default init, sync, or query | Default CLI does not load a model; ONNX dependencies are behind the `semantic` feature |
| Storage | SQLite files, symbols, graph edges, and FTS5 index | SQLite schema v4 with files, symbols, graph edges, and FTS5; no sqlite-vec dependency |
| Repository state | Git object and working-tree indexing, including bare and filesystem-only modes | One snapshot per namespace; sync after switching branches, with dirty and filesystem modes supported |
| Performance | The specification proposes fast local retrieval | No latency or build-size target is claimed as measured here |

The target is daemonless and library-first: `cigar-core` provides engine
contracts, storage supplies SQLite, and the CLI is a thin interface. A calling
agent is expected to translate intent into technical search terms. Optional
semantic extensions are not intended to be prerequisites for lexical search.
Identifier splitting and BM25 field weights are configurable under `[search]`.
Changing `split_identifiers` requires reindexing: the next sync rebuilds the
namespace, and queries reject a mismatched setting until sync or reindex finishes.
Weights affect queries immediately without rebuilding the index.

## Build and current quick start

Build the current workspace with Rust 1.90 or newer:

```bash
cargo build --release -p cigar-cli
```

The binary is `target/release/cigar` (`cigar.exe` on Windows). The default
indexing and query paths require no model download or ONNX Runtime.

```bash
cigar init                 # create .cig/, configuration and index metadata
cigar sync                 # index the current branch
cigar query "jwt expiry validation"
cigar inspect AuthMiddleware::authenticate
cigar status --verify
```

The optional `semantic` build feature enables model-management dependencies;
the current CLI query and sync paths remain lexical.

## Current CLI

| Command | Purpose |
|---|---|
| `cigar init` | Create local configuration and SQLite schema |
| `cigar sync` | Incrementally index a branch; `--dirty` indexes the working tree without advancing the indexed commit |
| `cigar query <text>` | Retrieve ranked code context |
| `cigar inspect <symbol>` | Inspect a symbol and its relations |
| `cigar status [--verify]` | Show index state and optionally verify it |
| `cigar reindex` | Rebuild the index |
| `cigar hook install` / `uninstall` | Manage Git hooks |
| `cigar model download` / `status` | Manage embedding model files |
| `cigar repair --verify` | Verify store consistency |
| `cigar mcp [--allow-write]` | Serve MCP over stdio |
| `cigar version` | Show version and build information |

Use `cigar --help` and `cigar <command> --help` for current flags. Global
options include `--cwd`, `--cig-dir`, `--config`, `--namespace`, `--branch`,
`--json`, `--quiet`, `--verbose`, `--offline`, `--strict`, and `--wait`.

## MCP

```bash
cigar mcp                 # read-only tools
cigar mcp --allow-write   # also exposes cigar_sync
```

The current server provides `cigar_query`, `cigar_inspect`, and `cigar_status`;
`cigar_sync` is exposed only with `--allow-write`. For example:

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

## Current configuration and storage

`cigar init` writes `.cig/config.toml`. Configuration precedence is CLI flags,
then `CIGAR_*` environment variables, then the configuration file, then built-in
defaults. The default query and sync paths do not instantiate an embedding model.

The local state includes `.cig/store.db`, `.cig/HEAD` and a writer lock. Each
namespace holds one snapshot, not independent branch manifests. After switching
branches, run `cigar sync` to update the snapshot. Dirty sync invalidates the
commit baseline so the next commit sync reconciles the complete tree.

Schema versions 2 and 3 cannot be opened as version 4. Preserve the old index by
moving the entire `.cig` directory aside, then run `cigar init` and `cigar sync`
to create a fresh index.

## Tests

Run `cargo test --workspace`. Real-model tests require the `semantic` feature,
a separately installed model and ONNX Runtime. See [the testing design](docs/15-testing-dan-verifikasi.md)
for additional target verification criteria.

## License

Engine code is licensed under the [Mozilla Public License 2.0](LICENSE).
The embedding models are not bundled; consult their published licenses and
revisions before downloading them.
