//! Face verification and the benchmark. Everything here blocks and expects
//! the caller to hold the camera lock.

use crate::camera::CameraManager;
use crate::config::Config;
use crate::detection::{self, FaceDetector};
use crate::liveness;
use crate::recognition::{best_similarity, FaceEngine};
use faceauth_core::{AuthOutcome, BenchmarkReport};
use image::RgbImage;
use log::{error, info};
use std::time::Instant;

pub struct Pipeline<'a> {
    pub config: &'a Config,
    pub detector: &'a FaceDetector,
    pub engine: &'a FaceEngine,
}

pub struct Verdict {
    pub outcome: AuthOutcome,
    pub best_score: f32,
}

impl Verdict {
    fn of(outcome: AuthOutcome) -> Self {
        Self { outcome, best_score: 0.0 }
    }
}

/// Capture a short frame sequence and compare it with `enrolled`.
///
/// A match needs a majority of frames above the threshold (one lucky frame is
/// not enough) and, when enabled, a liveness pass over the sequence.
pub fn verify(camera: &CameraManager, p: &Pipeline, enrolled: &[Vec<f32>]) -> Verdict {
    let started = Instant::now();
    let captured = match capture(camera, p) {
        Ok(c) => c,
        Err(e) => {
            error!("Camera error: {e:#}");
            return Verdict::of(AuthOutcome::CameraUnavailable);
        }
    };
    let Captured { frames, faces_first, first_embedding, .. } = captured;

    // Use frame 0's landmarks; if the sensor had not settled yet (common right
    // after resume), give the last frame a chance.
    let mut kpts = detection::primary(&faces_first).map(|f| f.kpts);
    // Frame 0 was already recognised during capture, with these landmarks.
    let mut first_embedding = if kpts.is_some() { first_embedding } else { None };
    if kpts.is_none() && frames.len() > 1 {
        if let Ok(faces_last) = p.detector.detect(frames.last().expect("non-empty")) {
            kpts = detection::primary(&faces_last).map(|f| f.kpts);
        }
    }
    let Some(kpts) = kpts else {
        let brightness = mean_brightness(&frames[0]);
        info!("No face found (frame brightness {brightness:.0})");
        return Verdict::of(if !(20.0..=235.0).contains(&brightness) { AuthOutcome::BadLighting } else { AuthOutcome::NoFace });
    };

    if p.config.security.require_liveness && !liveness::check_liveness(&frames) {
        return Verdict::of(AuthOutcome::NotLive);
    }

    let threshold = p.config.recognition.match_threshold;
    let required = frames.len() / 2 + 1;
    let mut best = -1.0f32;
    let mut votes = 0;
    for (i, frame) in frames.iter().enumerate() {
        let embedding = match (i, first_embedding.take()) {
            (0, Some(precomputed)) => precomputed,
            _ => p.engine.embed(&detection::align(frame, &kpts)),
        };
        match embedding {
            Ok(emb) => {
                let score = best_similarity(&emb, enrolled);
                best = best.max(score);
                if score > threshold {
                    votes += 1;
                }
            }
            Err(e) => error!("Embedding failed: {e:#}"),
        }
        // Stop as soon as the vote is decided either way.
        let remaining = frames.len() - i - 1;
        if votes >= required || votes + remaining < required {
            break;
        }
    }
    info!(
        "Verification: {votes}/{} frames above {threshold:.2} (need {required}), best {best:.3}, {} ms",
        frames.len(),
        started.elapsed().as_millis()
    );
    Verdict {
        outcome: if votes >= required { AuthOutcome::Matched } else { AuthOutcome::NotMatched },
        best_score: best,
    }
}

/// Time each stage of the pipeline without comparing against anyone.
pub fn benchmark(camera: &CameraManager, p: &Pipeline) -> anyhow::Result<BenchmarkReport> {
    let total = Instant::now();
    let captured = capture(camera, p)?;
    let face = detection::primary(&captured.faces_first).map(|f| f.kpts);
    let rec_start = Instant::now();
    if let Some(kpts) = face {
        // Frame 0 was recognised during capture; time the rest here.
        for frame in captured.frames.iter().skip(1) {
            p.engine.embed(&detection::align(frame, &kpts))?;
        }
    }
    let recognition_ms = captured.first_embed_ms + ms(rec_start);
    let (w, h) = captured.frames[0].dimensions();
    Ok(BenchmarkReport {
        camera: camera.describe().unwrap_or_else(|| "unknown".into()),
        recognizer: p.config.recognition.model.describe().to_string(),
        resolution: format!("{w}×{h}"),
        capture_ms: captured.capture_ms,
        detection_ms: captured.detection_ms,
        recognition_ms,
        total_ms: ms(total),
    })
}

struct Captured {
    frames: Vec<RgbImage>,
    faces_first: Vec<detection::Face>,
    /// Embedding of frame 0 (aligned with its own primary face), computed
    /// while the camera was still capturing.
    first_embedding: Option<anyhow::Result<Vec<f32>>>,
    capture_ms: f32,
    detection_ms: f32,
    first_embed_ms: f32,
}

/// Open the camera and grab the sequence. While the remaining frames are being
/// captured, frame 0 is detected and recognised on another thread, which hides
/// most of that work behind the camera. The camera is closed before returning
/// so the LED goes off as early as possible.
fn capture(camera: &CameraManager, p: &Pipeline) -> anyhow::Result<Captured> {
    let cap_start = Instant::now();
    let mut session = camera.start_session(p.config.camera.warmup_frames)?;
    let first = session.capture_frame()?;
    let cam = &p.config.camera;
    let (analysed, rest) = std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let t = Instant::now();
            let faces = p.detector.detect(&first);
            let detection_ms = ms(t);
            let t = Instant::now();
            let embedding = faces
                .as_ref()
                .ok()
                .and_then(|f| detection::primary(f))
                .map(|face| p.engine.embed(&detection::align(&first, &face.kpts)));
            (faces, detection_ms, embedding, ms(t))
        });
        let rest = session.capture_sequence(cam.sequence_length.saturating_sub(1), cam.sequence_interval_ms);
        (worker.join(), rest)
    });
    drop(session);
    let capture_ms = ms(cap_start);
    let (faces_first, detection_ms, first_embedding, first_embed_ms) = match analysed {
        Ok((Ok(faces), det_ms, emb, emb_ms)) => (faces, det_ms, emb, emb_ms),
        Ok((Err(e), ..)) => return Err(e),
        Err(_) => anyhow::bail!("Analysis thread panicked"),
    };
    let mut frames = vec![first];
    // A failure capturing the tail is not fatal: fewer frames just make the
    // vote stricter.
    frames.extend(rest.unwrap_or_default());
    Ok(Captured { frames, faces_first, first_embedding, capture_ms, detection_ms, first_embed_ms })
}

fn mean_brightness(img: &RgbImage) -> f32 {
    let raw = img.as_raw();
    if raw.is_empty() {
        return 0.0;
    }
    // Sampling every 7th byte is plenty for a mean.
    let (sum, n) = raw.iter().step_by(7).fold((0u64, 0u64), |(s, n), &v| (s + v as u64, n + 1));
    sum as f32 / n as f32
}

fn ms(t: Instant) -> f32 {
    t.elapsed().as_secs_f32() * 1000.0
}
