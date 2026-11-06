//! ScrFD face detection and ArcFace landmark alignment.

use anyhow::{Context, Result};
use image::RgbImage;
use log::{debug, info, warn};
use ndarray::Array4;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// ScrFD input side. Frames are letterboxed (aspect ratio kept, padded) to
/// this size, matching how the model was trained.
const DET_SIZE: u32 = 640;
const STRIDES: [u32; 3] = [8, 16, 32];
const ANCHORS_PER_CELL: usize = 2;
const NMS_IOU: f32 = 0.4;

/// ArcFace canonical landmark positions for a 112 × 112 face. Every ArcFace
/// variant was trained on faces warped to these points, so embeddings are only
/// comparable when faces are aligned to them.
const ARCFACE_REF: [[f32; 2]; 5] = [
    [38.2946, 51.6963], // left eye (image left)
    [73.5318, 51.5014], // right eye
    [56.0252, 71.7366], // nose tip
    [41.5493, 92.3655], // left mouth corner
    [70.7299, 92.2041], // right mouth corner
];

/// A detected face in original-image pixel coordinates.
#[derive(Debug, Clone)]
pub struct Face {
    pub score: f32,
    /// x1, y1, x2, y2
    pub bbox: [f32; 4],
    /// left eye, right eye, nose, left mouth, right mouth (image left/right)
    pub kpts: [[f32; 2]; 5],
}

impl Face {
    pub fn width(&self) -> f32 {
        self.bbox[2] - self.bbox[0]
    }

    pub fn height(&self) -> f32 {
        self.bbox[3] - self.bbox[1]
    }

    pub fn area(&self) -> f32 {
        self.width().max(0.0) * self.height().max(0.0)
    }
}

pub struct FaceDetector {
    session: Mutex<Session>,
    threshold: f32,
}

impl FaceDetector {
    pub fn new(models_dir: &Path, threshold: f32) -> Result<Self> {
        let path = pick_model(models_dir, "det_500m")?;
        info!("Loading face detector {}", path.display());
        // Level1 keeps startup cheap; one intra/inter thread keeps the daemon
        // from saturating every core on login.
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(1)?
            .with_inter_threads(1)?
            .with_parallel_execution(false)?
            .commit_from_file(&path)
            .with_context(|| format!("Failed to load {}", path.display()))?;
        Ok(Self { session: Mutex::new(session), threshold })
    }

    /// All faces above the confidence threshold after non-maximum suppression,
    /// largest first.
    pub fn detect(&self, image: &RgbImage) -> Result<Vec<Face>> {
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return Ok(Vec::new());
        }
        let scale = (DET_SIZE as f32 / w as f32).min(DET_SIZE as f32 / h as f32);
        let input = letterbox(image, scale);
        let outputs = self.run(input)?;

        let mut candidates = Vec::new();
        for (level, &stride) in STRIDES.iter().enumerate() {
            let (scores, boxes, kps) = &outputs[level];
            let (Some(scores), Some(boxes)) = (scores, boxes) else {
                warn!("ScrFD output missing for stride {stride}");
                continue;
            };
            let grid = (DET_SIZE / stride) as usize;
            let s = stride as f32;
            for (i, &score) in scores.iter().enumerate() {
                if score < self.threshold {
                    continue;
                }
                let cell = i / ANCHORS_PER_CELL;
                let cx = (cell % grid) as f32 * s;
                let cy = (cell / grid) as f32 * s;
                let d = &boxes[i * 4..i * 4 + 4];
                let bbox = [
                    (cx - d[0] * s) / scale,
                    (cy - d[1] * s) / scale,
                    (cx + d[2] * s) / scale,
                    (cy + d[3] * s) / scale,
                ];
                if bbox[2] - bbox[0] < 2.0 || bbox[3] - bbox[1] < 2.0 {
                    continue;
                }
                let kpts = match kps {
                    Some(k) => {
                        let o = &k[i * 10..i * 10 + 10];
                        std::array::from_fn(|p| [(cx + o[p * 2] * s) / scale, (cy + o[p * 2 + 1] * s) / scale])
                    }
                    None => synthetic_kpts(&bbox),
                };
                candidates.push(Face { score, bbox, kpts });
            }
        }

        let mut faces = nms(candidates, NMS_IOU);
        faces.sort_by(|a, b| b.area().total_cmp(&a.area()));
        if let Some(f) = faces.first() {
            debug!("Detected {} face(s); largest score {:.3}, {:.0}×{:.0}px", faces.len(), f.score, f.width(), f.height());
        }
        Ok(faces)
    }

    /// Run ScrFD and group its nine outputs by stride:
    /// `[(scores[N], boxes[N*4], kps[N*10]); 3]`, where N is fixed by the
    /// stride and input size.
    #[allow(clippy::type_complexity)]
    fn run(&self, input: Array4<f32>) -> Result<[(Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<f32>>); 3]> {
        let tensor = Value::from_array(input)?;
        let mut session = self.session.lock().map_err(|e| anyhow::anyhow!("Detector lock poisoned: {e}"))?;
        let outputs = session.run(ort::inputs![tensor])?;
        let mut grouped: [(Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<f32>>); 3] = Default::default();
        for (_, value) in outputs.iter() {
            let Ok((shape, data)) = value.try_extract_tensor::<f32>() else { continue };
            // Shape is [N, C], or [1, N, C] for exports with a batch dimension.
            let (n, c) = match &shape[..] {
                [n, c] | [1, n, c] => (*n as usize, *c as usize),
                _ => continue,
            };
            let Some(level) = STRIDES.iter().position(|&s| {
                let g = (DET_SIZE / s) as usize;
                g * g * ANCHORS_PER_CELL == n
            }) else {
                continue;
            };
            match c {
                1 => grouped[level].0 = Some(data.to_vec()),
                4 => grouped[level].1 = Some(data.to_vec()),
                10 => grouped[level].2 = Some(data.to_vec()),
                _ => {}
            }
        }
        Ok(grouped)
    }
}

/// The face to authenticate: the largest one among those reasonably confident
/// (a large but uncertain detection must not win over a clear face).
pub fn primary(faces: &[Face]) -> Option<&Face> {
    let best = faces.iter().map(|f| f.score).fold(0.0f32, f32::max);
    faces.iter().filter(|f| f.score >= best * 0.6).max_by(|a, b| a.area().total_cmp(&b.area()))
}

/// Warp `image` so the landmarks land on ArcFace's canonical positions.
pub fn align(image: &RgbImage, kpts: &[[f32; 2]; 5]) -> RgbImage {
    let m = estimate_similarity(kpts, &ARCFACE_REF);
    warp_to_112(image, m)
}

/// Mean luminance (0–255) inside the face box.
pub fn face_brightness(image: &RgbImage, face: &Face) -> f32 {
    let (w, h) = image.dimensions();
    let x1 = (face.bbox[0].max(0.0) as u32).min(w.saturating_sub(1));
    let y1 = (face.bbox[1].max(0.0) as u32).min(h.saturating_sub(1));
    let x2 = (face.bbox[2].max(0.0) as u32).clamp(x1 + 1, w);
    let y2 = (face.bbox[3].max(0.0) as u32).clamp(y1 + 1, h);
    let (mut sum, mut n) = (0f64, 0u64);
    // Every 4th pixel is plenty for a mean.
    for y in (y1..y2).step_by(4) {
        for x in (x1..x2).step_by(4) {
            let p = image.get_pixel(x, y).0;
            sum += 0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64;
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64) as f32
    }
}

/// `<stem>.onnx` (InsightFace's original) if present, else a legacy
/// `<stem>_int8.onnx` left by versions before 1.0.
pub(crate) fn pick_model(dir: &Path, stem: &str) -> Result<PathBuf> {
    let full = dir.join(format!("{stem}.onnx"));
    let quantized = dir.join(format!("{stem}_int8.onnx"));
    for p in [&full, &quantized] {
        if std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false) {
            return Ok(p.clone());
        }
    }
    anyhow::bail!("Model not found: {} (run dist/fetch-models.sh)", full.display())
}

/// Resize keeping the aspect ratio, place at the top-left of a DET_SIZE²
/// canvas, normalise to ScrFD's (x − 127.5) / 128 and lay out as NCHW.
fn letterbox(image: &RgbImage, scale: f32) -> Array4<f32> {
    let (w, h) = image.dimensions();
    let nw = ((w as f32 * scale).round() as u32).clamp(1, DET_SIZE);
    let nh = ((h as f32 * scale).round() as u32).clamp(1, DET_SIZE);
    let resized = image::imageops::resize(image, nw, nh, image::imageops::FilterType::Triangle);
    let side = DET_SIZE as usize;
    let plane = side * side;
    // Padding is black (0) in pixel space, as in InsightFace's reference code.
    let mut data = vec![-127.5f32 / 128.0; 3 * plane];
    let raw = resized.as_raw();
    for y in 0..nh as usize {
        let row = &raw[y * nw as usize * 3..(y + 1) * nw as usize * 3];
        let base = y * side;
        for (x, px) in row.chunks_exact(3).enumerate() {
            data[base + x] = (px[0] as f32 - 127.5) / 128.0;
            data[plane + base + x] = (px[1] as f32 - 127.5) / 128.0;
            data[2 * plane + base + x] = (px[2] as f32 - 127.5) / 128.0;
        }
    }
    Array4::from_shape_vec((1, 3, side, side), data).expect("shape matches buffer")
}

fn iou(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Share of the smaller box covered by the larger one.
fn containment(a: &[f32; 4], b: &[f32; 4]) -> f32 {
    let ix = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let iy = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let smaller = ((a[2] - a[0]) * (a[3] - a[1])).min((b[2] - b[0]) * (b[3] - b[1]));
    if smaller <= 0.0 {
        0.0
    } else {
        ix * iy / smaller
    }
}

/// Non-maximum suppression. Besides ordinary overlap, a box lying mostly
/// inside a stronger one is dropped: ScrFD sometimes fires a partial-face box
/// inside the real one, whose IoU is too low for plain NMS, and that must not
/// count as a second person.
fn nms(mut candidates: Vec<Face>, threshold: f32) -> Vec<Face> {
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<Face> = Vec::new();
    for c in candidates {
        if kept.iter().all(|k| iou(&k.bbox, &c.bbox) <= threshold && containment(&k.bbox, &c.bbox) <= 0.6) {
            kept.push(c);
        }
    }
    kept
}

/// Rough landmark positions for models exported without keypoint outputs.
fn synthetic_kpts(b: &[f32; 4]) -> [[f32; 2]; 5] {
    let (cx, cy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
    let (w, h) = (b[2] - b[0], b[3] - b[1]);
    [
        [cx - w * 0.18, cy - h * 0.20],
        [cx + w * 0.18, cy - h * 0.20],
        [cx, cy],
        [cx - w * 0.12, cy + h * 0.25],
        [cx + w * 0.12, cy + h * 0.25],
    ]
}

/// Least-squares similarity transform (rotation, uniform scale, translation)
/// mapping `src` onto `dst`. Returns `[a, b, tx, ty]` for
/// `u = a·x − b·y + tx`, `v = b·x + a·y + ty` (Umeyama, 4 DOF).
fn estimate_similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [f32; 4] {
    let n = 5.0_f32;
    let cx = src.iter().map(|p| p[0]).sum::<f32>() / n;
    let cy = src.iter().map(|p| p[1]).sum::<f32>() / n;
    let cu = dst.iter().map(|p| p[0]).sum::<f32>() / n;
    let cv = dst.iter().map(|p| p[1]).sum::<f32>() / n;
    let sx2: f32 = src.iter().map(|p| (p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sum();
    if sx2 < 1e-6 {
        return [1.0, 0.0, cu - cx, cv - cy];
    }
    let a = src.iter().zip(dst).map(|(s, d)| (s[0] - cx) * (d[0] - cu) + (s[1] - cy) * (d[1] - cv)).sum::<f32>() / sx2;
    let b = src.iter().zip(dst).map(|(s, d)| (s[0] - cx) * (d[1] - cv) - (s[1] - cy) * (d[0] - cu)).sum::<f32>() / sx2;
    [a, b, cu - a * cx + b * cy, cv - b * cx - a * cy]
}

/// Inverse-warp into a 112 × 112 face with bilinear sampling; source pixels
/// outside the image are clamped to the edge.
fn warp_to_112(image: &RgbImage, m: [f32; 4]) -> RgbImage {
    let (src_w, src_h) = image.dimensions();
    let mut out = RgbImage::new(112, 112);
    let [a, b, tx, ty] = m;
    let s2 = a * a + b * b;
    if s2 < 1e-12 || src_w == 0 || src_h == 0 {
        return out;
    }
    let (ia, ib) = (a / s2, b / s2);
    let itx = -(a * tx + b * ty) / s2;
    let ity = (b * tx - a * ty) / s2;
    let (max_x, max_y) = (src_w as i32 - 1, src_h as i32 - 1);
    let get = |x: i32, y: i32| image.get_pixel(x.clamp(0, max_x) as u32, y.clamp(0, max_y) as u32).0;

    for dy in 0..112u32 {
        for dx in 0..112u32 {
            let (u, v) = (dx as f32, dy as f32);
            let sx = ia * u + ib * v + itx;
            let sy = -ib * u + ia * v + ity;
            let (x0, y0) = (sx.floor() as i32, sy.floor() as i32);
            let (fx, fy) = (sx - x0 as f32, sy - y0 as f32);
            let (p00, p10, p01, p11) = (get(x0, y0), get(x0 + 1, y0), get(x0, y0 + 1), get(x0 + 1, y0 + 1));
            let px = std::array::from_fn(|c| {
                let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
                let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
                (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8
            });
            out.put_pixel(dx, dy, image::Rgb(px));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Detector speed per model file on a synthetic 1280×720 frame.
    /// `FACEAUTH_MODELS_DIR=$PWD/models cargo test --release -p faceauthd bench_detectors -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the model files"]
    fn bench_detectors() {
        let dir = std::path::PathBuf::from(std::env::var("FACEAUTH_MODELS_DIR").unwrap_or_else(|_| "../models".into()));
        let frame = RgbImage::from_fn(1280, 720, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        for file in ["det_500m_int8.onnx", "det_500m.onnx"] {
            let path = dir.join(file);
            if !path.exists() {
                continue;
            }
            let tmp = std::env::temp_dir().join(format!("faceauth-bench-{}", std::process::id()));
            std::fs::create_dir_all(&tmp).unwrap();
            let _ = std::fs::remove_file(tmp.join("det_500m.onnx"));
            std::fs::copy(&path, tmp.join("det_500m.onnx")).unwrap();
            let det = FaceDetector::new(&tmp, 0.5).unwrap();
            for _ in 0..3 {
                det.detect(&frame).unwrap();
            }
            let runs = 15;
            let t = std::time::Instant::now();
            for _ in 0..runs {
                det.detect(&frame).unwrap();
            }
            println!("BENCH {file:<20} {:7.1} ms/frame", t.elapsed().as_secs_f64() * 1000.0 / runs as f64);
            std::fs::remove_dir_all(&tmp).unwrap();
        }
    }

    #[test]
    fn aligning_reference_points_is_identity() {
        let m = estimate_similarity(&ARCFACE_REF, &ARCFACE_REF);
        assert!((m[0] - 1.0).abs() < 1e-4 && m[1].abs() < 1e-4);
        assert!(m[2].abs() < 1e-3 && m[3].abs() < 1e-3);
    }

    #[test]
    fn similarity_recovers_scale_and_shift() {
        let src: [[f32; 2]; 5] = std::array::from_fn(|i| [ARCFACE_REF[i][0] * 2.0 + 10.0, ARCFACE_REF[i][1] * 2.0 + 20.0]);
        let m = estimate_similarity(&src, &ARCFACE_REF);
        assert!((m[0] - 0.5).abs() < 1e-4, "{m:?}");
        assert!((m[2] + 5.0).abs() < 1e-3 && (m[3] + 10.0).abs() < 1e-3, "{m:?}");
    }

    #[test]
    fn nms_merges_overlapping_boxes() {
        let face = |score, x: f32| Face { score, bbox: [x, 0.0, x + 100.0, 100.0], kpts: [[0.0; 2]; 5] };
        let kept = nms(vec![face(0.9, 0.0), face(0.8, 5.0), face(0.7, 300.0)], NMS_IOU);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].score, 0.9);
    }

    #[test]
    fn nms_drops_boxes_nested_in_a_face() {
        let outer = Face { score: 0.9, bbox: [100.0, 100.0, 300.0, 340.0], kpts: [[0.0; 2]; 5] };
        let inner = Face { score: 0.6, bbox: [140.0, 120.0, 260.0, 220.0], kpts: [[0.0; 2]; 5] };
        assert!(iou(&outer.bbox, &inner.bbox) < NMS_IOU, "plain NMS would keep it");
        assert_eq!(nms(vec![outer, inner], NMS_IOU).len(), 1);
    }

    #[test]
    fn primary_prefers_large_confident_face() {
        let face = |score, size: f32| Face { score, bbox: [0.0, 0.0, size, size], kpts: [[0.0; 2]; 5] };
        let faces = [face(0.9, 80.0), face(0.85, 200.0), face(0.3, 400.0)];
        assert_eq!(primary(&faces).unwrap().width(), 200.0);
    }

    #[test]
    fn letterbox_pads_with_black() {
        let img = RgbImage::from_pixel(1280, 720, image::Rgb([255, 255, 255]));
        let t = letterbox(&img, 0.5);
        assert!((t[[0, 0, 0, 0]] - 127.5 / 128.0).abs() < 1e-3);
        assert!((t[[0, 0, 639, 0]] + 127.5 / 128.0).abs() < 1e-3);
    }
}
