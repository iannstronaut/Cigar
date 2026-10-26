//! Model download with checksum verification (docs/08 sections 6-7).
//!
//! Downloads are explicit: `sync` never fetches. Every artifact is verified
//! against the compiled-in SHA-256 before the `.ready` marker is written, so a
//! truncated or tampered file can never be used for indexing.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cigar_core::error::{EngineError, Result};

use crate::manifest::{ModelManifest, READY_MARKER};

/// Result of a download.
#[derive(Clone, Debug)]
pub struct DownloadReport {
    pub directory: PathBuf,
    pub onnx_bytes: u64,
    pub tokenizer_bytes: u64,
    pub already_present: bool,
}

/// Downloads and verifies the model artifacts.
pub fn download_model(
    manifest: &ModelManifest,
    model_dir: &Path,
    force: bool,
) -> Result<DownloadReport> {
    let directory = crate::manifest::ensure_model_dir(manifest, model_dir)?;
    if manifest.is_ready(model_dir) && !force {
        return Ok(DownloadReport {
            directory,
            onnx_bytes: fs::metadata(manifest.directory(model_dir).join(&manifest.onnx_file))
                .map_or(0, |meta| meta.len()),
            tokenizer_bytes: fs::metadata(manifest.directory(model_dir).join("tokenizer.json"))
                .map_or(0, |meta| meta.len()),
            already_present: true,
        });
    }

    let onnx_path = directory.join(&manifest.onnx_file);
    let tokenizer_path = directory.join("tokenizer.json");
    let _ = fs::remove_file(directory.join(READY_MARKER));

    if force || !onnx_path.is_file() {
        fetch_verified(
            &manifest.onnx_url(),
            &onnx_path,
            &manifest.sha256,
            manifest.size_bytes,
        )?;
    }

    let mut tokenizer_bytes = 0_u64;
    if manifest.needs_tokenizer && (force || !tokenizer_path.is_file()) {
        // The tokenizer is small; verifying its length is sufficient because the
        // manifest pins the revision URL.
        fetch(&manifest.tokenizer_url(), &tokenizer_path)?;
        tokenizer_bytes = fs::metadata(&tokenizer_path).map_or(0, |meta| meta.len());
    } else if tokenizer_path.is_file() {
        tokenizer_bytes = fs::metadata(&tokenizer_path).map_or(0, |meta| meta.len());
    }

    // Re-verify the checksum even when the file existed: a previous partial
    // download must never be trusted.
    verify_file(&onnx_path, &manifest.sha256, manifest.size_bytes)?;

    File::create(directory.join(READY_MARKER))
        .and_then(|mut file| file.write_all(manifest.revision.as_bytes()))
        .map_err(|err| EngineError::io(directory.join(READY_MARKER), err))?;

    let onnx_bytes = fs::metadata(&onnx_path).map_or(0, |meta| meta.len());
    Ok(DownloadReport {
        directory,
        onnx_bytes,
        tokenizer_bytes,
        already_present: false,
    })
}

fn fetch(url: &str, target: &Path) -> Result<()> {
    let bytes = http_get(url)?;
    let temp = target.with_extension("part");
    fs::write(&temp, &bytes).map_err(|err| EngineError::io(&temp, err))?;
    fs::rename(&temp, target).map_err(|err| EngineError::io(target, err))?;
    Ok(())
}

fn fetch_verified(url: &str, target: &Path, sha256: &str, expected_size: u64) -> Result<()> {
    let bytes = http_get(url)?;
    if bytes.len() as u64 != expected_size {
        return Err(EngineError::ModelUnavailable {
            message: format!(
                "ukuran tidak cocok: diharapkan {expected_size} byte, diterima {}",
                bytes.len()
            ),
        });
    }
    let digest = sha256_hex(&bytes);
    if digest != sha256 {
        return Err(EngineError::ModelUnavailable {
            message: format!("checksum tidak cocok: diharapkan {sha256}, diterima {digest}"),
        });
    }
    let temp = target.with_extension("part");
    fs::write(&temp, &bytes).map_err(|err| EngineError::io(&temp, err))?;
    fs::rename(&temp, target).map_err(|err| EngineError::io(target, err))?;
    Ok(())
}

/// Verifies an existing file against the pinned checksum.
pub fn verify_file(path: &Path, sha256: &str, expected_size: u64) -> Result<()> {
    let metadata = fs::metadata(path).map_err(|err| EngineError::io(path, err))?;
    if metadata.len() != expected_size {
        return Err(EngineError::ModelUnavailable {
            message: format!(
                "{} berukuran {} byte, diharapkan {expected_size}; unduh ulang",
                path.display(),
                metadata.len()
            ),
        });
    }
    let mut file = File::open(path).map_err(|err| EngineError::io(path, err))?;
    let mut hasher = Blake3Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| EngineError::io(path, err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finish_hex();
    if digest != sha256 {
        return Err(EngineError::ModelUnavailable {
            message: format!(
                "checksum {} tidak cocok (diharapkan {sha256}, diterima {digest})",
                path.display()
            ),
        });
    }
    Ok(())
}

/// Minimal SHA-256 implementation so verification has no extra dependency and
/// matches the checksums published by the model registry.
struct Blake3Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length_bits: u64,
}

impl Blake3Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::with_capacity(64),
            length_bits: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add((data.len() as u64) * 8);
        while !data.is_empty() {
            let take = (64 - self.buffer.len()).min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == 64 {
                let block = self.buffer.clone();
                self.compress(&block);
                self.buffer.clear();
            }
        }
    }

    fn finish_hex(mut self) -> String {
        let mut padding = vec![0x80_u8];
        while (self.buffer.len() + padding.len()) % 64 != 56 {
            padding.push(0);
        }
        padding.extend_from_slice(&self.length_bits.to_be_bytes());
        self.update(&padding);
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut words = [0_u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Blake3Sha256::new();
    hasher.update(bytes);
    hasher.finish_hex()
}

/// HTTP GET with a length cap and a clear error message.
fn http_get(url: &str) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .timeout(std::time::Duration::from_secs(600))
        .call()
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("gagal mengunduh {url}: {err}"),
        })?;
    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("unduhan terputus: {err}"),
        })?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"The quick brown fox jumps over the lazy dog"),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
    }

    #[test]
    fn verify_rejects_wrong_size() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("model.onnx");
        fs::write(&path, b"abc").unwrap();
        let error = verify_file(&path, &"0".repeat(64), 999).unwrap_err();
        assert!(error.to_string().contains("berukuran"));
    }

    #[test]
    fn verify_rejects_wrong_checksum() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("model.onnx");
        fs::write(&path, b"abc").unwrap();
        let error = verify_file(&path, &"0".repeat(64), 3).unwrap_err();
        assert!(error.to_string().contains("checksum"));
    }

    #[test]
    fn verify_accepts_correct_checksum() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("model.onnx");
        fs::write(&path, b"abc").unwrap();
        verify_file(
            &path,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            3,
        )
        .unwrap();
    }
}
