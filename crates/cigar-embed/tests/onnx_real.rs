//! Tests that exercise a real ONNX model. Ignored by default because they need
//! a downloaded model and the ONNX Runtime shared library:
//!
//! ```text
//! ORT_DYLIB_PATH=/path/to/onnxruntime.dll \
//! CIGAR_TEST_MODEL_DIR=/path/to/models \
//!   cargo test -p cigar-embed --test onnx_real -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use cigar_core::traits::Embedder;
use cigar_embed::manifest::ModelManifest;
use cigar_embed::OnnxEmbedder;

fn model_available() -> Option<(PathBuf, ModelManifest)> {
    let root = std::env::var_os("CIGAR_TEST_MODEL_DIR")?;
    let root = PathBuf::from(root);
    let manifest = ModelManifest::granite_125m();
    if !manifest.is_ready(&root) {
        eprintln!("model belum siap di {}", root.display());
        return None;
    }
    Some((root, manifest))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

#[test]
#[ignore = "butuh model ONNX nyata"]
fn embeds_with_real_model() {
    let Some((root, manifest)) = model_available() else {
        return;
    };
    let dir = manifest.directory(&root);
    let embedder = OnnxEmbedder::load(&dir, &manifest, 4, "", "").unwrap();
    assert_eq!(embedder.dimensions(), 768);

    let vectors = embedder
        .embed_batch(&[
            "fn authenticate(token: &str) -> bool".to_string(),
            "fn validate_signature(bytes: &[u8]) -> bool".to_string(),
            "const BUTTON_COLOR: &str = \"#fff\"".to_string(),
        ])
        .unwrap();
    assert_eq!(vectors.len(), 3);
    for vector in &vectors {
        assert_eq!(vector.len(), 768);
        let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norma {norm}");
        assert!(vector.iter().all(|x| x.is_finite()));
    }

    // Semantically adjacent code must be closer than unrelated UI code.
    let related = cosine(&vectors[0], &vectors[1]);
    let unrelated = cosine(&vectors[0], &vectors[2]);
    println!("cos(auth, validate)={related:.4} cos(auth, color)={unrelated:.4}");
    assert!(
        related > unrelated,
        "related={related} unrelated={unrelated}"
    );
}

#[test]
#[ignore = "butuh model ONNX nyata"]
fn batch_and_single_agree() {
    let Some((root, manifest)) = model_available() else {
        return;
    };
    let dir = manifest.directory(&root);
    let embedder = OnnxEmbedder::load(&dir, &manifest, 2, "", "").unwrap();
    let text = "fn parse(source: &str) -> Result<Ast, ParseError>";
    let single = embedder.embed_batch(&[text.to_owned()]).unwrap();
    let batched = embedder
        .embed_batch(&[text.to_owned(), "fn other() {}".to_owned()])
        .unwrap();
    let similarity = cosine(&single[0], &batched[0]);
    assert!(similarity > 0.999, "similarity {similarity}");
}
