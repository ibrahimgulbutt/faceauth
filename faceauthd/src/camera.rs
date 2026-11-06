//! Camera selection, format negotiation and capture.
//!
//! `camera.device = "auto"` uses the first V4L2 device that offers a colour
//! format; a path selects a device explicitly (that is how IR cameras are
//! used). Greyscale (IR) frames are expanded to RGB so the rest of the
//! pipeline is format-agnostic.

use anyhow::{bail, Context, Result};
use faceauth_core::CameraDevice;
use image::{GrayImage, RgbImage};
use log::{info, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

const ALL_FORMATS: [FrameFormat; 6] = [
    FrameFormat::MJPEG,
    FrameFormat::YUYV,
    FrameFormat::NV12,
    FrameFormat::GRAY,
    FrameFormat::RAWRGB,
    FrameFormat::RAWBGR,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    Auto,
    Index(u32),
}

pub struct CameraManager {
    selection: Selection,
    /// Device index and format chosen on the first successful open, reused
    /// until opening fails (hot-plug, renumbering).
    resolved: Mutex<Option<(u32, CameraFormat)>>,
    /// Wall-clock time of the last close. SystemTime rather than Instant
    /// because CLOCK_MONOTONIC does not advance during suspend, and a long
    /// sleep is exactly when the sensor needs a longer warm-up.
    last_closed: Mutex<Option<SystemTime>>,
}

impl CameraManager {
    pub fn new(device: &str) -> Result<Self> {
        let selection = if device.trim().eq_ignore_ascii_case("auto") {
            Selection::Auto
        } else {
            Selection::Index(index_from_path(Path::new(device.trim()))?)
        };
        Ok(Self { selection, resolved: Mutex::new(None), last_closed: Mutex::new(None) })
    }

    /// Index of the device this manager would use right now, if any.
    pub fn selected_index(&self) -> Option<u32> {
        match self.selection {
            Selection::Index(i) => Some(i),
            Selection::Auto => {
                if let Some((i, _)) = *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) {
                    return Some(i);
                }
                list_devices().into_iter().find(|d| d.colour).map(|d| d.index)
            }
        }
    }

    /// "Integrated Camera (/dev/video0)"
    pub fn describe(&self) -> Option<String> {
        let index = self.selected_index()?;
        Some(format!("{} (/dev/video{index})", device_name(index)))
    }

    /// Open a capture session. The settle delay and warm-up frame count scale
    /// with how long the camera has been idle:
    ///
    /// | Idle          | Settle | Warm-up frames |
    /// |---------------|--------|----------------|
    /// | < 30 s        | 0 ms   | 2              |
    /// | 30 s – 5 min  | 100 ms | 4              |
    /// | > 5 min       | 300 ms | configured     |
    pub fn start_session(&self, configured_warmup: usize) -> Result<ActiveCamera<'_>> {
        let idle_secs = self
            .last_closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        let (settle_ms, warmup) = match idle_secs {
            s if s < 30 => (0, 2),
            s if s < 300 => (100, 4),
            _ => (300, configured_warmup),
        };

        let mut camera = match self.open() {
            Ok(c) => c,
            Err(e) => {
                // The device may have been renumbered or briefly busy: forget
                // the cached choice and try once more from scratch.
                warn!("Camera open failed ({e:#}); retrying");
                *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) = None;
                std::thread::sleep(Duration::from_millis(300));
                self.open()?
            }
        };
        camera.open_stream().context("Failed to start the camera stream")?;
        let fmt = camera.camera_format();
        info!(
            "Camera streaming {}x{} {:?} @{}fps (idle {}, settle {settle_ms} ms, warm-up {warmup} frames)",
            fmt.width(),
            fmt.height(),
            fmt.format(),
            fmt.frame_rate(),
            if idle_secs == u64::MAX { "∞".to_string() } else { format!("{idle_secs}s") },
        );
        if settle_ms > 0 {
            std::thread::sleep(Duration::from_millis(settle_ms));
        }
        let mut active = ActiveCamera { camera, manager: self };
        for _ in 0..warmup {
            let _ = active.camera.frame();
        }
        Ok(active)
    }

    fn open(&self) -> Result<Camera> {
        let cached = *self.resolved.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((index, fmt)) = cached {
            return open_with_format(index, fmt);
        }
        let candidates: Vec<u32> = match self.selection {
            Selection::Index(i) => vec![i],
            Selection::Auto => list_devices().into_iter().filter(|d| d.colour).map(|d| d.index).collect(),
        };
        if candidates.is_empty() {
            bail!("No camera found");
        }
        let mut last_err = None;
        for index in candidates {
            match probe(index).and_then(|formats| {
                let fmt = choose_format(&formats).context("Camera offers no usable format")?;
                Ok((open_with_format(index, fmt)?, fmt))
            }) {
                Ok((camera, fmt)) => {
                    *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) = Some((index, fmt));
                    return Ok(camera);
                }
                Err(e) => last_err = Some(e.context(format!("/dev/video{index}"))),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No usable camera")))
    }
}

pub struct ActiveCamera<'a> {
    camera: Camera,
    manager: &'a CameraManager,
}

impl ActiveCamera<'_> {
    pub fn capture_frame(&mut self) -> Result<RgbImage> {
        let frame = self.camera.frame().context("Failed to read a camera frame")?;
        if frame.source_frame_format() == FrameFormat::GRAY {
            let res = frame.resolution();
            let (w, h) = (res.width(), res.height());
            let data = frame.buffer();
            let needed = (w * h) as usize;
            if data.len() < needed {
                bail!("Short greyscale frame ({} of {needed} bytes)", data.len());
            }
            let grey = GrayImage::from_raw(w, h, data[..needed].to_vec()).context("Bad greyscale frame")?;
            return Ok(image::DynamicImage::ImageLuma8(grey).to_rgb8());
        }
        Ok(frame.decode_image::<RgbFormat>()?)
    }

    /// Capture `count` frames, `interval_ms` apart. Individual failed frames
    /// are skipped; at least one frame must succeed.
    pub fn capture_sequence(&mut self, count: usize, interval_ms: u64) -> Result<Vec<RgbImage>> {
        let mut frames = Vec::with_capacity(count);
        for i in 0..count {
            if let Ok(frame) = self.capture_frame() {
                frames.push(frame);
            }
            if interval_ms > 0 && i + 1 < count {
                std::thread::sleep(Duration::from_millis(interval_ms));
            }
        }
        if frames.is_empty() && count > 0 {
            bail!("Failed to capture any frames");
        }
        Ok(frames)
    }
}

impl Drop for ActiveCamera<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.camera.stop_stream() {
            warn!("Failed to stop camera stream: {e}");
        }
        *self.manager.last_closed.lock().unwrap_or_else(|p| p.into_inner()) = Some(SystemTime::now());
    }
}

/// Wait up to `timeout` for exclusive use of the camera.
pub fn lock_camera(camera: &Mutex<CameraManager>, timeout: Duration) -> Option<std::sync::MutexGuard<'_, CameraManager>> {
    let deadline = Instant::now() + timeout;
    loop {
        match camera.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    }
}

// ─── Device discovery ───────────────────────────────────────────────────────

pub struct Device {
    pub index: u32,
    pub name: String,
    pub formats: Vec<FrameFormat>,
    pub colour: bool,
    pub infrared: bool,
}

/// All V4L2 capture devices, in index order. Metadata-only nodes are skipped.
pub fn list_devices() -> Vec<Device> {
    let mut indices: Vec<u32> = std::fs::read_dir("/sys/class/video4linux")
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str()?.strip_prefix("video")?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    indices.sort_unstable();

    indices
        .into_iter()
        .filter_map(|index| {
            let formats: Vec<FrameFormat> = probe(index).ok()?.iter().map(|f| f.format()).collect();
            if formats.is_empty() {
                return None;
            }
            let mut unique = formats.clone();
            unique.sort_by_key(|f| format!("{f:?}"));
            unique.dedup();
            let name = device_name(index);
            let colour = unique.iter().any(|f| matches!(f, FrameFormat::MJPEG | FrameFormat::YUYV | FrameFormat::NV12 | FrameFormat::RAWRGB | FrameFormat::RAWBGR));
            let named_ir = name.split(|c: char| !c.is_alphanumeric()).any(|w| w.eq_ignore_ascii_case("ir"))
                || name.to_ascii_lowercase().contains("infrared");
            Some(Device { index, name, colour, infrared: named_ir || !colour, formats: unique })
        })
        .collect()
}

pub fn describe_devices(selected: Option<u32>) -> Vec<CameraDevice> {
    list_devices()
        .into_iter()
        .map(|d| CameraDevice {
            path: format!("/dev/video{}", d.index),
            name: d.name,
            formats: d.formats.iter().map(|f| format!("{f:?}")).collect(),
            infrared: d.infrared,
            selected: Some(d.index) == selected,
        })
        .collect()
}

fn device_name(index: u32) -> String {
    std::fs::read_to_string(format!("/sys/class/video4linux/video{index}/name"))
        .map(|s| clean_name(s.trim()))
        .unwrap_or_else(|_| format!("Camera {index}"))
}

/// UVC names are "<product>: <product>" cut to 31 bytes by V4L2, e.g.
/// "Integrated RGB Camera: Integrat". Keep the first half when the second is
/// just a truncated copy of it.
fn clean_name(raw: &str) -> String {
    match raw.split_once(": ") {
        Some((head, tail)) if !tail.is_empty() && head.starts_with(tail) => head.to_string(),
        _ => raw.to_string(),
    }
}

/// `/dev/video2` or a symlink to it (such as `/dev/v4l/by-id/...`) → 2.
fn index_from_path(path: &Path) -> Result<u32> {
    let real = std::fs::canonicalize(path).with_context(|| format!("camera.device {} does not exist", path.display()))?;
    real.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|n| n.parse().ok())
        .with_context(|| format!("camera.device {} is not a /dev/videoN device", path.display()))
}

fn probe(index: u32) -> Result<Vec<CameraFormat>> {
    let request = RequestedFormat::with_formats(RequestedFormatType::None, &ALL_FORMATS);
    let mut camera = Camera::new(CameraIndex::Index(index), request)?;
    Ok(camera.compatible_camera_formats()?)
}

fn open_with_format(index: u32, fmt: CameraFormat) -> Result<Camera> {
    let wanted = [fmt.format()];
    let request = RequestedFormat::with_formats(RequestedFormatType::Exact(fmt), &wanted);
    Ok(Camera::new(CameraIndex::Index(index), request)?)
}

/// MJPEG near 1280×720 → YUYV near 640×480 → NV12 → greyscale (IR) near
/// 640×480 → whatever is left. Formats below 15 fps are used only as a last
/// resort.
fn choose_format(formats: &[CameraFormat]) -> Option<CameraFormat> {
    let pick = |ff: FrameFormat, target: (i64, i64), min_fps: u32| {
        formats
            .iter()
            .filter(|f| f.format() == ff && f.frame_rate() >= min_fps)
            .min_by_key(|f| {
                let dx = f.width() as i64 - target.0;
                let dy = f.height() as i64 - target.1;
                (dx * dx + dy * dy, std::cmp::Reverse(f.frame_rate()))
            })
            .copied()
    };
    let preferences = [
        (FrameFormat::MJPEG, (1280, 720)),
        (FrameFormat::YUYV, (640, 480)),
        (FrameFormat::NV12, (1280, 720)),
        (FrameFormat::GRAY, (640, 480)),
        (FrameFormat::RAWRGB, (640, 480)),
        (FrameFormat::RAWBGR, (640, 480)),
    ];
    for min_fps in [15, 1] {
        for (ff, target) in preferences {
            if let Some(f) = pick(ff, target, min_fps) {
                return Some(f);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use nokhwa::utils::Resolution;

    fn fmt(ff: FrameFormat, w: u32, h: u32, fps: u32) -> CameraFormat {
        CameraFormat::new(Resolution::new(w, h), ff, fps)
    }

    #[test]
    fn cleans_truncated_uvc_names() {
        assert_eq!(clean_name("Integrated RGB Camera: Integrat"), "Integrated RGB Camera");
        assert_eq!(clean_name("HD Webcam: HD Webcam"), "HD Webcam");
        assert_eq!(clean_name("Logitech BRIO"), "Logitech BRIO");
        assert_eq!(clean_name("Camera: IR"), "Camera: IR");
    }

    #[test]
    fn prefers_mjpeg_720p() {
        let formats = [
            fmt(FrameFormat::YUYV, 640, 480, 30),
            fmt(FrameFormat::MJPEG, 1920, 1080, 30),
            fmt(FrameFormat::MJPEG, 1280, 720, 30),
        ];
        assert_eq!(choose_format(&formats), Some(fmt(FrameFormat::MJPEG, 1280, 720, 30)));
    }

    #[test]
    fn uses_greyscale_for_ir_only_devices() {
        let formats = [fmt(FrameFormat::GRAY, 640, 360, 30), fmt(FrameFormat::GRAY, 340, 340, 15)];
        assert_eq!(choose_format(&formats), Some(fmt(FrameFormat::GRAY, 640, 360, 30)));
    }

    #[test]
    fn slow_formats_are_last_resort() {
        let formats = [fmt(FrameFormat::MJPEG, 1280, 720, 5), fmt(FrameFormat::YUYV, 640, 480, 30)];
        assert_eq!(choose_format(&formats), Some(fmt(FrameFormat::YUYV, 640, 480, 30)));
    }
}
