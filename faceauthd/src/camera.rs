use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use anyhow::{Result, Context};
use log::info;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub struct CameraManager {
    index: CameraIndex,
    requested: RequestedFormat<'static>,
    /// Wall-clock timestamp of the last camera close.
    /// We use SystemTime (CLOCK_REALTIME) because std::time::Instant uses
    /// CLOCK_MONOTONIC on Linux, which does NOT advance during suspend.
    /// SystemTime advances through a suspend/resume cycle, so idle_secs
    /// correctly reflects a 1-hour sleep as ~3600 seconds.
    last_closed: Arc<Mutex<Option<SystemTime>>>,
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
            ),
            last_closed: Arc::new(Mutex::new(None)),
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        // Don't open the camera at startup — that would flash the LED immediately on boot
        // with no benefit.  start_session() will catch any real camera error on first use.
        info!("Camera manager initialized (camera opened on-demand)");
        Ok(())
    }



    /// Open a camera session with adaptive settle delay and warmup.
    ///
    /// Settle delay and warmup frame count scale with how long the camera has
    /// been idle, using wall-clock time so suspend/resume is handled correctly:
    ///
    /// | Idle time        | Settle  | Warmup frames | Typical total  |
    /// |------------------|---------|---------------|----------------|
    /// | < 30 s (warm)    | 0 ms    | 2             | ~270 ms        |
    /// | 30 s – 5 min     | 100 ms  | 4             | ~500 ms        |
    /// | > 5 min (cold)   | 300 ms  | configured    | ~900 ms        |
    ///
    /// The `configured_warmup` value (from config.toml) is only used in the
    /// cold path; warm/tepid paths ignore it deliberately.
    pub fn start_session(&self, configured_warmup: usize) -> Result<ActiveCamera> {
        // Compute idle seconds before opening.  SystemTime::duration_since may
        // return Err if wall-clock jumped backwards (NTP step); treat that as cold.
        let idle_secs: u64 = {
            let ts = self.last_closed.lock()
                .unwrap_or_else(|p| p.into_inner());
            ts.as_ref()
                .and_then(|t| SystemTime::now().duration_since(*t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX)  // never opened → cold start
        };

        let (settle_ms, warmup_frames) = if idle_secs < 30 {
            // Camera closed very recently: buffers are fresh, AEC converged.
            (0u64, 2usize)
        } else if idle_secs < 300 {
            // 30 s – 5 min: brief cooldown, light warmup.
            (100u64, 4usize)
        } else {
            // > 5 min or first open: full cold-start path.
            (300u64, configured_warmup)
        };

        info!("Opening camera stream (idle={}s → settle={}ms, warmup={}f)",
              if idle_secs == u64::MAX { "∞".to_string() } else { idle_secs.to_string() },
              settle_ms, warmup_frames);

        let mut cam = Camera::new(self.index.clone(), self.requested.clone())
            .context("Failed to create camera instance")?;
        cam.open_stream().context("Failed to open camera stream")?;

        if settle_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(settle_ms));
        }

        let mut active = ActiveCamera {
            camera: cam,
            last_closed: self.last_closed.clone(),
        };
        active.warmup(warmup_frames)?;

        info!("Camera ready.");
        Ok(active)
    }
}

pub struct ActiveCamera {
    camera: Camera,
    last_closed: Arc<Mutex<Option<SystemTime>>>,
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
        // Record the close time so the next start_session() can compute idle time.
        if let Ok(mut ts) = self.last_closed.lock() {
            *ts = Some(SystemTime::now());
        }
    }
}
