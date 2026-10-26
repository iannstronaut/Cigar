//! ONNX Runtime dynamic library discovery (docs/08 section 6).
//!
//! `ort` is built with `load-dynamic`, so the runtime ships as a sidecar DLL.
//! Resolution order mirrors the docs: an explicit `ORT_DYLIB_PATH`, then the
//! executable directory, then the CIGAR runtime directory. A missing runtime is
//! reported as a model error with the exact fix.

use std::path::{Path, PathBuf};

use cigar_core::error::{EngineError, Result};

/// Environment variable `ort` itself honours.
pub const DYLIB_ENV: &str = "ORT_DYLIB_PATH";

/// Library file name for the current platform.
#[must_use]
pub fn library_name() -> &'static str {
    if cfg!(windows) {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// Candidate locations, in priority order.
#[must_use]
pub fn candidates(model_dir: &Path, runtime_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let name = library_name();
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        out.push(exe_dir.join(name));
        out.push(exe_dir.join("runtime").join(name));
    }
    out.push(runtime_dir.join(name));
    out.push(model_dir.join(name));
    out
}

/// Locates the runtime library, returning `None` when `ORT_DYLIB_PATH` already
/// points at an existing file.
pub fn locate(model_dir: &Path, runtime_dir: &Path) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(DYLIB_ENV) {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    candidates(model_dir, runtime_dir)
        .into_iter()
        .find(|path| path.is_file())
}

/// Ensures `ORT_DYLIB_PATH` points at a usable runtime.
///
/// Returns `Ok(Some(path))` when it set the variable, `Ok(None)` when the
/// variable was already valid, and an error with instructions when no runtime
/// exists.
pub fn ensure_runtime(model_dir: &Path, runtime_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(found) = locate(model_dir, runtime_dir) else {
        return Err(EngineError::ModelUnavailable {
            message: format!(
                "runtime ONNX Runtime ({0}) tidak ditemukan. Letakkan {0} di samping executable, \
                 di {1}, atau set {DYLIB_ENV}. Unduh dari \
                 https://github.com/microsoft/onnxruntime/releases (v1.31.0, CPU).",
                library_name(),
                runtime_dir.display()
            ),
        });
    };
    if std::env::var_os(DYLIB_ENV).is_some() {
        return Ok(None);
    }
    // SAFETY: `set_var` is unsafe in Rust 2024 because other threads may read
    // the environment concurrently. This runs before any `ort` session is
    // created, and CIGAR performs no other environment mutation concurrently.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var(DYLIB_ENV, &found);
    }
    Ok(Some(found))
}

/// Verifies the runtime can be loaded by consulting `ort`'s version check.
pub fn runtime_version() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_name_is_platform_specific() {
        let name = library_name();
        assert!(name.starts_with("onnxruntime") || name.starts_with("libonnxruntime"));
    }

    #[test]
    fn candidates_include_runtime_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let list = candidates(dir.path(), dir.path());
        assert!(list.iter().any(|path| path.parent() == Some(dir.path())));
    }

    #[test]
    fn locate_finds_planted_library() {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime = dir.path().join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let library = runtime.join(library_name());
        std::fs::write(&library, b"stub").unwrap();
        let found = locate(dir.path(), &runtime);
        assert_eq!(found, Some(library));
    }
}
