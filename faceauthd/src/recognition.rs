//! ArcFace face embeddings.

use anyhow::{Context, Result};
use image::RgbImage;
use log::info;
use ndarray::Array4;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

pub use crate::config::RecognitionModel;

const INPUT: u32 = 112;
/// Aligned faces darker than this (mean, 0–255) are brightened.
const DARK_FACE: f32 = 70.0;

pub struct FaceEngine {
    session: Mutex<Session>,
    /// First 16 hex digits of the model file's SHA-256. Stored with every
    /// profile: embeddings from different models are not comparable.
    pub model_id: String,
}

impl FaceEngine {
    /// Load the configured recognizer. Fails (closed) when its file is
    /// missing: without a real model there is no way to tell faces apart, and
    /// silently falling back to another model would invalidate every profile.
    pub fn new(models_dir: &Path, model: RecognitionModel, threads: usize) -> Result<Self> {
        let path = crate::detection::pick_model(models_dir, model.file_stem()).with_context(|| {
            format!(
                "The {} recognizer is not installed. Re-run the installer, or set recognition.model = \"{}\" in the config",
                model.name(),
                model.other().name()
            )
        })?;
        Self::from_file(&path, threads)
    }

    pub fn from_file(path: &Path, threads: usize) -> Result<Self> {
        let model_id = file_id(path)?;
        info!("Loading face recognizer {} (model {model_id}, {threads} thread(s))", path.display());
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(threads.max(1))?
            .with_inter_threads(1)?
            .with_parallel_execution(false)?
            .commit_from_file(path)
            .with_context(|| format!("Failed to load {}", path.display()))?;
        Ok(Self { session: Mutex::new(session), model_id })
    }

    /// L2-normalised embedding of an aligned 112 × 112 face. Dark faces are
    /// brightened first (the same way at setup and at login), which keeps
    /// scores up in dim light; normally lit faces are untouched.
    pub fn embed(&self, face: &RgbImage) -> Result<Vec<f32>> {
        let resized;
        let face = if face.dimensions() == (INPUT, INPUT) {
            face
        } else {
            resized = image::imageops::resize(face, INPUT, INPUT, image::imageops::FilterType::CatmullRom);
            &resized
        };
        let brightened = crate::detection::brighten_if_dark(face, DARK_FACE);
        let face = brightened.as_ref().unwrap_or(face);
        let tensor = Value::from_array(preprocess(face))?;
        let mut session = self.session.lock().map_err(|e| anyhow::anyhow!("Recognizer lock poisoned: {e}"))?;
        let outputs = session.run(ort::inputs![tensor])?;
        let (_, data) = outputs[0].try_extract_tensor::<f32>()?;
        let norm = data.iter().map(|x| x * x).sum::<f32>().sqrt();
        if !norm.is_finite() || norm < 1e-6 {
            anyhow::bail!("Recognizer produced a degenerate embedding");
        }
        Ok(data.iter().map(|x| x / norm).collect())
    }
}

/// Cosine similarity of two normalised embeddings. Embeddings of different
/// length (another model, or a damaged profile) never match.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return -1.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Best similarity of `emb` against any of `refs` (−1 when `refs` is empty).
pub fn best_similarity(emb: &[f32], refs: &[Vec<f32>]) -> f32 {
    refs.iter().map(|r| similarity(emb, r)).fold(-1.0, f32::max)
}

/// RGB, NCHW, (x − 127.5) / 127.5 as in InsightFace's ArcFace reference code.
fn preprocess(face: &RgbImage) -> Array4<f32> {
    let side = INPUT as usize;
    let plane = side * side;
    let mut data = vec![0f32; 3 * plane];
    for (i, px) in face.as_raw().chunks_exact(3).enumerate() {
        data[i] = (px[0] as f32 - 127.5) / 127.5;
        data[plane + i] = (px[1] as f32 - 127.5) / 127.5;
        data[2 * plane + i] = (px[2] as f32 - 127.5) / 127.5;
    }
    Array4::from_shape_vec((1, 3, side, side), data).expect("shape matches buffer")
}

fn file_id(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    Ok(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inference speed of every recognizer file present. Content does not
    /// affect speed, so a synthetic image stands in for a face.
    /// `FACEAUTH_MODELS_DIR=$PWD/models cargo test --release -p faceauthd bench_recognizers -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the model files"]
    fn bench_recognizers() {
        let dir = std::path::PathBuf::from(std::env::var("FACEAUTH_MODELS_DIR").unwrap_or_else(|_| "../models".into()));
        let face = RgbImage::from_fn(112, 112, |x, y| image::Rgb([(x * 2) as u8, (y * 2) as u8, ((x + y) % 256) as u8]));
        for file in ["arcface_int8.onnx", "arcface.onnx", "arcface_r50_int8.onnx", "arcface_r50.onnx"] {
            let path = dir.join(file);
            if !path.exists() {
                continue;
            }
            for threads in [1, 2, 4] {
                let engine = FaceEngine::from_file(&path, threads).unwrap();
                for _ in 0..3 {
                    engine.embed(&face).unwrap();
                }
                let runs = 15;
                let t = std::time::Instant::now();
                for _ in 0..runs {
                    engine.embed(&face).unwrap();
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0 / runs as f64;
                println!("BENCH {file:<24} threads={threads}  {ms:7.1} ms/embedding");
            }
        }
    }

    #[test]
    fn mismatched_embeddings_never_match() {
        assert_eq!(similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), -1.0);
        assert_eq!(similarity(&[], &[]), -1.0);
        assert_eq!(best_similarity(&[1.0], &[]), -1.0);
    }

    #[test]
    fn identical_embeddings_score_one() {
        let e = vec![0.6, 0.8];
        assert!((similarity(&e, &e) - 1.0).abs() < 1e-6);
    }
}
