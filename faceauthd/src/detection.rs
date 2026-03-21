use anyhow::Result;
use image::{ImageBuffer, Rgb};
use ndarray::Array;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use std::path::Path;
use log::{info, debug, warn};

use std::sync::Mutex;

pub struct FaceDetector {
    session: Mutex<Session>,
    threshold: f32,
}

/// Bounding box in original-image pixel coordinates (kept for the benchmark path).
pub struct FaceBBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// 5 facial landmark coordinates in original-image pixel space.
/// Order: left eye · right eye · nose tip · left mouth corner · right mouth corner
/// (matches ScrFD output order and the ArcFace training convention).
#[derive(Clone)]
pub struct FaceKpts {
    pub points: [[f32; 2]; 5],
}

/// ArcFace canonical landmark positions for a 112 × 112 output face.
/// Every ArcFace variant (R50, MobileNet, …) was trained against these targets,
/// so the embedding space is valid ONLY when faces are aligned to these coords.
const ARCFACE_REF: [[f32; 2]; 5] = [
    [38.2946, 51.6963], // left eye
    [73.5318, 51.5014], // right eye
    [56.0252, 71.7366], // nose tip
    [41.5493, 92.3655], // left mouth corner
    [70.7299, 92.2041], // right mouth corner
];

impl FaceDetector {
    pub fn new(model_path: &Path, threshold: f32) -> Result<Self> {
        // Check for quantized model first
        let model_name = model_path.file_stem().unwrap().to_str().unwrap();
        let parent = model_path.parent().unwrap();
        let quantized_path = parent.join(format!("{}_int8.onnx", model_name));
        
        // Solid Solution Fix: Check size > 0, not just existence
        let is_valid_quantized = if quantized_path.exists() {
            if let Ok(metadata) = std::fs::metadata(&quantized_path) {
                metadata.len() > 0
            } else {
                false
            }
        } else {
            false
        };

        let final_path = if is_valid_quantized {
            info!("Found valid quantized detection model: {:?}", quantized_path);
            quantized_path
        } else {
            if quantized_path.exists() {
                warn!("Found quantized model {:?} but it is empty/invalid. Falling back to standard model.", quantized_path);
            }
            model_path.to_path_buf()
        };

        // Threading Optimization:
        // Since we process multiple frames in parallel at the application level,
        // we MUST strictly limit internal ONNX Runtime threading to avoid contention.
        let intra_threads = 1;
        
        info!("Initializing FaceDetector with {} thread (optimized for parallel requests)", intra_threads);

        // Level1 = basic optimizations only. Level3 is extremely CPU-heavy at startup
        // (uses Intel oneDNN graph fusions) and ignores intra_threads for int8 models.
        // For a security daemon doing <20 inferences/day, Level1 startup is far better.
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level1)?
            .with_intra_threads(intra_threads)?     // parallel work within one node
            .with_inter_threads(intra_threads)?     // parallel scheduling between nodes
            .with_parallel_execution(false)?
            .commit_from_file(final_path)?;
        
        Ok(Self { session: Mutex::new(session), threshold })
    }

    /// Run the ScrFD ONNX model and collect output tensors by shape.
    /// Returns [(scores[N,1], bboxes[N,4], kpts[N,10]); 3] for strides [8, 16, 32].
    fn run_scrfd(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<[(Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<f32>>); 3]> {
        // Resize to 640×640 for model input
        let resized = image::imageops::resize(image, 640, 640, image::imageops::FilterType::Triangle);
        let mut input = Array::zeros((1, 3, 640, 640));
        for (x, y, pixel) in resized.enumerate_pixels() {
            let [r, g, b] = pixel.0;
            input[[0, 0, y as usize, x as usize]] = r as f32;
            input[[0, 1, y as usize, x as usize]] = g as f32;
            input[[0, 2, y as usize, x as usize]] = b as f32;
        }
        input.mapv_inplace(|v| (v - 127.5) / 128.0);

        let input_tensor = Value::from_array(input)?;
        let mut session = self.session.lock()
            .map_err(|e| anyhow::anyhow!("Detection session lock poisoned: {}", e))?;
        let outputs = session.run(ort::inputs![input_tensor])?;

        // Collect score ([N,1]), bbox ([N,4]), and keypoint ([N,10]) tensors.
        // ScrFD outputs: N=12800 (stride 8), N=3200 (stride 16), N=800 (stride 32).
        // The [N,10] tensors are 5 landmark (x,y) offset pairs — same stride encoding as bboxes.
        let mut scale_tensors: [(Option<Vec<f32>>, Option<Vec<f32>>, Option<Vec<f32>>); 3] = Default::default();
        for (_, value) in outputs.iter() {
            if let Ok((shape, data)) = value.try_extract_tensor::<f32>() {
                if shape.len() == 2 {
                    let (n, c) = (shape[0], shape[1]);
                    let idx = match n { 12800 => 0, 3200 => 1, 800 => 2, _ => continue };
                    match c {
                        1  => scale_tensors[idx].0 = Some(data.to_vec()),
                        4  => scale_tensors[idx].1 = Some(data.to_vec()),
                        10 => scale_tensors[idx].2 = Some(data.to_vec()),
                        _  => {}
                    }
                }
            }
        }
        // `outputs` and `session` (MutexGuard) drop here in LIFO order — lock released.
        Ok(scale_tensors)
    }

    /// Tight square crop centred on `bbox` with a 30% margin on each side.
    fn crop_with_margin(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>, bbox: &FaceBBox) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        let (orig_w, orig_h) = image.dimensions();
        let face_w = (bbox.x2 - bbox.x1).max(1.0);
        let face_h = (bbox.y2 - bbox.y1).max(1.0);
        let cx = (bbox.x1 + bbox.x2) / 2.0;
        let cy = (bbox.y1 + bbox.y2) / 2.0;

        // half-side = max(face_w, face_h) * 0.65  (= larger_dim/2 * 1.3 → 30% per side)
        let half = (face_w.max(face_h) * 0.65).ceil() as i64;
        let crop_x = (cx as i64 - half).clamp(0, (orig_w - 1) as i64) as u32;
        let crop_y = (cy as i64 - half).clamp(0, (orig_h - 1) as i64) as u32;
        let crop_size = ((half * 2) as u32)
            .min(orig_w.saturating_sub(crop_x))
            .min(orig_h.saturating_sub(crop_y));
        image::imageops::crop_imm(image, crop_x, crop_y, crop_size, crop_size).to_image()
    }

    /// Detect and return a landmark-aligned 112 × 112 crop.
    /// Used by the enrollment path so stored embeddings are in the same
    /// aligned space as authentication embeddings.
    pub fn detect(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Option<ImageBuffer<Rgb<u8>, Vec<u8>>>> {
        Ok(self.detect_aligned(image)?.map(|(img, _)| img))
    }

    /// Detect the best face, return both a tight crop AND the bounding box.
    ///
    /// The bounding box is in original-image pixel coordinates and can be passed to
    /// `crop_from_bbox` for frames 1-N to get consistent tight crops without running
    /// detection again (face tracking via propagated bbox).
    pub fn detect_with_bbox(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Option<(ImageBuffer<Rgb<u8>, Vec<u8>>, FaceBBox)>> {
        let (orig_w, orig_h) = image.dimensions();
        let scale_tensors = self.run_scrfd(image)?;

        // ScrFD distance-based bbox format: the 4 output values per anchor are
        // (l, t, r, b) — distances from anchor centre in stride-normalised units.
        //   cx = ix × stride  (InsightFace: no +0.5)
        //   x1 = cx − l×stride,  y1 = cy − t×stride
        //   x2 = cx + r×stride,  y2 = cy + b×stride
        // Anchor layout (2 per grid cell, row-major / y-outer):
        //   anchor_base = i / 2
        //   ix = anchor_base % grid_size,  iy = anchor_base / grid_size
        let stride_info: [(usize, u32, u32); 3] = [
            (12800, 8,  80),   // stride 8,  80×80 grid
            (3200,  16, 40),   // stride 16, 40×40 grid
            (800,   32, 20),   // stride 32, 20×20 grid
        ];

        let mut best_score = self.threshold; // only accept boxes above threshold
        let mut best_box_640: Option<[f32; 4]> = None;

        for (scale_idx, &(n, stride, grid_size)) in stride_info.iter().enumerate() {
            let (scores_opt, bboxes_opt, _) = &scale_tensors[scale_idx];
            let (scores, bboxes) = match (scores_opt, bboxes_opt) {
                (Some(s), Some(b)) => (s.as_slice(), b.as_slice()),
                _ => {
                    warn!("[Detection] Missing ScrFD tensors at stride {}", stride);
                    continue;
                }
            };

            for i in 0..n {
                let score = scores[i];
                if score <= best_score {
                    continue;
                }
                let anchor_base = i / 2;
                let ix = (anchor_base % grid_size as usize) as f32;
                let iy = (anchor_base / grid_size as usize) as f32;
                // Anchor centres in 640-space pixels: ix*stride (InsightFace convention, no +0.5)
                let cx = ix * stride as f32;
                let cy = iy * stride as f32;

                let d = &bboxes[i * 4..(i + 1) * 4];
                // Distances are stride-normalised — multiply by stride to get pixel offsets
                let stride_f = stride as f32;
                let x1 = cx - d[0] * stride_f;
                let y1 = cy - d[1] * stride_f;
                let x2 = cx + d[2] * stride_f;
                let y2 = cy + d[3] * stride_f;

                // Reject degenerate / out-of-bounds boxes
                if x2 > x1 + 2.0 && y2 > y1 + 2.0 {
                    best_score = score;
                    best_box_640 = Some([x1, y1, x2, y2]);
                }
            }
        }

        let [x1_640, y1_640, x2_640, y2_640] = match best_box_640 {
            Some(b) => b,
            None => {
                debug!("[Detection] No face above threshold {:.3}", self.threshold);
                return Ok(None);
            }
        };

        // Scale from 640×640 detection space back to original image dimensions.
        // Use separate x/y factors to account for the aspect-ratio distortion introduced
        // when the original (non-square) frame was squished to 640×640.
        let sx = orig_w as f32 / 640.0;
        let sy = orig_h as f32 / 640.0;
        let bbox = FaceBBox {
            x1: (x1_640 * sx).clamp(0.0, (orig_w.saturating_sub(1)) as f32),
            y1: (y1_640 * sy).clamp(0.0, (orig_h.saturating_sub(1)) as f32),
            x2: (x2_640 * sx).clamp(0.0, orig_w as f32),
            y2: (y2_640 * sy).clamp(0.0, orig_h as f32),
        };

        info!("[Detection] Face detected (score={:.4}), bbox=[{:.0},{:.0},{:.0},{:.0}]",
              best_score, bbox.x1, bbox.y1, bbox.x2, bbox.y2);

        let crop = self.crop_with_margin(image, &bbox);
        Ok(Some((crop, bbox)))
    }

    /// Crop using a previously detected bbox (benchmark path only).
    pub fn crop_from_bbox(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>, bbox: &FaceBBox) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        self.crop_with_margin(image, bbox)
    }

    // ─── Landmark alignment ───────────────────────────────────────────────────

    /// Analytical least-squares similarity transform mapping `src` → `dst`.
    ///
    /// Returns [a, b, tx, ty] where the forward map is:
    ///   u = a·x − b·y + tx
    ///   v = b·x + a·y + ty
    ///
    /// Derivation: minimise Σ‖M·sᵢ − dᵢ‖² over (a, b, tx, ty).
    /// With 5 correspondences the system is heavily over-determined;
    /// the closed-form is the Umeyama 4-DOF similarity estimator.
    fn estimate_similarity(src: &[[f32; 2]; 5], dst: &[[f32; 2]; 5]) -> [f32; 4] {
        let n = 5.0_f32;
        let cx: f32 = src.iter().map(|p| p[0]).sum::<f32>() / n;
        let cy: f32 = src.iter().map(|p| p[1]).sum::<f32>() / n;
        let cu: f32 = dst.iter().map(|p| p[0]).sum::<f32>() / n;
        let cv: f32 = dst.iter().map(|p| p[1]).sum::<f32>() / n;

        let sx2: f32 = src.iter()
            .map(|p| (p[0] - cx).powi(2) + (p[1] - cy).powi(2))
            .sum();
        if sx2 < 1e-6 {
            // Degenerate: all source landmarks identical → pure translation.
            return [1.0, 0.0, cu - cx, cv - cy];
        }

        let a: f32 = src.iter().zip(dst.iter())
            .map(|(s, d)| (s[0] - cx) * (d[0] - cu) + (s[1] - cy) * (d[1] - cv))
            .sum::<f32>() / sx2;
        let b: f32 = src.iter().zip(dst.iter())
            .map(|(s, d)| (s[0] - cx) * (d[1] - cv) - (s[1] - cy) * (d[0] - cu))
            .sum::<f32>() / sx2;

        [a, b, cu - a * cx + b * cy, cv - b * cx - a * cy]
    }

    /// Inverse-warp `image` into a 112 × 112 canonical face aligned to ARCFACE_REF.
    ///
    /// Given forward transform  u = a·x − b·y + tx,  v = b·x + a·y + ty,
    /// the inverse is:
    ///   x = ( a·u + b·v − a·tx − b·ty) / s²
    ///   y = (−b·u + a·v + b·tx − a·ty) / s²
    /// where s² = a² + b².  Source pixels outside the image are clamped to the edge.
    fn warp_to_112(
        image: &ImageBuffer<Rgb<u8>, Vec<u8>>,
        m: [f32; 4],
    ) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        let (src_w, src_h) = image.dimensions();
        let mut out: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::new(112, 112);
        let [a, b, tx, ty] = m;
        let s2 = a * a + b * b;
        if s2 < 1e-12 { return out; }

        let inv_a  =  a / s2;
        let inv_b  =  b / s2;
        let inv_tx = -(a * tx + b * ty) / s2;
        let inv_ty =  (b * tx - a * ty) / s2;

        let max_xi = (src_w as i32) - 1;
        let max_yi = (src_h as i32) - 1;

        for dy in 0u32..112 {
            for dx in 0u32..112 {
                let u = dx as f32;
                let v = dy as f32;
                let sx =  inv_a * u + inv_b * v + inv_tx;
                let sy = -inv_b * u + inv_a * v + inv_ty;

                let x0 = sx.floor() as i32;
                let y0 = sy.floor() as i32;
                let fx = sx - x0 as f32;
                let fy = sy - y0 as f32;

                let get = |xi: i32, yi: i32| -> [f32; 3] {
                    let xi = xi.clamp(0, max_xi) as u32;
                    let yi = yi.clamp(0, max_yi) as u32;
                    let p = image.get_pixel(xi, yi);
                    [p[0] as f32, p[1] as f32, p[2] as f32]
                };

                let p00 = get(x0,     y0    );
                let p10 = get(x0 + 1, y0    );
                let p01 = get(x0,     y0 + 1);
                let p11 = get(x0 + 1, y0 + 1);

                let lerp = |c: usize| -> u8 {
                    let top = p00[c] * (1.0 - fx) + p10[c] * fx;
                    let bot = p01[c] * (1.0 - fx) + p11[c] * fx;
                    (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8
                };
                out.put_pixel(dx, dy, image::Rgb([lerp(0), lerp(1), lerp(2)]));
            }
        }
        out
    }

    /// Detect the best face and return a **112 × 112 landmark-aligned** crop
    /// together with the detected 5-point keypoints.
    ///
    /// # Why alignment is required
    ///
    /// ArcFace was trained exclusively on landmark-aligned faces: every training
    /// sample was warped so that eyes and mouth land at fixed pixel positions in
    /// a 112 × 112 canvas.  Without this normalisation cosine similarity collapses
    /// whenever camera distance, head angle, or auto-exposure shifts the face by
    /// even a few percent — exactly what the logs showed (normal score 0.79–0.81,
    /// score after 65-min sleep 0.25–0.37; detection was fine, alignment was broken).
    ///
    /// # Frames 1-N
    ///
    /// Pass the returned `FaceKpts` to `align_from_kpts` for the rest of the
    /// capture sequence.  The face doesn't move significantly over ~200 ms so the
    /// same similarity transform is accurate for every frame.
    pub fn detect_aligned(
        &self,
        image: &ImageBuffer<Rgb<u8>, Vec<u8>>,
    ) -> Result<Option<(ImageBuffer<Rgb<u8>, Vec<u8>>, FaceKpts)>> {
        let (orig_w, orig_h) = image.dimensions();
        let scale_tensors = self.run_scrfd(image)?;

        let stride_info: [(usize, u32, u32); 3] = [
            (12800, 8,  80),
            (3200,  16, 40),
            (800,   32, 20),
        ];

        let mut best_score            = self.threshold;
        let mut best_box_640:  Option<[f32; 4]>       = None;
        let mut best_kpts_640: Option<[[f32; 2]; 5]>  = None;

        for (scale_idx, &(n, stride, grid_size)) in stride_info.iter().enumerate() {
            let (scores_opt, bboxes_opt, kpts_opt) = &scale_tensors[scale_idx];
            let (scores, bboxes) = match (scores_opt, bboxes_opt) {
                (Some(s), Some(b)) => (s.as_slice(), b.as_slice()),
                _ => { warn!("[Detection] Missing ScrFD tensors at stride {}", stride); continue; }
            };
            let kps: Option<&[f32]> = kpts_opt.as_deref();
            let stride_f = stride as f32;

            for i in 0..n {
                let score = scores[i];
                if score <= best_score { continue; }

                let anchor_base = i / 2;
                let ix = (anchor_base % grid_size as usize) as f32;
                let iy = (anchor_base / grid_size as usize) as f32;
                let cx = ix * stride_f;
                let cy = iy * stride_f;

                let d = &bboxes[i * 4..(i + 1) * 4];
                let x1 = cx - d[0] * stride_f;
                let y1 = cy - d[1] * stride_f;
                let x2 = cx + d[2] * stride_f;
                let y2 = cy + d[3] * stride_f;

                if x2 > x1 + 2.0 && y2 > y1 + 2.0 {
                    best_score   = score;
                    best_box_640 = Some([x1, y1, x2, y2]);
                    // Keypoints: stride-normalised offsets from the anchor centre,
                    // same encoding as bbox distances (multiply by stride to get pixels).
                    if let Some(k) = kps {
                        let o = &k[i * 10..(i + 1) * 10];
                        best_kpts_640 = Some([
                            [cx + o[0] * stride_f, cy + o[1] * stride_f], // left eye
                            [cx + o[2] * stride_f, cy + o[3] * stride_f], // right eye
                            [cx + o[4] * stride_f, cy + o[5] * stride_f], // nose
                            [cx + o[6] * stride_f, cy + o[7] * stride_f], // left mouth
                            [cx + o[8] * stride_f, cy + o[9] * stride_f], // right mouth
                        ]);
                    }
                }
            }
        }

        if best_box_640.is_none() {
            debug!("[Detection] No face above threshold {:.3}", self.threshold);
            return Ok(None);
        }

        let [x1_640, y1_640, x2_640, y2_640] = best_box_640.unwrap();
        let sx = orig_w as f32 / 640.0;
        let sy = orig_h as f32 / 640.0;
        info!("[Detection] Face detected (score={:.4}), bbox=[{:.0},{:.0},{:.0},{:.0}]",
              best_score, x1_640 * sx, y1_640 * sy, x2_640 * sx, y2_640 * sy);

        // ── Alignment ──────────────────────────────────────────────────────────
        if let Some(kpts_640) = best_kpts_640 {
            let kpts_orig = [
                [kpts_640[0][0] * sx, kpts_640[0][1] * sy],
                [kpts_640[1][0] * sx, kpts_640[1][1] * sy],
                [kpts_640[2][0] * sx, kpts_640[2][1] * sy],
                [kpts_640[3][0] * sx, kpts_640[3][1] * sy],
                [kpts_640[4][0] * sx, kpts_640[4][1] * sy],
            ];
            let m = Self::estimate_similarity(&kpts_orig, &ARCFACE_REF);
            let aligned = Self::warp_to_112(image, m);
            return Ok(Some((aligned, FaceKpts { points: kpts_orig })));
        }

        // ── Fallback: keypoints absent (non-standard model) — use bbox crop ────
        warn!("[Detection] Keypoints missing — falling back to bbox crop (alignment disabled)");
        let bbox = FaceBBox {
            x1: (x1_640 * sx).clamp(0.0, (orig_w - 1) as f32),
            y1: (y1_640 * sy).clamp(0.0, (orig_h - 1) as f32),
            x2: (x2_640 * sx).clamp(0.0, orig_w as f32),
            y2: (y2_640 * sy).clamp(0.0, orig_h as f32),
        };
        let cx  = (bbox.x1 + bbox.x2) / 2.0;
        let cy  = (bbox.y1 + bbox.y2) / 2.0;
        let w   = bbox.x2 - bbox.x1;
        let h   = bbox.y2 - bbox.y1;
        let synth = FaceKpts { points: [
            [cx - w * 0.18, cy - h * 0.20],
            [cx + w * 0.18, cy - h * 0.20],
            [cx,            cy            ],
            [cx - w * 0.12, cy + h * 0.25],
            [cx + w * 0.12, cy + h * 0.25],
        ]};
        Ok(Some((self.crop_with_margin(image, &bbox), synth)))
    }

    /// Align any frame to the ArcFace 112 × 112 canonical position using
    /// keypoints detected from frame 0.  No ONNX inference — pure bilinear
    /// warp, ~0.1 ms per frame.  Use for frames 1-N of the capture sequence.
    pub fn align_from_kpts(
        &self,
        image: &ImageBuffer<Rgb<u8>, Vec<u8>>,
        kpts: &FaceKpts,
    ) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        let m = Self::estimate_similarity(&kpts.points, &ARCFACE_REF);
        Self::warp_to_112(image, m)
    }
}
