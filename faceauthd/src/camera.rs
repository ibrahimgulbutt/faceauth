use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use anyhow::{Result, Context};
use log::info;

pub struct CameraManager {
    index: CameraIndex,
    requested: RequestedFormat<'static>,
}

impl CameraManager {
    pub fn new() -> Self {
        Self { 
            index: CameraIndex::Index(0),
            // Request 1280x720 for faster processing (down from 1080p)
            requested: RequestedFormat::new::<RgbFormat>(
                RequestedFormatType::Closest(
                    CameraFormat::new(
                        Resolution::new(1280, 720),
                        FrameFormat::MJPEG,
                        30
                    )
                )
            )
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        // Don't open the camera at startup — that would flash the LED immediately on boot
        // with no benefit.  start_session() will catch any real camera error on first use.
        info!("Camera manager initialized (camera opened on-demand)");
        Ok(())
    }



    pub fn start_session(&self) -> Result<ActiveCamera> {
        info!("Opening camera stream...");
        let mut cam = Camera::new(self.index.clone(), self.requested.clone())
            .context("Failed to create camera instance")?;
        cam.open_stream().context("Failed to open camera stream")?;
        info!("Camera stream opened successfully.");
        Ok(ActiveCamera { camera: cam })
    }
}

pub struct ActiveCamera {
    camera: Camera,
}

impl ActiveCamera {
    pub fn warmup(&mut self, frames: usize) -> Result<()> {
        for _ in 0..frames {
            // Just capturing the frame is enough to clear the buffer.
            // No need to sleep; the capture itself waits for the frame interval (e.g. 33ms).
            let _ = self.camera.frame();
        }
        Ok(())
    }

    pub fn capture_frame(&mut self) -> Result<image::ImageBuffer<image::Rgb<u8>, Vec<u8>>> {
        let frame = self.camera.frame()?;
        let decoded = frame.decode_image::<RgbFormat>()?;
        Ok(decoded)
    }

    pub fn capture_sequence(&mut self, count: usize, interval_ms: u64) -> Result<Vec<image::ImageBuffer<image::Rgb<u8>, Vec<u8>>>> {
        let mut frames = Vec::new();
        for _ in 0..count {
            if let Ok(frame) = self.capture_frame() {
                frames.push(frame);
            }
            if interval_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(interval_ms));
            }
        }
        if frames.is_empty() {
            anyhow::bail!("Failed to capture any frames");
        }
        Ok(frames)
    }
}

impl Drop for ActiveCamera {
    fn drop(&mut self) {
        info!("Stopping camera stream...");
        if let Err(e) = self.camera.stop_stream() {
            log::warn!("Failed to stop camera stream gracefully: {}", e);
        } else {
            info!("Camera stream stopped.");
        }
    }
}
