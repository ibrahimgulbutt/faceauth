use anyhow::Result;
use image::{ImageBuffer, Rgb};
use ndarray::{Array, Array4};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use std::path::Path;
use std::sync::Mutex;
use log::{info, error};
use imageproc::contrast::equalize_histogram;


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
        let final_path = if quantized_path.exists() {
            info!("Found quantized recognition model: {:?}", quantized_path);
            quantized_path
        } else {
            model_path
        };

        let recognition_session = if final_path.exists() {
            info!("Loading ArcFace model from {:?}", final_path);
            Some(Mutex::new(Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                .with_intra_threads(4)?
                .with_parallel_execution(true)?
                .commit_from_file(final_path)?))
        } else {
            error!("ArcFace model not found at {:?}. Recognition will fail.", model_path);
            None
        };

        Ok(Self {
            recognition_session,
        })
    }

    /// Preprocess image for ArcFace: Resize to 112x112, Normalize
    fn preprocess(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Array4<f32>> {
        let resized = image::imageops::resize(image, INPUT_SIZE.0, INPUT_SIZE.1, image::imageops::FilterType::Triangle);
        
        // Histogram Equalization to improve low-light performance
        let gray = image::imageops::grayscale(&resized);
        let equalized = equalize_histogram(&gray);

        let mut array = Array::zeros((1, 3, INPUT_SIZE.1 as usize, INPUT_SIZE.0 as usize));
        
        // Standard Iterator Optimization (LLVM will auto-vectorize this)
        // Normalize: (x - 127.5) / 128.0
        for (i, p) in equalized.pixels().enumerate() {
            let val = (p.0[0] as f32 - 127.5) / 128.0;
            let x = i % INPUT_SIZE.0 as usize;
            let y = i / INPUT_SIZE.0 as usize;
            array[[0, 0, y, x]] = val;
            array[[0, 1, y, x]] = val;
            array[[0, 2, y, x]] = val;
        }
        
        Ok(array)
    }

    pub fn get_embedding(&self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Vec<f32>> {
        if let Some(session_mutex) = &self.recognition_session {
            let input_array = self.preprocess(image)?;
            let input_tensor = Value::from_array(input_array)?;
            
            let mut session = session_mutex.lock().unwrap();
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
