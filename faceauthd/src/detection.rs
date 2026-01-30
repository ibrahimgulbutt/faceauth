use anyhow::Result;
use image::{ImageBuffer, Rgb};
use ndarray::Array;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use std::path::Path;
use log::{info, warn};

use std::sync::Mutex;

pub struct FaceDetector {
    session: Mutex<Session>,
    threshold: f32,
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

        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(intra_threads)?
            .with_parallel_execution(false)?
            .commit_from_file(final_path)?;
        
        Ok(Self { session: Mutex::new(session), threshold })
    }

    /// Returns a center crop without running detection (Adaptive Optimization)
    pub fn get_center_crop(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        let (w, h) = image.dimensions();
        let size = std::cmp::min(w, h);
        let x = (w - size) / 2;
        let y = (h - size) / 2;
        image::imageops::crop_imm(image, x, y, size, size).to_image()
    }

    pub fn detect(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Option<ImageBuffer<Rgb<u8>, Vec<u8>>>> {
        info!("[Detection] Starting detection on image {}x{}", image.width(), image.height());
        
        // 1. Preprocess: Resize to 640x640
        let (orig_w, orig_h) = image.dimensions();
        let target_size = (640, 640);
        let resized = image::imageops::resize(image, target_size.0, target_size.1, image::imageops::FilterType::Triangle);
        
        let mut input = Array::zeros((1, 3, 640, 640));
        for (x, y, pixel) in resized.enumerate_pixels() {
            let [r, g, b] = pixel.0;
            input[[0, 0, y as usize, x as usize]] = r as f32;
            input[[0, 1, y as usize, x as usize]] = g as f32;
            input[[0, 2, y as usize, x as usize]] = b as f32;
        }
        
        // Normalize: (x - 127.5) / 128.0
        input.mapv_inplace(|x| (x - 127.5) / 128.0);

        let input_tensor = Value::from_array(input)?;
        info!("[Detection] Running ONNX inference...");
        let mut session = self.session.lock()
            .map_err(|e| anyhow::anyhow!("Detection session lock poisoned: {}", e))?;
        let outputs = session.run(ort::inputs![input_tensor])?;
        info!("[Detection] Inference complete. Outputs: {}", outputs.len());
        
        // SCRFD typically has 6 or 9 outputs. 
        // 3 strides: 8, 16, 32.
        // For each stride: score map, bbox map, (optional kps map).
        // We need to identify which output is which. 
        // Usually they are ordered: score_8, bbox_8, kps_8, score_16, ...
        // Or just score_8, bbox_8, score_16, bbox_16...
        
        // Simplified logic: We will look for the highest score across all outputs that look like score maps.
        // A score map has shape (1, 1 or 2, H, W).
        
        let mut best_score = 0.0f32;
        // let mut best_bbox: Option<(f32, f32, f32, f32)> = None; // (x1, y1, x2, y2) in 640x640 space - Unused for now

        // We iterate through outputs to find score maps
        // This is a heuristic because we don't know exact output names/order without inspecting the model.
        // But typically, score maps have 1 or 2 channels. Bbox maps have 4 channels.
        
        // Let's assume standard SCRFD export where we have 6 or 9 outputs.
        // We'll try to parse them based on shape.
        
        // Strides: 8, 16, 32
        // 640 / 8 = 80
        // 640 / 16 = 40
        // 640 / 32 = 20
        
        info!("[Detection] Processing detection outputs...");
        for (name, value) in outputs.iter() {
            if let Ok((shape, data)) = value.try_extract_tensor::<f32>() {
                info!("[Detection] Output '{}': shape={:?}", name, shape);
                
                // Handle flattened shapes [N, 1] from ONNX Runtime
                // The logs show shapes like [12800, 1], [3200, 1], [800, 1] for scores
                if shape.len() == 2 && shape[1] == 1 {
                    let mut local_max = 0.0;
                    for &score in data.iter() {
                        if score > local_max {
                            local_max = score;
                        }
                        if score > best_score {
                            best_score = score;
                        }
                    }
                    info!("[Detection] Output '{}' (shape {:?}) max score: {}", name, shape, local_max);
                }
                // Handle standard 4D shapes [1, num_anchors, h, w] just in case
                else if shape.len() == 4 && (shape[2] == 80 || shape[2] == 40 || shape[2] == 20) {
                    let num_anchors = shape[1]; 
                    if num_anchors <= 2 {
                        let mut local_max = 0.0;
                        for &score in data.iter() {
                            if score > local_max {
                                local_max = score;
                            }
                            if score > best_score {
                                best_score = score;
                            }
                        }
                        info!("[Detection] Output '{}' (shape {:?}) max score: {}", name, shape, local_max);
                    }
                }
            } else {
                warn!("[Detection] Output '{}': failed to extract tensor", name);
            }
        }

        info!("[Detection] Best detection score: {}", best_score);

        // Threshold from config
        if best_score > self.threshold {
            info!("[Detection] Face detected! Returning center crop.");
            // Since we aren't decoding the bbox perfectly yet, we return a center crop.
            // This assumes the user is roughly in front of the camera.
            let (w, h) = image.dimensions();
            // Take a central square crop
            let size = std::cmp::min(w, h);
            let x = (w - size) / 2;
            let y = (h - size) / 2;
            
            let cropped = image::imageops::crop_imm(image, x, y, size, size).to_image();
            // Resize to 112x112 which is standard for ArcFace, though the engine might do it too.
            // Let's return the high-res crop so the engine can resize as needed.
            return Ok(Some(cropped));
        }

        Ok(None)
    }
}
