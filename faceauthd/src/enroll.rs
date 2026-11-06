//! Guided enrollment: the daemon drives the camera, streams a mirrored
//! preview to the client and collects samples that pass every quality gate.
//! Nothing is stored here; the caller commits the result atomically.

use crate::auth::Pipeline;
use crate::camera::CameraManager;
use crate::detection::{self, Face};
use crate::recognition::best_similarity;
use faceauth_core::{EnrollEvent, EnrollFailure, EnrollMode, FaceBox, Hint, Pose, Response};
use image::RgbImage;
use log::{debug, info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::Sender;

/// Whole capture phase must finish within this time.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);
/// After this long on one step, any good-quality pose is accepted so nobody
/// gets stuck on a pose they cannot quite reach.
const STEP_RELAX_AFTER: Duration = Duration::from_secs(6);
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(350);
/// Minimum similarity to the samples already collected (same person).
const SAME_PERSON: f32 = 0.40;
/// Above this a new sample adds nothing.
const DUPLICATE: f32 = 0.92;
const PREVIEW_WIDTH: u32 = 640;
/// At most ~16 preview frames per second, whatever the camera's frame rate
/// (webcams drop to 15 fps or less in dim light).
const PREVIEW_INTERVAL: Duration = Duration::from_millis(60);
const PREVIEW_QUALITY: u8 = 78;

pub fn steps(mode: EnrollMode) -> Vec<Pose> {
    use Pose::*;
    match mode {
        EnrollMode::Replace => vec![Center, Center, Left, Left, Right, Right, Up, Up, Down, Down],
        EnrollMode::Append => vec![Center, Center, Left, Right, Up, Down],
    }
}

pub enum Finish {
    Completed(Vec<Vec<f32>>),
    Failed(EnrollFailure),
    /// The client hung up.
    Cancelled,
}

/// Run the capture phase. `existing` holds the stored samples (for `Append`)
/// so new ones can be checked against them. Blocks; call with the camera
/// lock held.
pub fn capture(
    camera: &CameraManager,
    p: &Pipeline,
    mode: EnrollMode,
    existing: &[Vec<f32>],
    tx: &Sender<Response>,
    cancel: &AtomicBool,
) -> Finish {
    let steps = steps(mode);
    let mut session = match camera.start_session(p.config.camera.warmup_frames) {
        Ok(s) => s,
        Err(e) => {
            warn!("Enrollment camera error: {e:#}");
            return Finish::Failed(EnrollFailure::CameraUnavailable);
        }
    };
    if tx.blocking_send(Response::Enroll(EnrollEvent::Started { steps: steps.clone() })).is_err() {
        return Finish::Cancelled;
    }

    let shared = Shared {
        latest: Mutex::new(None),
        seq: AtomicU64::new(0),
        face_box: Mutex::new(None),
        stop: AtomicBool::new(false),
    };
    let deadline = Instant::now() + CAPTURE_TIMEOUT;

    let outcome = std::thread::scope(|s| {
        let analysis = s.spawn(|| analyse(p, &steps, existing, &shared, tx, cancel, deadline));

        // Capture loop: keep the preview smooth regardless of analysis speed.
        let mut failures = 0;
        let mut n: u64 = 0;
        let mut last_preview: Option<Instant> = None;
        let mut capture_error = false;
        while !shared.stop.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
            let frame = match session.capture_frame() {
                Ok(f) => {
                    failures = 0;
                    Arc::new(f)
                }
                Err(e) => {
                    failures += 1;
                    if failures >= 10 {
                        warn!("Camera stopped delivering frames: {e:#}");
                        capture_error = true;
                        shared.stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    continue;
                }
            };
            n += 1;
            *shared.latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(frame.clone());
            shared.seq.store(n, Ordering::Release);

            if !matches!(last_preview, Some(t) if t.elapsed() < PREVIEW_INTERVAL) {
                last_preview = Some(Instant::now());
                let face = *shared.face_box.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(event) = preview(&frame, face) {
                    match tx.try_send(Response::Enroll(event)) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Closed(_)) => cancel.store(true, Ordering::Relaxed),
                    }
                }
            }
        }
        shared.stop.store(true, Ordering::Relaxed);
        let result = analysis.join().unwrap_or_else(|_| Finish::Failed(internal("analysis thread panicked")));
        match result {
            Finish::Cancelled if capture_error => Finish::Failed(EnrollFailure::CameraUnavailable),
            other => other,
        }
    });
    drop(session);
    outcome
}

struct Shared {
    latest: Mutex<Option<Arc<RgbImage>>>,
    seq: AtomicU64,
    face_box: Mutex<Option<FaceBox>>,
    stop: AtomicBool,
}

fn analyse(
    p: &Pipeline,
    steps: &[Pose],
    existing: &[Vec<f32>],
    shared: &Shared,
    tx: &Sender<Response>,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Finish {
    let mut gate = Gate::new(p, steps, existing);
    let mut last_seq = 0;
    let mut shown: Option<(usize, Hint)> = None;
    let mut candidate = (Hint::NoFace, 0u32);

    let finish = |f: Finish| {
        shared.stop.store(true, Ordering::Relaxed);
        f
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            return finish(Finish::Cancelled);
        }
        if shared.stop.load(Ordering::Relaxed) {
            // The capture loop gave up.
            return Finish::Cancelled;
        }
        if Instant::now() > deadline {
            return finish(Finish::Failed(EnrollFailure::TimedOut));
        }
        let seq = shared.seq.load(Ordering::Acquire);
        if seq == last_seq {
            std::thread::sleep(Duration::from_millis(8));
            continue;
        }
        last_seq = seq;
        let Some(frame) = shared.latest.lock().unwrap_or_else(|p| p.into_inner()).clone() else { continue };

        let faces = match p.detector.detect(&frame) {
            Ok(f) => f,
            Err(e) => return finish(Finish::Failed(internal(&format!("{e:#}")))),
        };
        *shared.face_box.lock().unwrap_or_else(|p| p.into_inner()) =
            detection::primary(&faces).map(|f| mirrored_box(f, frame.width(), frame.height()));

        let hint = match gate.consider(&frame, &faces) {
            Ok(h) => h,
            Err(e) => return finish(Finish::Failed(internal(&format!("{e:#}")))),
        };
        let collected = gate.collected.len();

        // Show a problem only once it persists for two analysed frames, so the
        // hint doesn't flicker; progress is shown immediately.
        let accepted = !matches!(shown, Some((c, _)) if c == collected);
        if candidate.0 == hint {
            candidate.1 += 1;
        } else {
            candidate = (hint, 1);
        }
        let stable = candidate.1 >= 2 || accepted;
        if stable && shown != Some((collected, hint)) {
            shown = Some((collected, hint));
            let pose = steps[collected.min(steps.len() - 1)];
            let event = EnrollEvent::Progress { collected, target: steps.len(), pose, hint };
            if tx.blocking_send(Response::Enroll(event)).is_err() {
                return finish(Finish::Cancelled);
            }
        }

        if collected >= steps.len() {
            return finish(Finish::Completed(std::mem::take(&mut gate.collected)));
        }
    }
}

/// Decides whether a frame becomes a sample, and if not, why.
struct Gate<'a> {
    p: &'a Pipeline<'a>,
    steps: &'a [Pose],
    existing: &'a [Vec<f32>],
    collected: Vec<Vec<f32>>,
    /// The user's own frontal pose, learned from the first sample.
    neutral: Option<(f32, f32)>,
    step_started: Instant,
    last_accept: Option<Instant>,
}

impl<'a> Gate<'a> {
    fn new(p: &'a Pipeline<'a>, steps: &'a [Pose], existing: &'a [Vec<f32>]) -> Self {
        Self { p, steps, existing, collected: Vec::new(), neutral: None, step_started: Instant::now(), last_accept: None }
    }

    fn consider(&mut self, frame: &RgbImage, faces: &[Face]) -> anyhow::Result<Hint> {
        let Some(face) = detection::primary(faces) else { return Ok(Hint::NoFace) };
        let others = faces.iter().filter(|f| !std::ptr::eq(*f, face));
        if let Some(other) = others.into_iter().find(|f| f.area() >= face.area() * 0.25) {
            debug!("Second face {:?} (score {:.2}) next to {:?} (score {:.2})", other.bbox, other.score, face.bbox, face.score);
            return Ok(Hint::MultipleFaces);
        }
        if face.width() < self.p.config.detection.min_face_size as f32 {
            return Ok(Hint::MoveCloser);
        }
        let brightness = detection::face_brightness(frame, face);
        if brightness < 45.0 {
            return Ok(Hint::TooDark);
        }
        if brightness > 215.0 {
            return Ok(Hint::TooBright);
        }

        let Some(&target) = self.steps.get(self.collected.len()) else { return Ok(Hint::Good) };
        let pose = head_pose(&face.kpts);
        let relaxed = self.step_started.elapsed() > STEP_RELAX_AFTER;
        match pose_fit(target, pose, self.neutral) {
            PoseFit::Ok => {}
            PoseFit::TooFar => return Ok(Hint::TooFar),
            PoseFit::Short(hint) if !relaxed => return Ok(hint),
            PoseFit::Short(_) => debug!("Step relaxed after {:?}", STEP_RELAX_AFTER),
        }
        if self.last_accept.is_some_and(|t| t.elapsed() < MIN_SAMPLE_GAP) {
            return Ok(Hint::Good);
        }

        let embedding = self.p.engine.embed(&detection::align(frame, &face.kpts))?;
        let mut reference: Vec<Vec<f32>> = self.existing.to_vec();
        reference.extend(self.collected.iter().cloned());
        let closest = best_similarity(&embedding, &reference);
        if !reference.is_empty() && closest < SAME_PERSON {
            info!("Rejected sample: similarity {closest:.2} to existing samples (different person?)");
            return Ok(Hint::DifferentPerson);
        }
        if closest > DUPLICATE {
            return Ok(Hint::MoveSlightly);
        }

        if target == Pose::Center && self.neutral.is_none() {
            self.neutral = Some(pose);
        }
        self.collected.push(embedding);
        self.last_accept = Some(Instant::now());
        self.step_started = Instant::now();
        info!(
            "Sample {}/{} ({:?}, yaw {:+.2}, pitch {:.2}, closest {closest:.2})",
            self.collected.len(),
            self.steps.len(),
            target,
            pose.0,
            pose.1
        );
        Ok(Hint::Good)
    }
}

/// (yaw, pitch) from the five landmarks, in the unmirrored camera image.
/// yaw: nose offset from the eye midpoint ÷ eye distance; positive when the
/// user turns to *their* left. pitch: nose position between the eye line (0)
/// and the mouth line (1); smaller when the chin goes up.
pub fn head_pose(k: &[[f32; 2]; 5]) -> (f32, f32) {
    let eye_mid = [(k[0][0] + k[1][0]) / 2.0, (k[0][1] + k[1][1]) / 2.0];
    let mouth_mid_y = (k[3][1] + k[4][1]) / 2.0;
    let eye_dist = ((k[1][0] - k[0][0]).powi(2) + (k[1][1] - k[0][1]).powi(2)).sqrt().max(1.0);
    let yaw = (k[2][0] - eye_mid[0]) / eye_dist;
    let pitch = (k[2][1] - eye_mid[1]) / (mouth_mid_y - eye_mid[1]).max(1.0);
    (yaw, pitch)
}

/// Pitch of the ArcFace reference face, used until the user's own is known.
const DEFAULT_PITCH: f32 = 0.49;

#[derive(Debug, PartialEq)]
enum PoseFit {
    Ok,
    Short(Hint),
    TooFar,
}

fn pose_fit(target: Pose, (yaw, pitch): (f32, f32), neutral: Option<(f32, f32)>) -> PoseFit {
    let (ny, np) = neutral.unwrap_or((0.0, DEFAULT_PITCH));
    let (dy, dp) = (yaw - ny, pitch - np);
    let turned = |d: f32, ok: f32, far: f32| {
        if d > far {
            PoseFit::TooFar
        } else if d > ok {
            PoseFit::Ok
        } else {
            PoseFit::Short(Hint::TurnMore)
        }
    };
    match target {
        // The first frontal sample is generous (it calibrates "neutral" for a
        // camera that sits above or beside the screen); later ones are tight.
        Pose::Center => {
            let (ty, tp) = if neutral.is_none() { (0.15, 0.15) } else { (0.10, 0.08) };
            if dy.abs() < ty && dp.abs() < tp {
                PoseFit::Ok
            } else {
                PoseFit::Short(Hint::LookStraight)
            }
        }
        Pose::Left => turned(dy, 0.13, 0.42),
        Pose::Right => turned(-dy, 0.13, 0.42),
        Pose::Up => turned(-dp, 0.06, 0.30),
        Pose::Down => turned(dp, 0.06, 0.30),
    }
}

/// Face box in mirrored, normalised preview coordinates.
fn mirrored_box(face: &Face, w: u32, h: u32) -> FaceBox {
    let (w, h) = (w as f32, h as f32);
    FaceBox {
        x: (1.0 - face.bbox[2] / w).clamp(0.0, 1.0),
        y: (face.bbox[1] / h).clamp(0.0, 1.0),
        w: (face.width() / w).clamp(0.0, 1.0),
        h: (face.height() / h).clamp(0.0, 1.0),
    }
}

fn preview(frame: &RgbImage, face: Option<FaceBox>) -> Option<EnrollEvent> {
    let (w, h) = frame.dimensions();
    let pw = PREVIEW_WIDTH.min(w);
    let ph = ((h as f32 * pw as f32 / w as f32).round() as u32).max(1);
    let small = image::imageops::thumbnail(frame, pw, ph);
    let mirrored = image::imageops::flip_horizontal(&small);
    let mut jpeg = Vec::with_capacity(64 * 1024);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, PREVIEW_QUALITY)
        .encode(mirrored.as_raw(), pw, ph, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(EnrollEvent::Frame { jpeg, width: pw, height: ph, face })
}

fn internal(message: &str) -> EnrollFailure {
    EnrollFailure::Internal { message: message.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ArcFace reference landmarks: a perfectly frontal face.
    const FRONT: [[f32; 2]; 5] =
        [[38.29, 51.70], [73.53, 51.50], [56.03, 71.74], [41.55, 92.37], [70.73, 92.20]];

    fn shifted_nose(dx: f32, dy: f32) -> [[f32; 2]; 5] {
        let mut k = FRONT;
        k[2][0] += dx;
        k[2][1] += dy;
        k
    }

    #[test]
    fn frontal_face_is_center() {
        let pose = head_pose(&FRONT);
        assert!(pose.0.abs() < 0.01 && (pose.1 - DEFAULT_PITCH).abs() < 0.01, "{pose:?}");
        assert_eq!(pose_fit(Pose::Center, pose, None), PoseFit::Ok);
    }

    #[test]
    fn nose_toward_image_right_is_users_left() {
        // In the raw (unmirrored) image the user's left is on the right.
        let pose = head_pose(&shifted_nose(8.0, 0.0));
        assert_eq!(pose_fit(Pose::Left, pose, Some((0.0, DEFAULT_PITCH))), PoseFit::Ok);
        assert_eq!(pose_fit(Pose::Right, pose, Some((0.0, DEFAULT_PITCH))), PoseFit::Short(Hint::TurnMore));
    }

    #[test]
    fn extreme_turns_are_too_far() {
        let pose = head_pose(&shifted_nose(-20.0, 0.0));
        assert_eq!(pose_fit(Pose::Right, pose, Some((0.0, DEFAULT_PITCH))), PoseFit::TooFar);
    }

    #[test]
    fn chin_up_moves_nose_toward_eyes() {
        let up = head_pose(&shifted_nose(0.0, -5.0));
        let down = head_pose(&shifted_nose(0.0, 5.0));
        assert_eq!(pose_fit(Pose::Up, up, Some((0.0, DEFAULT_PITCH))), PoseFit::Ok);
        assert_eq!(pose_fit(Pose::Down, down, Some((0.0, DEFAULT_PITCH))), PoseFit::Ok);
        assert_eq!(pose_fit(Pose::Up, down, Some((0.0, DEFAULT_PITCH))), PoseFit::Short(Hint::TurnMore));
    }

    #[test]
    fn neutral_calibration_shifts_center() {
        // Camera above the screen: the user's natural pose looks slightly down.
        let natural = head_pose(&shifted_nose(0.0, 3.0));
        assert_eq!(pose_fit(Pose::Center, natural, None), PoseFit::Ok);
        assert_eq!(pose_fit(Pose::Center, natural, Some(natural)), PoseFit::Ok);
    }

    #[test]
    fn mirrored_box_flips_horizontally() {
        let face = Face { score: 0.9, bbox: [0.0, 0.0, 100.0, 50.0], kpts: FRONT };
        let b = mirrored_box(&face, 1000, 500);
        assert!((b.x - 0.9).abs() < 1e-6 && (b.w - 0.1).abs() < 1e-6);
    }

    #[test]
    fn steps_match_design() {
        assert_eq!(steps(EnrollMode::Replace).len(), 10);
        assert_eq!(steps(EnrollMode::Append).len(), 6);
    }
}
