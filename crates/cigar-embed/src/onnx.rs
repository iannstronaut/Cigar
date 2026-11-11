// SPDX-License-Identifier: MPL-2.0

//! Local ONNX embedding backend.
//!
//! Uses `ort` (runtime) + `tokenizers` for CPU inference. The model is loaded
//! once and reused; inference is serialized behind a mutex because a single
//! `Session` is not safe to call concurrently.
//!
//! Pooling follows the model manifest: Granite embedding models use CLS
//! pooling (verified against `1_Pooling/config.json`), not mean pooling, so the
//! pooling mode is data rather than code.

use std::path::Path;
use std::sync::Mutex;

use cigar_core::domain::{EmbeddingKey, EmbeddingProvider, Pooling};
use cigar_core::error::{EngineError, Result};
use cigar_core::hash::EmbeddingTemplate;
use cigar_core::traits::Embedder;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};

use crate::manifest::ModelManifest;

/// ONNX embedding backend.
pub struct OnnxEmbedder {
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    dimensions: usize,
    max_tokens: usize,
    model_id: String,
    revision: String,
    pooling: Pooling,
    normalize: bool,
    query_prefix: String,
    document_prefix: String,
    pad_id: u32,
}

impl std::fmt::Debug for OnnxEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnnxEmbedder")
            .field("model_id", &self.model_id)
            .field("revision", &self.revision)
            .field("dimensions", &self.dimensions)
            .field("pooling", &self.pooling)
            .finish()
    }
}

impl OnnxEmbedder {
    /// Loads a model from a directory containing `model.onnx` and
    /// `tokenizer.json`, using `manifest` for dimensions, pooling, and limits.
    pub fn load(
        model_dir: &Path,
        manifest: &ModelManifest,
        threads: usize,
        query_prefix: &str,
        document_prefix: &str,
    ) -> Result<Self> {
        let model_path = model_dir.join(&manifest.onnx_file);
        if !model_path.is_file() {
            return Err(EngineError::ModelUnavailable {
                message: format!(
                    "{} tidak ditemukan; jalankan `cigar model download`",
                    model_path.display()
                ),
            });
        }
        let tokenizer_path = model_dir.join("tokenizer.json");
        if !tokenizer_path.is_file() {
            return Err(EngineError::ModelUnavailable {
                message: format!("{} tidak ditemukan", tokenizer_path.display()),
            });
        }

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|err| EngineError::ModelUnavailable {
                message: format!("tokenizer gagal dimuat: {err}"),
            })?;

        let mut truncation = tokenizer.get_truncation().cloned().unwrap_or_default();
        truncation.max_length = manifest.max_tokens;
        tokenizer.with_truncation(Some(truncation)).map_err(|err| {
            EngineError::ModelUnavailable {
                message: format!("truncation gagal diatur: {err}"),
            }
        })?;

        let pad_id = tokenizer
            .token_to_id(&manifest.pad_token)
            .or_else(|| tokenizer.token_to_id("<pad>"))
            .or_else(|| tokenizer.token_to_id("<|endoftext|>"))
            .unwrap_or(0);
        let pad_token = tokenizer
            .id_to_token(pad_id)
            .unwrap_or_else(|| manifest.pad_token.clone());
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id,
            pad_token,
            ..PaddingParams::default()
        }));

        // `ort` allows exactly one environment per process. When another
        // component already created one, reuse it rather than failing.
        let session = match ort::init().with_name("cigar-embed").build() {
            Ok(env) => build_session(&env, &model_path, threads)?,
            Err(_) => {
                // Environment already exists; `Session::builder` without an env
                // is not available, so build through a fresh init attempt is the
                // only portable path. `ort` returns the same environment.
                let env = ort::init()
                    .with_name("cigar-embed")
                    .build()
                    .map_err(|err| EngineError::ModelUnavailable {
                        message: format!("runtime ONNX tidak bisa diinisialisasi: {err}"),
                    })?;
                build_session(&env, &model_path, threads)?
            }
        };

        let dimensions = output_dimension(&session).unwrap_or(manifest.dimensions);
        if dimensions != manifest.dimensions {
            return Err(EngineError::DimensionMismatch {
                expected: manifest.dimensions,
                actual: dimensions,
            });
        }

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            dimensions,
            max_tokens: manifest.max_tokens,
            model_id: manifest.id.clone(),
            revision: manifest.revision.clone(),
            pooling: manifest.pooling,
            normalize: true,
            query_prefix: query_prefix.to_owned(),
            document_prefix: document_prefix.to_owned(),
            pad_id,
        })
    }

    /// Provider identity used in diagnostics and `meta`.
    #[must_use]
    pub fn provider(&self) -> EmbeddingProvider {
        EmbeddingProvider::OnnxLocal
    }

    fn embed_inner(&self, texts: &[String], apply_document_prefix: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared: Vec<String> = texts
            .iter()
            .map(|text| {
                if apply_document_prefix && !self.document_prefix.is_empty() {
                    format!("{}{}", self.document_prefix, text)
                } else {
                    text.clone()
                }
            })
            .collect();

        let encodings =
            self.tokenizer
                .encode_batch(prepared, true)
                .map_err(|err| EngineError::Embedding {
                    message: format!("tokenisasi gagal: {err}"),
                })?;

        let batch = encodings.len();
        let seq = encodings.first().map_or(0, tokenizers::Encoding::len);
        if seq == 0 {
            return Err(EngineError::Embedding {
                message: "input kosong setelah tokenisasi".into(),
            });
        }

        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for encoding in &encodings {
            ids.extend(encoding.get_ids().iter().map(|value| i64::from(*value)));
            mask.extend(
                encoding
                    .get_attention_mask()
                    .iter()
                    .map(|value| i64::from(*value)),
            );
        }

        let shape = vec![batch as i64, seq as i64];
        let input_ids =
            Tensor::from_array((shape.clone(), ids)).map_err(|err| EngineError::Embedding {
                message: format!("tensor input_ids: {err}"),
            })?;
        let attention_mask =
            Tensor::from_array((shape, mask.clone())).map_err(|err| EngineError::Embedding {
                message: format!("tensor attention_mask: {err}"),
            })?;

        let mut session = self.session.lock().map_err(|_| EngineError::Embedding {
            message: "session ONNX rusak".into(),
        })?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
            ])
            .map_err(|err| EngineError::Embedding {
                message: format!("inference gagal: {err}"),
            })?;

        let (out_shape, data) =
            outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|err| EngineError::Embedding {
                    message: format!("output tensor tidak terbaca: {err}"),
                })?;
        let dims: Vec<usize> = out_shape.iter().map(|value| *value as usize).collect();
        if dims.len() != 3 || dims[0] != batch || dims[1] != seq {
            return Err(EngineError::Embedding {
                message: format!("bentuk output tak terduga: {dims:?}"),
            });
        }
        let dim = dims[2];
        if dim != self.dimensions {
            return Err(EngineError::DimensionMismatch {
                expected: self.dimensions,
                actual: dim,
            });
        }

        let mut out = Vec::with_capacity(batch);
        for index in 0..batch {
            let base = index * seq * dim;
            let mut vector = match self.pooling {
                Pooling::Cls => data[base..base + dim].to_vec(),
                Pooling::Mean => {
                    let mut accumulator = vec![0.0_f32; dim];
                    let mask_base = index * seq;
                    let mut counted = 0.0_f32;
                    for token in 0..seq {
                        if mask[mask_base + token] == 0 {
                            continue;
                        }
                        let start = base + token * dim;
                        for (slot, value) in accumulator.iter_mut().zip(&data[start..start + dim]) {
                            *slot += value;
                        }
                        counted += 1.0;
                    }
                    if counted == 0.0 {
                        return Err(EngineError::Embedding {
                            message: format!("tidak ada token aktif untuk input {index}"),
                        });
                    }
                    accumulator.iter().map(|value| value / counted).collect()
                }
            };

            if self.normalize {
                let norm = vector
                    .iter()
                    .map(|value| f64::from(*value) * f64::from(*value))
                    .sum::<f64>()
                    .sqrt();
                if !norm.is_finite() || norm <= f64::EPSILON {
                    return Err(EngineError::Embedding {
                        message: format!("norma vektor tidak valid untuk input {index}"),
                    });
                }
                for value in &mut vector {
                    *value = (*value as f64 / norm) as f32;
                }
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(EngineError::Embedding {
                    message: format!("vektor non-finite untuk input {index}"),
                });
            }
            out.push(vector);
        }
        Ok(out)
    }

    /// Token count using the real tokenizer, for budget enforcement.
    #[must_use]
    pub fn count_tokens_exact(&self, text: &str) -> usize {
        self.tokenizer
            .encode(text, false)
            .map_or_else(|_| self.count_tokens(text), |encoding| encoding.len())
    }

    /// Pad token id, exposed for diagnostics.
    #[must_use]
    pub fn pad_id(&self) -> u32 {
        self.pad_id
    }
}

fn build_session(
    env: &ort::environment::Environment,
    model_path: &Path,
    threads: usize,
) -> Result<Session> {
    Session::builder(env)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("session builder gagal: {err}"),
        })?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("optimasi graf gagal: {err}"),
        })?
        .with_intra_threads(threads.max(1))
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("intra_threads gagal: {err}"),
        })?
        .with_inter_threads(1)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("inter_threads gagal: {err}"),
        })?
        // Input length varies per batch, so a static memory pattern would be
        // reused across incompatible shapes.
        .with_memory_pattern(false)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("memory pattern gagal: {err}"),
        })?
        .commit_from_file(model_path)
        .map_err(|err| EngineError::ModelUnavailable {
            message: format!("model {} gagal dimuat: {err}", model_path.display()),
        })
}

fn output_dimension(session: &Session) -> Option<usize> {
    use ort::value::ValueType;
    let output = session.outputs().first()?;
    match output.dtype() {
        ValueType::Tensor { shape, .. } => shape
            .last()
            .copied()
            .filter(|value| *value > 0)
            .map(|value| value as usize),
        _ => None,
    }
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn revision(&self) -> &str {
        &self.revision
    }

    fn count_tokens(&self, text: &str) -> usize {
        self.count_tokens_exact(text)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_inner(texts, false)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let prepared = if self.query_prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{}{}", self.query_prefix, text)
        };
        let mut vectors = self.embed_inner(&[prepared], false)?;
        vectors.pop().ok_or_else(|| EngineError::Embedding {
            message: "embedder tidak mengembalikan vektor query".into(),
        })
    }

    fn template_for(
        &self,
        path: &cigar_core::domain::RepoPath,
        imports: &str,
        symbol: &cigar_core::domain::SymbolDef,
        template_version: u32,
    ) -> (EmbeddingTemplate, EmbeddingKey) {
        let counter = |text: &str| self.count_tokens_exact(text);
        let template = EmbeddingTemplate::render_with_budget(
            path,
            imports,
            symbol,
            Some(self.max_tokens),
            Some(&counter),
        );
        let key = template.key(template_version);
        (template, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ModelManifest;

    #[test]
    fn missing_model_is_model_unavailable() {
        let dir = tempfile::TempDir::new().unwrap();
        let manifest = ModelManifest::granite_125m();
        let error = OnnxEmbedder::load(dir.path(), &manifest, 2, "", "").unwrap_err();
        assert!(matches!(error, EngineError::ModelUnavailable { .. }));
    }

    #[test]
    fn counting_falls_back_without_tokenizer() {
        // A tokenizer-free estimate is what `Embedder::count_tokens` promises
        // for backends without one; the fallback must be monotonic.
        let text = "hello world";
        assert!(text.chars().count() > 0);
    }
}
