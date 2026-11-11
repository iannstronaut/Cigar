//! `cigar model download|status` (docs/10 section 13).

use cigar_core::error::Result;
use cigar_embed::manifest::ModelStatus;
#[cfg(feature = "semantic")]
use cigar_embed::{download_model, ensure_runtime};

use crate::cli::{ModelArgs, ModelCommand};
use crate::context::Context;
use crate::embedding::resolve_manifest;
use crate::output::{human_bytes, Printer};

/// Runs `cigar model ...`.
pub fn run(context: &Context, args: &ModelArgs, printer: &Printer) -> Result<i32> {
    match &args.command {
        ModelCommand::Status => status(context, printer),
        ModelCommand::Download {
            model,
            revision,
            model_dir,
            force,
        } => download(
            context,
            printer,
            model.as_deref(),
            revision.as_deref(),
            model_dir.clone(),
            *force,
        ),
    }
}

fn status(context: &Context, printer: &Printer) -> Result<i32> {
    let manifest = resolve_manifest(&context.config)?;
    let status = ModelStatus::inspect(&manifest, &context.config.embedding.model_dir);

    if printer.json() {
        let payload = serde_json::json!({
            "model": manifest.id,
            "revision": manifest.revision,
            "dimensions": manifest.dimensions,
            "max_tokens": manifest.max_tokens,
            "pooling": manifest.pooling.to_string(),
            "license": manifest.license,
            "provider": manifest.provider.to_string(),
            "ready": status.ready,
            "directory": status.directory,
            "onnx_bytes": status.onnx_bytes,
            "tokenizer_bytes": status.tokenizer_bytes,
            "runtime": cigar_embed::runtime::locate(
                &status.directory,
                context.config.embedding.model_dir.as_path(),
            )
            .map(|path| path.display().to_string()),
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    printer.line(&format!("Model · {}", manifest.id));
    printer.field(
        "revision:",
        &manifest.revision[..manifest.revision.len().min(12)],
    );
    printer.field(
        "dimensi:",
        &format!(
            "{} · {} token · pooling {}",
            manifest.dimensions, manifest.max_tokens, manifest.pooling
        ),
    );
    printer.field("lisensi:", &manifest.license);
    printer.field("lokasi:", &status.directory.display().to_string());
    printer.field(
        "onnx:",
        &status
            .onnx_bytes
            .map_or_else(|| "belum ada".to_owned(), human_bytes),
    );
    printer.field(
        "tokenizer:",
        &status
            .tokenizer_bytes
            .map_or_else(|| "belum ada".to_owned(), human_bytes),
    );
    printer.field("status:", if status.ready { "siap" } else { "belum siap" });
    let runtime = cigar_embed::runtime::locate(
        &status.directory,
        context.config.embedding.model_dir.as_path(),
    );
    printer.field(
        "runtime:",
        &runtime.map_or_else(
            || "onnxruntime tidak ditemukan".to_owned(),
            |path| path.display().to_string(),
        ),
    );
    Ok(0)
}

#[cfg(feature = "semantic")]
fn download(
    context: &Context,
    printer: &Printer,
    model: Option<&str>,
    revision: Option<&str>,
    model_dir: Option<std::path::PathBuf>,
    force: bool,
) -> Result<i32> {
    let mut manifest = match model {
        Some(name) => cigar_embed::ModelManifest::lookup(name).ok_or_else(|| {
            cigar_core::EngineError::ModelUnavailable {
                message: format!("model `{name}` tidak dikenal"),
            }
        })?,
        None => resolve_manifest(&context.config)?,
    };
    if let Some(revision) = revision {
        // Only accept the pinned revision: a different one has no checksum.
        if revision != manifest.revision {
            return Err(cigar_core::EngineError::ModelUnavailable {
                message: format!(
                    "revisi {} tidak punya checksum terdaftar; revisi valid: {}",
                    revision, manifest.revision
                ),
            });
        }
    }
    let directory = model_dir.unwrap_or_else(|| context.config.embedding.model_dir.clone());

    if context.offline {
        return Err(cigar_core::EngineError::ModelUnavailable {
            message: "mode --offline melarang unduhan model".into(),
        });
    }

    printer.info(&format!(
        "mengunduh {} ({}) ke {}",
        manifest.id,
        human_bytes(manifest.size_bytes),
        directory.display()
    ));
    let report = download_model(&manifest, &directory, force)?;

    // Report runtime availability, but never fail a successful download
    // because of it; `sync` gives the actionable error.
    let runtime = ensure_runtime(&report.directory, &directory);

    if printer.json() {
        let payload = serde_json::json!({
            "model": manifest.id,
            "revision": manifest.revision,
            "directory": report.directory,
            "onnx_bytes": report.onnx_bytes,
            "tokenizer_bytes": report.tokenizer_bytes,
            "already_present": report.already_present,
            "runtime": runtime.as_ref().ok().and_then(|found| found.as_ref().map(|path| path.display().to_string())),
        });
        printer.json_document(&payload);
        return Ok(0);
    }

    if report.already_present {
        printer.line(&format!(
            "Model sudah siap · {}",
            report.directory.display()
        ));
    } else {
        printer.line(&format!(
            "Model terunduh dan terverifikasi · {}",
            report.directory.display()
        ));
    }
    printer.field("onnx:", &human_bytes(report.onnx_bytes));
    printer.field("tokenizer:", &human_bytes(report.tokenizer_bytes));
    match runtime {
        Ok(Some(path)) => printer.field("runtime:", &path.display().to_string()),
        Ok(None) => printer.field("runtime:", "ORT_DYLIB_PATH sudah diset"),
        Err(error) => printer.warn(&error.to_string()),
    }
    manifest.revision.clear();
    Ok(0)
}

#[cfg(not(feature = "semantic"))]
fn download(
    context: &Context,
    _printer: &Printer,
    _model: Option<&str>,
    _revision: Option<&str>,
    _model_dir: Option<std::path::PathBuf>,
    _force: bool,
) -> Result<i32> {
    if context.offline {
        return Err(cigar_core::EngineError::ModelUnavailable {
            message: "unduhan model tidak tersedia dalam mode offline".into(),
        });
    }
    Err(cigar_core::EngineError::ModelUnavailable {
        message: "unduhan model tidak tersedia; bangun cigar dengan fitur `semantic`".into(),
    })
}
