//! Exclusive store lock (docs/04 section12, docs/10 section15).
//!
//! Uses an OS advisory file lock as the authority. The metadata file content is
//! only a diagnostic aid: a stale pid in the file is not itself the lock.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use fs2::FileExt;

use cigar_core::error::{EngineError, Result};

/// Guard that holds the exclusive lock for its lifetime.
#[derive(Debug)]
pub struct StoreLock {
    file: File,
    path: PathBuf,
}

impl StoreLock {
    /// Acquires the lock immediately, or returns `LockBusy`.
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_with_wait(path, Duration::ZERO)
    }

    /// Acquires the lock, polling with 50???500 ms backoff up to `wait`.
    pub fn acquire_with_wait(path: &Path, wait: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| EngineError::io(parent, err))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|err| EngineError::io(path, err))?;

        let started = Instant::now();
        let mut backoff = Duration::from_millis(50);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => break,
                Err(_) if started.elapsed() < wait => {
                    sleep(backoff.min(wait.saturating_sub(started.elapsed())));
                    backoff = (backoff * 2).min(Duration::from_millis(500));
                }
                Err(_) => {
                    let pid = read_pid(path);
                    return Err(EngineError::LockBusy {
                        path: path.display().to_string(),
                        pid,
                    });
                }
            }
        }

        let pid = std::process::id();
        file.set_len(0).ok();
        file.write_all(format!("pid={pid}\n").as_bytes()).ok();
        file.flush().ok();

        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Path of the lock file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        // Explicit unlock first so Windows releases the handle region even if
        // another handle keeps the file open.
        let _ = FileExt::unlock(&self.file);
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("pid="))
        .and_then(|value| value.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn second_acquire_fails_without_wait() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let _first = StoreLock::acquire(&path).unwrap();
        let error = StoreLock::acquire(&path).unwrap_err();
        assert!(matches!(error, EngineError::LockBusy { .. }));
    }

    #[test]
    fn lock_is_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        {
            let _first = StoreLock::acquire(&path).unwrap();
        }
        let _second = StoreLock::acquire(&path).expect("lock harus bisa diambil ulang");
    }

    #[test]
    fn wait_allows_handoff() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let handle = std::thread::spawn(move || {
            let _guard = StoreLock::acquire(&path).unwrap();
            sleep(Duration::from_millis(120));
        });
        sleep(Duration::from_millis(20));
        let path = dir.path().join("lock");
        let lock = StoreLock::acquire_with_wait(&path, Duration::from_secs(2));
        handle.join().unwrap();
        assert!(lock.is_ok());
    }
}
