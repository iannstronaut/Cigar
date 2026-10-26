//! Argument parsing. Clap lives only in this crate.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// CIGAR command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "cigar",
    version,
    about = "Code-Indexed Graph Augmented Retrieval",
    long_about = "CIGAR builds a local, branch-aware semantic index of a repository and \
                  serves compact, relevant context to coding agents.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Flags accepted by every subcommand.
#[derive(Debug, Args, Clone)]
pub struct GlobalArgs {
    /// Root to search for the repository.
    #[arg(long, global = true, env = "CIGAR_CWD", value_name = "PATH")]
    pub cwd: Option<PathBuf>,

    /// Override the state directory.
    #[arg(long, global = true, env = "CIGAR_DIR", value_name = "PATH")]
    pub cig_dir: Option<PathBuf>,

    /// Override the configuration file.
    #[arg(long, global = true, env = "CIGAR_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Namespace for isolated index data.
    #[arg(long, global = true, env = "CIGAR_NAMESPACE", value_name = "NAME")]
    pub namespace: Option<String>,

    /// Branch to operate on (defaults to HEAD).
    #[arg(long, global = true, env = "CIGAR_BRANCH", value_name = "NAME")]
    pub branch: Option<String>,

    /// Emit JSON instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Only emit errors.
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Emit detailed diagnostics to stderr.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Disable colored output.
    #[arg(long, global = true, env = "NO_COLOR", hide_env_values = true)]
    pub no_color: bool,

    /// Disable progress rendering.
    #[arg(long, global = true)]
    pub no_progress: bool,

    /// Seconds to wait for the store lock.
    #[arg(long, global = true, default_value_t = 0, value_name = "SECS")]
    pub wait: u64,

    /// Forbid network access.
    #[arg(long, global = true, env = "CIGAR_OFFLINE")]
    pub offline: bool,

    /// Treat parse warnings as errors.
    #[arg(long, global = true)]
    pub strict: bool,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize the state directory, schema, and model.
    Init(InitArgs),
    /// Incrementally index the repository.
    Sync(SyncArgs),
    /// Show index state and statistics.
    Status(StatusArgs),
    /// Search for relevant code context.
    Query(QueryArgs),
    /// Show a symbol's details and relationships.
    Inspect(InspectArgs),
    /// Rebuild the index from scratch.
    Reindex(ReindexArgs),
    /// Inspect or prune indexed branches.
    Branch(BranchArgs),
    /// Remove unreferenced shared data.
    Gc(GcArgs),
    /// Manage Git hooks.
    Hook(HookArgs),
    /// Run the MCP server over stdio.
    Mcp(McpArgs),
    /// Manage the embedding model.
    Model(ModelArgs),
    /// Verify and repair store inconsistencies.
    Repair(RepairArgs),
    /// Print version and build information.
    Version(VersionArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Source mode: auto, worktree, bare, or fs-only.
    #[arg(long, default_value = "auto", value_name = "MODE")]
    pub git_mode: String,

    /// Embedding provider.
    #[arg(long, default_value = "onnx-local", value_name = "PROVIDER")]
    pub embedding: String,

    /// Model identifier.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,

    /// Embedding dimensions.
    #[arg(long, value_name = "N")]
    pub dimensions: Option<usize>,

    /// Model cache directory.
    #[arg(long, value_name = "PATH")]
    pub model_dir: Option<PathBuf>,

    /// Skip model download and verification.
    #[arg(long)]
    pub no_model: bool,

    /// Revalidate and repair configuration without deleting data.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Index the working tree without advancing HEAD.
    #[arg(long)]
    pub dirty: bool,
    /// Read every file, reusing vectors by key.
    #[arg(long)]
    pub full: bool,
    /// Skip untracked files in dirty mode.
    #[arg(long)]
    pub no_untracked: bool,
    /// Limit parse parallelism.
    #[arg(long, value_name = "N")]
    pub jobs: Option<usize>,
    /// Sync every branch found in Git refs.
    #[arg(long)]
    pub all_branches: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Run integrity verification.
    #[arg(long)]
    pub verify: bool,
    /// Show every indexed branch.
    #[arg(long)]
    pub all_branches: bool,
}

#[derive(Debug, Args)]
pub struct QueryArgs {
    /// Natural-language question.
    #[arg(value_name = "QUERY")]
    pub query: String,
    /// Number of primary results.
    #[arg(long, value_name = "N")]
    pub top_k: Option<usize>,
    /// Graph expansion depth.
    #[arg(long, value_name = "N")]
    pub depth: Option<u8>,
    /// Approximate token budget.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<usize>,
    /// Neighbors per relation direction.
    #[arg(long, value_name = "N")]
    pub max_neighbors: Option<usize>,
    /// Restrict to matching paths (repeatable).
    #[arg(long, value_name = "GLOB")]
    pub path: Vec<String>,
    /// Restrict to symbol kinds.
    #[arg(long, value_name = "KINDS")]
    pub kind: Option<String>,
    /// Minimum cosine similarity.
    #[arg(long, value_name = "F")]
    pub min_score: Option<f32>,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Qualified name, short name, or symbol hash.
    #[arg(value_name = "SYMBOL")]
    pub symbol: String,
    /// Restrict resolution to one file.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Graph depth (0..=3).
    #[arg(long, value_name = "N")]
    pub depth: Option<u8>,
}

#[derive(Debug, Args)]
pub struct ReindexArgs {
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
    /// Skip the pre-rebuild backup.
    #[arg(long)]
    pub no_backup: bool,
    /// Reindex the working tree instead of HEAD.
    #[arg(long)]
    pub dirty: bool,
}

#[derive(Debug, Args)]
pub struct BranchArgs {
    #[command(subcommand)]
    pub command: BranchCommand,
}

#[derive(Debug, Subcommand)]
pub enum BranchCommand {
    /// List indexed branches.
    List,
    /// Prune branches whose Git ref no longer exists.
    Prune {
        /// Report what would be removed.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
pub struct GcArgs {
    /// Report only.
    #[arg(long)]
    pub dry_run: bool,
    /// Prune branches whose refs disappeared.
    #[arg(long)]
    pub prune_branches: bool,
    /// Vacuum the database afterwards.
    #[arg(long)]
    pub vacuum: bool,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    #[command(subcommand)]
    pub command: HookCommand,
}

#[derive(Debug, Subcommand)]
pub enum HookCommand {
    /// Install post-commit, post-merge, and post-checkout hooks.
    Install {
        /// Binary path to invoke.
        #[arg(long, value_name = "PATH")]
        bin: Option<PathBuf>,
        /// Print planned changes without writing.
        #[arg(long)]
        dry_run: bool,
        /// Append hook output to this log file.
        #[arg(long, value_name = "PATH")]
        log: Option<PathBuf>,
    },
    /// Remove CIGAR-managed hook blocks.
    Uninstall,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    /// Enable the `cigar_sync` tool.
    #[arg(long)]
    pub allow_write: bool,
    /// Write JSON-RPC logs here instead of stderr.
    #[arg(long, value_name = "PATH")]
    pub log: Option<PathBuf>,
    /// Default query token budget.
    #[arg(long, value_name = "N")]
    pub max_tokens: Option<usize>,
}

#[derive(Debug, Args)]
pub struct ModelArgs {
    #[command(subcommand)]
    pub command: ModelCommand,
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Download and verify the configured model.
    Download {
        /// Model identifier override.
        #[arg(long, value_name = "ID")]
        model: Option<String>,
        /// Revision override.
        #[arg(long, value_name = "REV")]
        revision: Option<String>,
        /// Model cache directory.
        #[arg(long, value_name = "PATH")]
        model_dir: Option<PathBuf>,
        /// Re-download even when present.
        #[arg(long)]
        force: bool,
    },
    /// Report model presence, checksum, and dimensions.
    Status,
}

#[derive(Debug, Args)]
pub struct RepairArgs {
    /// Report problems without changing anything.
    #[arg(long)]
    pub verify: bool,
    /// Delete vectors with no occurrence.
    #[arg(long)]
    pub drop_orphans: bool,
    /// Re-embed symbols that lack a vector.
    #[arg(long)]
    pub rebuild_vectors: bool,
    /// Rewrite `.cig/HEAD` and refs from the database.
    #[arg(long)]
    pub head: bool,
    /// Skip the confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct VersionArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Output style requested by flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

impl Cli {
    /// Parses process arguments.
    #[must_use]
    pub fn parse_args() -> Self {
        <Self as Parser>::parse()
    }
}
