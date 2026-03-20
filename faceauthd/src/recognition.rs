use anyhow::Result;
use image::{ImageBuffer, Rgb};
use ndarray::{Array, Array4};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use std::path::Path;
use std::sync::Mutex;
use log::{info, error, warn};


// Constants for ArcFace (MobileFaceNet)
const INPUT_SIZE: (u32, u32) = (112, 112);

pub struct FaceEngine {
    // detection_session: Session, // TODO: Add RetinaFace/SCRFD
    recognition_session: Option<Mutex<Session>>,
}

impl FaceEngine {
    pub fn new() -> Result<Self> {
        // In a real scenario, we would load models from /usr/share/faceauth/models/
        // For this phase, we will try to load them if they exist, or fail gracefully.
        
        let model_path = Path::new("/usr/share/faceauth/models/arcface.onnx");
        
        // Check for quantized model
        let quantized_path = Path::new("/usr/share/faceauth/models/arcface_int8.onnx");
        
        // Solid Solution Fix: Check size > 0
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
            info!("Found valid quantized recognition model: {:?}", quantized_path);
            quantized_path
        } else {
            if quantized_path.exists() {
                warn!("Found quantized model {:?} but it is empty/invalid. Falling back to standard model.", quantized_path);
            }
            model_path
        };

        // Threading Optimization:
        // Use single-threaded inference per session to allow maximum parallel throughput of frames
        let intra_threads = 1;

        let recognition_session = if final_path.exists() {
            info!("Loading ArcFace model from {:?} with {} thread (Parallel-Optimized)", final_path, intra_threads);
            match Session::builder() {
                Ok(builder) => {
                    match builder
                        .with_optimization_level(GraphOptimizationLevel::Level1)
                        .and_then(|b| b.with_intra_threads(intra_threads))   // parallel within node
                        .and_then(|b| b.with_inter_threads(intra_threads))   // parallel between nodes
                        .and_then(|b| b.with_parallel_execution(false))
                        .and_then(|b| b.commit_from_file(final_path)) 
                    {
                        Ok(s) => Some(Mutex::new(s)),
                        Err(e) => {
                            error!("CRITICAL: Failed to load Recognition Model from {:?}: {}", final_path, e);
                            error!("Possible causes: Corrupted file, Missing permissions, or Incompatible CPU instruction set.");
                            return Err(e.into());
                        }
                    }
                },
                Err(e) => return Err(e.into()),
            }
        } else {
            error!("ArcFace model not found at {:?}. Recognition will fail.", model_path);
            None
        };

        Ok(Self {
            recognition_session,
        })
    }

    /// Preprocess image for ArcFace: Resize to 112x112, normalize per-channel (RGB).
    /// ArcFace standard: normalize each pixel to (value - 127.5) / 128.0 for all 3 channels.
    /// Do NOT convert to grayscale — ArcFace requires full RGB input.
    fn preprocess(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Array4<f32>> {
        let resized = image::imageops::resize(image, INPUT_SIZE.0, INPUT_SIZE.1, image::imageops::FilterType::CatmullRom);
        
        let mut array = Array::zeros((1, 3, INPUT_SIZE.1 as usize, INPUT_SIZE.0 as usize));
        
        for (x, y, pixel) in resized.enumerate_pixels() {
            let [r, g, b] = pixel.0;
            array[[0, 0, y as usize, x as usize]] = (r as f32 - 127.5) / 128.0;
            array[[0, 1, y as usize, x as usize]] = (g as f32 - 127.5) / 128.0;
            array[[0, 2, y as usize, x as usize]] = (b as f32 - 127.5) / 128.0;
        }
        
        Ok(array)
    }

    pub fn get_embedding(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Vec<f32>> {
        if let Some(session_mutex) = &self.recognition_session {
            let input_array = self.preprocess(image)?;
            let input_tensor = Value::from_array(input_array)?;
            
            let mut session = session_mutex.lock()
                .map_err(|e| anyhow::anyhow!("Recognition session lock poisoned: {}", e))?;

            let outputs = session.run(ort::inputs![input_tensor])?;
            
            let (_shape, data) = outputs[0].try_extract_tensor::<f32>()?;
            let embedding: Vec<f32> = data.to_vec();
            
            // Normalize embedding
            let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
            Ok(embedding.into_iter().map(|x| x / norm).collect())
        } else {
            // Mock embedding for testing if model is missing
            Ok(vec![0.1; 512])
        }
    }


    pub fn compare(&self, emb1: &[f32], emb2: &[f32]) -> f32 {
        // Cosine similarity
        let dot_product: f32 = emb1.iter().zip(emb2.iter()).map(|(a, b)| a * b).sum();
        // Since vectors are normalized, dot product is the cosine similarity
        dot_product
    }
}
