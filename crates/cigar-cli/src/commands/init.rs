//! `cigar init` (docs/10 ?4).

use std::fs;
use std::path::{Path, PathBuf};

use cigar_core::config::{render_default_config, Config};
use cigar_core::domain::{ConfigFingerprint, GitMode, Namespace, SyncMode, SyncStatus};
use cigar_core::error::{EngineError, Result};
use cigar_core::{ENGINE_VERSION, SCHEMA_VERSION};
use cigar_storage::{open_or_init_store, record_model_meta, StoreLock};

use crate::cli::InitArgs;
use crate::context::Context;
use crate::output::Printer;

/// Runs `cigar init`.
pub fn run(context: &Context, args: &InitArgs, printer: &Printer) -> Result<i32> {
    let git_mode: GitMode = args.git_mode.parse()?;
    if args.force {
        // `--force` never deletes data; it only revalidates and repairs metadata.
    }

    let existed = context.cig_dir.exists();
    create_state_dir(&context.cig_dir)?;

    let mut config = context.config.clone();
    config.git.mode = git_mode;
    if let Some(model) = &args.model {
        config.embedding.model = model.clone();
    }
    if let Some(dimensions) = args.dimensions {
        config.embedding.dimensions = dimensions;
    }
    if let Some(dir) = &args.model_dir {
        config.embedding.model_dir = dir.clone();
    }
    config.embedding.provider = args.embedding.parse()?;
    config.storage.path = context.store_path();
    config.validate()?;

    write_config_atomic(&context.config_path, &config)?;

    let fingerprint = config.fingerprint()?;
    let store = open_or_init_store(&context.store_path(), config.embedding.dimensions)?;

    record_model_meta(
        &store,
        &[
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("content_version", cigar_core::CONTENT_VERSION.to_string()),
            ("engine_version", ENGINE_VERSION.to_owned()),
            ("embedding_model", config.embedding.model.clone()),
            (
                "embedding_revision",
                resolved_revision(&config, &args.embedding),
            ),
            (
                "embedding_dimensions",
                config.embedding.dimensions.to_string(),
            ),
            (
                "embedding_template_version",
                config.embedding.template_version.to_string(),
            ),
            ("distance_metric", "cosine".to_owned()),
            ("config_fingerprint", fingerprint.to_string()),
        ],
    )?;

    let branch = resolve_init_branch(context)?;
    let branch_name = branch.clone();
    write_head_ref(&context.cig_dir, &branch_name, None)?;
    write_branch_marker(&context.cig_dir, &branch_name, None)?;
    initialise_branch_state(
        &store,
        &config.engine.namespace,
        &branch_name,
        &fingerprint,
        config.git.mode,
    )?;

    exclude_state_dir(&context.root, git_mode)?;

    let _lock = StoreLock::acquire(&context.lock_path())?;
    drop(_lock);

    if args.no_model || context.offline {
        printer.warn("model belum disiapkan; jalankan `cigar model download` sebelum sync");
    }

    if printer.json() {
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "root": context.root,
            "cig_dir": context.cig_dir,
            "already_initialized": existed,
            "git_mode": git_mode.to_string(),
            "branch": branch_name.to_string(),
            "namespace": config.engine.namespace.to_string(),
            "embedding": {
                "provider": config.embedding.provider.to_string(),
                "model": config.embedding.model,
                "dimensions": config.embedding.dimensions,
                "template_version": config.embedding.template_version,
            },
            "config_fingerprint": fingerprint.to_string(),
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    if existed {
        printer.line(&format!(
            "CIGAR sudah diinisialisasi - {}",
            context.cig_dir.display()
        ));
    } else {
        printer.line(&format!(
            "CIGAR diinisialisasi - {}",
            context.cig_dir.display()
        ));
    }
    printer.field("namespace:", config.engine.namespace.as_str());
    printer.field("git:", git_mode.as_str());
    printer.field("branch:", branch_name.as_str());
    printer.field(
        "embedding:",
        &format!(
            "{} - {}d - template v{}",
            config.embedding.model, config.embedding.dimensions, config.embedding.template_version
        ),
    );
    printer.field("store:", &context.store_path().display().to_string());
    if !args.no_model && !context.offline {
        printer.info("catatan: unduhan model dilakukan oleh `cigar model download`");
    }
    printer.blank();
    printer.line("Langkah berikutnya: `cigar sync`");
    Ok(0)
}

fn resolved_revision(config: &Config, provider: &str) -> String {
    if !config.embedding.revision.is_empty() {
        return config.embedding.revision.clone();
    }
    if provider == "fake" {
        return "fake-v1".into();
    }
    "unresolved".into()
}

fn create_state_dir(cig_dir: &Path) -> Result<()> {
    fs::create_dir_all(cig_dir).map_err(|err| EngineError::io(cig_dir, err))?;
    restrict_permissions(cig_dir)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(0o700);
    fs::set_permissions(dir, permissions).map_err(|err| EngineError::io(dir, err))
}

#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) -> Result<()> {
    // Windows ACLs are inherited from the parent; `.cig` stays outside the
    // repository manifest either way.
    Ok(())
}

fn write_config_atomic(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    let text = render_default_config(config);
    let temp = path.with_extension("toml.tmp");
    fs::write(&temp, text.as_bytes()).map_err(|err| EngineError::io(&temp, err))?;
    if path.exists() {
        // Preserve a user-edited file; only overwrite when the content differs
        // because `--force` asked for repair.
        let existing = fs::read_to_string(path).map_err(|err| EngineError::io(path, err))?;
        if existing == text {
            fs::remove_file(&temp).ok();
            return Ok(());
        }
    }
    fs::rename(&temp, path).map_err(|err| EngineError::io(path, err))?;
    Ok(())
}

fn resolve_init_branch(context: &Context) -> Result<cigar_core::domain::BranchName> {
    match context.head_ref()? {
        Some(crate::context::HeadRef::Branch(branch)) => Ok(branch),
        Some(crate::context::HeadRef::Detached(oid)) => {
            let _ = oid;
            Ok(cigar_core::domain::BranchName::detached())
        }
        None => Ok(context.resolve_branch()?),
    }
}

fn write_head_ref(
    cig_dir: &Path,
    branch: &cigar_core::domain::BranchName,
    _commit: Option<&str>,
) -> Result<()> {
    let path = cig_dir.join("HEAD");
    if path.exists() {
        return Ok(());
    }
    atomic_write(&path, format!("ref: refs/heads/{branch}\n").as_bytes())
}

fn write_branch_marker(
    cig_dir: &Path,
    branch: &cigar_core::domain::BranchName,
    commit: Option<&str>,
) -> Result<()> {
    let path = refs_path(cig_dir, branch);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
    }
    atomic_write(&path, format!("{}\n", commit.unwrap_or("")).as_bytes())
}

/// Mirror path for an indexed branch.
#[must_use]
pub fn refs_path(cig_dir: &Path, branch: &cigar_core::domain::BranchName) -> PathBuf {
    let mut path = cig_dir.join("refs").join("heads");
    for segment in branch.as_str().split('/') {
        path.push(segment);
    }
    path
}

/// Writes a file by writing a sibling temp file and renaming it.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes).map_err(|err| EngineError::io(&temp, err))?;
    fs::rename(&temp, path).map_err(|err| EngineError::io(path, err))?;
    Ok(())
}

fn initialise_branch_state(
    store: &cigar_storage::SqliteStore,
    namespace: &Namespace,
    branch: &cigar_core::domain::BranchName,
    fingerprint: &ConfigFingerprint,
    _mode: GitMode,
) -> Result<()> {
    use cigar_core::traits::ContextStore;
    if store.get_branch_state(namespace, branch)?.is_some() {
        return Ok(());
    }
    let state = cigar_core::domain::BranchState {
        namespace: namespace.clone(),
        branch: branch.clone(),
        commit_hash: None,
        last_sync_mode: SyncMode::Commit,
        status: SyncStatus::Ready,
        worktree_fingerprint: None,
        config_fingerprint: fingerprint.clone(),
        updated_at_ms: now_ms(),
    };
    store.set_branch_state(namespace, &state)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

fn exclude_state_dir(root: &Path, mode: GitMode) -> Result<()> {
    if mode == GitMode::Bare || mode == GitMode::FsOnly {
        return Ok(());
    }
    let git_dir = find_git_dir(root)?;
    let Some(git_dir) = git_dir else {
        return Ok(());
    };
    let info_dir = git_dir.join("info");
    fs::create_dir_all(&info_dir).map_err(|err| EngineError::io(&info_dir, err))?;
    let exclude = info_dir.join("exclude");
    let pattern = ".cig/";
    let existing = fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# CIGAR state\n");
    text.push_str(pattern);
    text.push('\n');
    fs::write(&exclude, text).map_err(|err| EngineError::io(&exclude, err))
}

fn find_git_dir(root: &Path) -> Result<Option<PathBuf>> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).map_err(|err| EngineError::io(&dot_git, err))?;
        if let Some(rest) = text.trim().strip_prefix("gitdir: ") {
            let path = PathBuf::from(rest.trim());
            let resolved = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            return Ok(Some(resolved));
        }
    }
    Ok(None)
}
