use anyhow::Result;
use image::{ImageBuffer, Rgb, GenericImageView};
use ndarray::{Array, Array4, Axis, s};
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Value;
use std::path::Path;

pub struct FaceDetector {
    session: Session,
}

impl FaceDetector {
    pub fn new(model_path: &Path) -> Result<Self> {
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(4)?
            .commit_from_file(model_path)?;
        
        Ok(Self { session })
    }

    pub fn detect(&mut self, image: &ImageBuffer<Rgb<u8>, Vec<u8>>) -> Result<Option<(u32, u32, u32, u32)>> {
        // Preprocess: Resize to 640x640 (standard for many SCRFD models) or keep original if dynamic
        // For simplicity, let's resize to 640x640 and pad
        let (_width, _height) = image.dimensions();
        let target_size = (640, 640);
        
        let resized = image::imageops::resize(image, target_size.0, target_size.1, image::imageops::FilterType::Triangle);
        
        let mut input = Array::zeros((1, 3, 640, 640));
        for (x, y, pixel) in resized.enumerate_pixels() {
            let [r, g, b] = pixel.0;
            input[[0, 0, y as usize, x as usize]] = r as f32;
            input[[0, 1, y as usize, x as usize]] = g as f32;
            input[[0, 2, y as usize, x as usize]] = b as f32;
        }
        
        input.mapv_inplace(|x| (x - 127.5) / 128.0);

        let input_tensor = Value::from_array(input)?;
        let _outputs = self.session.run(ort::inputs![input_tensor])?;
        
        Ok(Some((160, 120, 480, 360))) // Mock: Center box
    }
}
