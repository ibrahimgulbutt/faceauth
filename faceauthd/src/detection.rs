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

/// Bounding box in original-image pixel coordinates, returned by `detect_with_bbox`.
/// Propagate this to subsequent frames with `crop_from_bbox` to get tight face crops
/// without re-running the expensive detection model.
pub struct FaceBBox {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

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
    /// Returns [(scores_opt, bboxes_opt); 3] for strides [8, 16, 32]
    /// (anchor counts 12800, 3200, 800 respectively).
    fn run_scrfd(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<[(Option<Vec<f32>>, Option<Vec<f32>>); 3]> {
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

        // Collect score ([N,1]) and bbox ([N,4]) tensors, keyed by anchor count N.
        // ScrFD outputs: N=12800 (stride 8), N=3200 (stride 16), N=800 (stride 32).
        let mut scale_tensors: [(Option<Vec<f32>>, Option<Vec<f32>>); 3] = Default::default();
        for (_, value) in outputs.iter() {
            if let Ok((shape, data)) = value.try_extract_tensor::<f32>() {
                if shape.len() == 2 {
                    let (n, c) = (shape[0], shape[1]);
                    let idx = match n { 12800 => 0, 3200 => 1, 800 => 2, _ => continue };
                    match c {
                        1 => scale_tensors[idx].0 = Some(data.to_vec()),
                        4 => scale_tensors[idx].1 = Some(data.to_vec()),
                        _ => {}
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

    /// Detect the best face and return a tight crop.
    /// For the authentication pipeline use `detect_with_bbox` instead so the bounding box
    /// can be propagated to subsequent frames.
    pub fn detect(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Option<ImageBuffer<Rgb<u8>, Vec<u8>>>> {
        Ok(self.detect_with_bbox(image)?.map(|(img, _)| img))
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
            let (scores_opt, bboxes_opt) = &scale_tensors[scale_idx];
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

    /// Crop `image` using a previously detected bounding box.
    ///
    /// Use this for frames 1-N to get tight face crops without re-running the
    /// detection model (face doesn't move significantly across a 200ms sequence).
    pub fn crop_from_bbox(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>, bbox: &FaceBBox) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        self.crop_with_margin(image, bbox)
    }
}
