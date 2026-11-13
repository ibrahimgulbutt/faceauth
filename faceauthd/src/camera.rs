//! Camera selection, format negotiation and capture.
//!
//! `camera.device = "auto"` uses the first V4L2 device that offers a colour
//! format, `"ir"` the first infrared one, and a path selects a device
//! explicitly. Greyscale (IR) frames are expanded to RGB so the rest of the
//! pipeline is format-agnostic.
//!
//! Devices are probed with the `v4l` crate. Colour streams go through nokhwa;
//! greyscale streams are read with `v4l` directly, because nokhwa 0.10 asks
//! V4L2 for the FourCC "GRAY" instead of "GREY" and so can't open IR cameras.
//!
//! Infrared cameras need their IR light (emitter) on to see in the dark. Many
//! switch it on by themselves; others need a vendor command, which
//! `camera.ir_emitter` sends every time the stream starts. If the emitter
//! flashes on every other frame, the dark frames are skipped.

use crate::config::IrEmitter;
use anyhow::{bail, Context, Result};
use faceauth_core::CameraDevice;
use image::{GrayImage, RgbImage};
use log::{debug, info, warn};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// First colour camera.
    Auto,
    /// First infrared camera.
    Infrared,
    Index(u32),
}

/// Mean brightness below which the exposure is given extra frames to settle.
const DARK_FRAME: f32 = 70.0;
/// At most this many extra frames while the exposure settles.
const MAX_EXPOSURE_FRAMES: usize = 15;

pub struct CameraManager {
    selection: Selection,
    /// Device index and format chosen on the first successful open, reused
    /// until opening fails (hot-plug, renumbering).
    resolved: Mutex<Option<(u32, CameraFormat)>>,
    /// Wall-clock time of the last close. SystemTime rather than Instant
    /// because CLOCK_MONOTONIC does not advance during suspend, and a long
    /// sleep is exactly when the sensor needs a longer warm-up.
    last_closed: Mutex<Option<SystemTime>>,
    emitter: Vec<IrEmitter>,
}

impl CameraManager {
    pub fn new(device: &str, emitter: Vec<IrEmitter>) -> Result<Self> {
        let device = device.trim();
        let selection = if device.eq_ignore_ascii_case("auto") {
            Selection::Auto
        } else if device.eq_ignore_ascii_case("ir") || device.eq_ignore_ascii_case("infrared") {
            Selection::Infrared
        } else {
            Selection::Index(index_from_path(Path::new(device))?)
        };
        Ok(Self { selection, resolved: Mutex::new(None), last_closed: Mutex::new(None), emitter })
    }

    /// Whether the camera in use is infrared. Profiles made with an IR camera
    /// can't be compared with colour images and vice versa.
    pub fn is_infrared(&self) -> bool {
        match self.selection {
            Selection::Infrared => true,
            _ => self
                .selected_index()
                .and_then(|i| list_devices().into_iter().find(|d| d.index == i))
                .is_some_and(|d| d.infrared),
        }
    }

    /// Index of the device this manager would use right now, if any.
    pub fn selected_index(&self) -> Option<u32> {
        match self.selection {
            Selection::Index(i) => Some(i),
            Selection::Auto | Selection::Infrared => {
                if let Some((i, _)) = *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) {
                    return Some(i);
                }
                self.candidates().first().copied()
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

        let mut backend = match self.open() {
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
        let fmt = backend.start()?;
        info!(
            "Camera streaming {}x{} {:?} @{}fps (idle {}, settle {settle_ms} ms, warm-up {warmup} frames)",
            fmt.width(),
            fmt.height(),
            fmt.format(),
            fmt.frame_rate(),
            if idle_secs == u64::MAX { "∞".to_string() } else { format!("{idle_secs}s") },
        );
        // Vendor commands are only ever sent to an infrared stream: on a combined
        // module the colour half may expose the same units with other meanings.
        let infrared = fmt.format() == FrameFormat::GRAY;
        if !infrared && !self.emitter.is_empty() {
            debug!("Not sending camera.ir_emitter: the camera in use is not infrared");
        }
        if let (true, Some((index, _))) = (infrared, *self.resolved.lock().unwrap_or_else(|p| p.into_inner())) {
            for emitter in &self.emitter {
                if let Err(e) = enable_emitter(index, emitter) {
                    warn!("Could not switch on the IR emitter: {e:#}");
                }
            }
        }
        if settle_ms > 0 {
            std::thread::sleep(Duration::from_millis(settle_ms));
        }
        let mut active = ActiveCamera { backend, manager: self, strobe: None };
        for _ in 0..warmup {
            active.backend.skip_frame();
        }
        active.settle(infrared);
        Ok(active)
    }

    fn candidates(&self) -> Vec<u32> {
        match self.selection {
            Selection::Index(i) => vec![i],
            Selection::Auto => list_devices().into_iter().filter(|d| d.colour).map(|d| d.index).collect(),
            Selection::Infrared => list_devices().into_iter().filter(|d| d.infrared).map(|d| d.index).collect(),
        }
    }

    fn open(&self) -> Result<Backend> {
        let cached = *self.resolved.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((index, fmt)) = cached {
            return Backend::open(index, fmt);
        }
        let candidates = self.candidates();
        if candidates.is_empty() {
            bail!(if self.selection == Selection::Infrared { "No infrared camera found" } else { "No camera found" });
        }
        let mut last_err = None;
        for index in candidates {
            match probe(index).and_then(|formats| {
                let fmt = choose_format(&formats).context("Camera offers no usable format")?;
                Ok((Backend::open(index, fmt)?, fmt))
            }) {
                Ok((backend, fmt)) => {
                    *self.resolved.lock().unwrap_or_else(|p| p.into_inner()) = Some((index, fmt));
                    return Ok(backend);
                }
                Err(e) => last_err = Some(e.context(format!("/dev/video{index}"))),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No usable camera")))
    }
}

/// An opened camera: nokhwa for colour formats, `v4l` for greyscale.
enum Backend {
    Nokhwa(Box<Camera>),
    Grey(GreyCamera),
}

struct GreyCamera {
    device: v4l::Device,
    format: CameraFormat,
    stream: Option<GreyStream>,
}

struct GreyStream {
    stream: v4l::io::mmap::Stream<'static>,
    width: u32,
    height: u32,
    stride: u32,
}

impl Backend {
    fn open(index: u32, fmt: CameraFormat) -> Result<Self> {
        if fmt.format() == FrameFormat::GRAY {
            let device = v4l::Device::new(index as usize).with_context(|| format!("opening /dev/video{index}"))?;
            return Ok(Backend::Grey(GreyCamera { device, format: fmt, stream: None }));
        }
        let wanted = [fmt.format()];
        let request = RequestedFormat::with_formats(RequestedFormatType::Exact(fmt), &wanted);
        Ok(Backend::Nokhwa(Box::new(Camera::new(CameraIndex::Index(index), request)?)))
    }

    /// Start streaming; returns the format actually in use.
    fn start(&mut self) -> Result<CameraFormat> {
        match self {
            Backend::Nokhwa(camera) => {
                camera.open_stream().context("Failed to start the camera stream")?;
                Ok(camera.camera_format())
            }
            Backend::Grey(cam) => {
                use v4l::video::Capture;
                let want = v4l::Format::new(cam.format.width(), cam.format.height(), v4l::FourCC::new(b"GREY"));
                let got = cam.device.set_format(&want).context("Failed to set the greyscale format")?;
                if &got.fourcc.repr != b"GREY" {
                    bail!("Camera refused the greyscale format (got {})", got.fourcc);
                }
                if let Err(e) = cam.device.set_params(&v4l::video::capture::Parameters::with_fps(cam.format.frame_rate())) {
                    warn!("Could not set {} fps: {e}", cam.format.frame_rate());
                }
                let mut stream = v4l::io::mmap::Stream::with_buffers(&cam.device, v4l::buffer::Type::VideoCapture, 4)
                    .context("Failed to start the camera stream")?;
                stream.set_timeout(Duration::from_secs(2));
                let stride = got.stride.max(got.width);
                cam.stream = Some(GreyStream { stream, width: got.width, height: got.height, stride });
                Ok(CameraFormat::new(Resolution::new(got.width, got.height), FrameFormat::GRAY, cam.format.frame_rate()))
            }
        }
    }

    fn skip_frame(&mut self) {
        match self {
            Backend::Nokhwa(camera) => {
                let _ = camera.frame();
            }
            Backend::Grey(_) => {
                let _ = self.read();
            }
        }
    }

    fn read(&mut self) -> Result<RgbImage> {
        match self {
            Backend::Nokhwa(camera) => {
                let frame = camera.frame().context("Failed to read a camera frame")?;
                Ok(frame.decode_image::<RgbFormat>()?)
            }
            Backend::Grey(cam) => {
                use v4l::io::traits::CaptureStream;
                let s = cam.stream.as_mut().context("Camera stream not started")?;
                let (w, h, stride) = (s.width, s.height, s.stride);
                let (data, _) = s.stream.next().context("Failed to read a camera frame")?;
                let needed = (stride * (h - 1) + w) as usize;
                if data.len() < needed {
                    bail!("Short greyscale frame ({} of {needed} bytes)", data.len());
                }
                let mut grey = Vec::with_capacity((w * h) as usize);
                for row in data.chunks(stride as usize).take(h as usize) {
                    grey.extend_from_slice(&row[..w as usize]);
                }
                let grey = GrayImage::from_raw(w, h, grey).context("Bad greyscale frame")?;
                Ok(image::DynamicImage::ImageLuma8(grey).to_rgb8())
            }
        }
    }

    fn stop(&mut self) {
        match self {
            Backend::Nokhwa(camera) => {
                if let Err(e) = camera.stop_stream() {
                    warn!("Failed to stop camera stream: {e}");
                }
            }
            // Dropping the mmap stream stops it and frees the buffers.
            Backend::Grey(cam) => cam.stream = None,
        }
    }
}

pub struct ActiveCamera<'a> {
    backend: Backend,
    manager: &'a CameraManager,
    /// Brightness of a lit frame, when the IR emitter flashes on every other
    /// frame; dark frames are then skipped.
    strobe: Option<f32>,
}

impl ActiveCamera<'_> {
    /// After the warm-up: give the auto-exposure more frames while the picture
    /// is still dark and getting brighter, and detect a flashing IR emitter.
    fn settle(&mut self, infrared: bool) {
        let Ok(first) = self.read_frame() else { return };
        let mut levels = vec![mean_brightness(&first)];
        let needed = if infrared { 6 } else { 1 };
        while levels.len() < MAX_EXPOSURE_FRAMES {
            // Pairs of frames, so a flashing IR emitter doesn't look like change.
            let n = levels.len();
            let recent = if n >= 2 { levels[n - 1].max(levels[n - 2]) } else { levels[0] };
            let rising = n < 4 || recent > levels[n - 3].max(levels[n - 4]) + 1.0;
            let dark_and_rising = recent < DARK_FRAME && rising;
            if levels.len() >= needed && !dark_and_rising {
                break;
            }
            match self.read_frame() {
                Ok(f) => levels.push(mean_brightness(&f)),
                Err(_) => break,
            }
        }
        if infrared {
            self.strobe = strobe_level(&levels);
            if let Some(lit) = self.strobe {
                info!("IR emitter flashes every other frame (lit ≈ {lit:.0}); skipping dark frames");
            }
        }
        let last = levels[levels.len() - 1];
        if levels.len() > needed {
            info!("Exposure settled over {} frames (brightness {:.0} → {last:.0})", levels.len(), levels[0]);
        } else {
            info!("Picture brightness {last:.0}");
        }
    }

    pub fn capture_frame(&mut self) -> Result<RgbImage> {
        let Some(lit) = self.strobe else { return self.read_frame() };
        // With a flashing emitter every other frame is dark; take a lit one.
        let mut best = self.read_frame()?;
        for _ in 0..2 {
            let level = mean_brightness(&best);
            if level >= lit * 0.7 {
                self.strobe = Some(lit * 0.8 + level * 0.2);
                break;
            }
            let next = self.read_frame()?;
            if mean_brightness(&next) > level {
                best = next;
            }
        }
        Ok(best)
    }

    fn read_frame(&mut self) -> Result<RgbImage> {
        self.backend.read()
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
        self.backend.stop();
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
            let formats: Vec<FrameFormat> = match probe(index) {
                Ok(f) => f.iter().map(|f| f.format()).collect(),
                Err(e) => {
                    log::debug!("/dev/video{index}: {e:#}");
                    return None;
                }
            };
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

/// Mean brightness of a frame, sampling every 7th byte.
pub fn mean_brightness(img: &RgbImage) -> f32 {
    let raw = img.as_raw();
    if raw.is_empty() {
        return 0.0;
    }
    let (sum, n) = raw.iter().step_by(7).fold((0u64, 0u64), |(s, n), &v| (s + v as u64, n + 1));
    sum as f32 / n as f32
}

/// Brightness of the lit frames when the levels alternate bright/dark (an IR
/// emitter flashing every other frame), else None.
fn strobe_level(levels: &[f32]) -> Option<f32> {
    if levels.len() < 4 {
        return None;
    }
    let tail = &levels[levels.len() - 4..];
    let alternating = tail.windows(2).all(|w| {
        let (hi, lo) = if w[0] > w[1] { (w[0], w[1]) } else { (w[1], w[0]) };
        hi > lo * 1.5 + 5.0
    }) && tail.windows(3).all(|w| (w[0] > w[1]) == (w[2] > w[1]));
    alternating.then(|| tail.iter().cloned().fold(0.0, f32::max))
}

/// Send the vendor command that switches on an IR emitter (a UVC extension
/// unit control, found by `faceauth ir-setup` or linux-enable-ir-emitter).
fn enable_emitter(index: u32, emitter: &IrEmitter) -> Result<()> {
    let device = UvcDevice::open(index)?;
    device
        .set(emitter.unit, emitter.selector, &emitter.control)
        .with_context(|| format!("UVC control (unit {}, selector {})", emitter.unit, emitter.selector))?;
    info!("IR emitter switched on (unit {}, selector {})", emitter.unit, emitter.selector);
    Ok(())
}

// ─── UVC extension-unit controls ────────────────────────────────────────────

#[repr(C)]
struct UvcXuControlQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

// _IOWR('u', 0x21, struct uvc_xu_control_query)
const UVCIOC_CTRL_QUERY: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<UvcXuControlQuery>() as libc::c_ulong) << 16)
    | ((b'u' as libc::c_ulong) << 8)
    | 0x21;
const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const UVC_GET_MAX: u8 = 0x83;
const UVC_GET_LEN: u8 = 0x85;
const UVC_GET_INFO: u8 = 0x86;

/// A vendor (extension-unit) control on a UVC camera.
#[derive(Debug, Clone)]
pub struct XuControl {
    pub unit: u8,
    pub selector: u8,
    /// Value before anything was changed; always restored.
    pub init: Vec<u8>,
    pub max: Option<Vec<u8>>,
}

/// A camera node opened for UVC control queries (not for streaming).
pub struct UvcDevice {
    file: std::fs::File,
}

impl UvcDevice {
    pub fn open(index: u32) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/video{index}"))
            .with_context(|| format!("opening /dev/video{index}"))?;
        Ok(Self { file })
    }

    fn query(&self, unit: u8, selector: u8, query: u8, data: &mut [u8]) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut q = UvcXuControlQuery { unit, selector, query, size: data.len() as u16, data: data.as_mut_ptr() };
        // SAFETY: `q` points at a live buffer of `size` bytes for the call.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), UVCIOC_CTRL_QUERY as _, &mut q) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn set(&self, unit: u8, selector: u8, value: &[u8]) -> std::io::Result<()> {
        let mut data = value.to_vec();
        self.query(unit, selector, UVC_SET_CUR, &mut data)
    }

    /// Every writable extension-unit control the camera declares. The kernel
    /// answers only for controls in the camera's descriptors, so probing all
    /// unit/selector pairs is quick and touches nothing.
    pub fn controls(&self) -> Vec<XuControl> {
        let mut found = Vec::new();
        for unit in 1..=254u8 {
            for selector in 1..=254u8 {
                let mut len = [0u8; 2];
                if self.query(unit, selector, UVC_GET_LEN, &mut len).is_err() {
                    continue;
                }
                let len = u16::from_le_bytes(len) as usize;
                if len == 0 || len > 64 {
                    continue;
                }
                let mut info = [0u8; 1];
                // GET_INFO bit 1: SET_CUR supported.
                if self.query(unit, selector, UVC_GET_INFO, &mut info).is_err() || info[0] & 0x02 == 0 {
                    continue;
                }
                let mut init = vec![0u8; len];
                if self.query(unit, selector, UVC_GET_CUR, &mut init).is_err() {
                    continue;
                }
                let mut max = vec![0u8; len];
                let max = self.query(unit, selector, UVC_GET_MAX, &mut max).ok().map(|_| max);
                found.push(XuControl { unit, selector, init, max });
            }
        }
        found
    }
}

/// A greyscale stream from the first infrared camera, whatever
/// `camera.device` says. Used by the IR light search.
pub struct IrStream {
    backend: Option<Backend>,
    pub index: u32,
}

impl IrStream {
    pub fn open() -> Result<Self> {
        let device = list_devices().into_iter().find(|d| d.infrared).context("No infrared camera found")?;
        Ok(Self { backend: Some(Self::start(device.index)?), index: device.index })
    }

    fn start(index: u32) -> Result<Backend> {
        let formats: Vec<CameraFormat> =
            probe(index)?.into_iter().filter(|f| f.format() == FrameFormat::GRAY).collect();
        let fmt = choose_format(&formats).context("The infrared camera offers no greyscale format")?;
        let mut backend = Backend::open(index, fmt)?;
        backend.start()?;
        for _ in 0..8 {
            backend.skip_frame();
        }
        Ok(backend)
    }

    /// Close the stream completely (the old file handle must be gone, or
    /// the camera reports busy), wait a moment, and start it again.
    pub fn reopen(&mut self) -> Result<()> {
        if let Some(mut old) = self.backend.take() {
            old.stop();
        }
        std::thread::sleep(Duration::from_millis(400));
        self.backend = Some(Self::start(self.index)?);
        Ok(())
    }

    pub fn frame(&mut self) -> Result<RgbImage> {
        self.backend.as_mut().context("camera closed")?.read()
    }
}

impl Drop for IrStream {
    fn drop(&mut self) {
        if let Some(backend) = &mut self.backend {
            backend.stop();
        }
    }
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

/// The UVC interface string when there is one ("Integrated IR Camera"): it
/// tells the colour and IR halves of a combined module apart, which the
/// device name ("Integrated RGB Camera" for both) often doesn't.
fn device_name(index: u32) -> String {
    let sys = format!("/sys/class/video4linux/video{index}");
    if let Ok(iface) = std::fs::read_to_string(format!("{sys}/device/interface")) {
        let iface = iface.trim();
        if !iface.is_empty() {
            return iface.to_string();
        }
    }
    std::fs::read_to_string(format!("{sys}/name"))
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

/// Every (format, size, frame rate) a capture device offers. Metadata nodes
/// list no capture formats and come back empty.
fn probe(index: u32) -> Result<Vec<CameraFormat>> {
    use v4l::frameinterval::FrameIntervalEnum;
    use v4l::video::Capture;
    let dev = v4l::Device::new(index as usize).with_context(|| format!("opening /dev/video{index}"))?;
    let mut out = Vec::new();
    for desc in dev.enum_formats()? {
        let Some(ff) = frame_format(&desc.fourcc.repr) else { continue };
        for size in dev.enum_framesizes(desc.fourcc)? {
            for d in size.size.to_discrete() {
                let intervals = dev.enum_frameintervals(desc.fourcc, d.width, d.height).unwrap_or_default();
                let mut rates: Vec<u32> = intervals
                    .iter()
                    .filter_map(|i| match &i.interval {
                        FrameIntervalEnum::Discrete(f) if f.numerator > 0 => Some(f.denominator / f.numerator),
                        FrameIntervalEnum::Stepwise(s) if s.min.numerator > 0 => Some(s.min.denominator / s.min.numerator),
                        _ => None,
                    })
                    .filter(|&fps| fps > 0)
                    .collect();
                if rates.is_empty() {
                    rates.push(15);
                }
                for fps in rates {
                    out.push(CameraFormat::new(Resolution::new(d.width, d.height), ff, fps));
                }
            }
        }
    }
    Ok(out)
}

fn frame_format(fourcc: &[u8; 4]) -> Option<FrameFormat> {
    Some(match fourcc {
        b"MJPG" => FrameFormat::MJPEG,
        b"YUYV" => FrameFormat::YUYV,
        b"NV12" => FrameFormat::NV12,
        b"GREY" => FrameFormat::GRAY,
        b"RGB3" => FrameFormat::RAWRGB,
        b"BGR3" => FrameFormat::RAWBGR,
        _ => return None,
    })
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
    fn detects_a_flashing_emitter() {
        assert_eq!(strobe_level(&[20.0, 90.0, 21.0, 95.0, 22.0]), Some(95.0));
        assert_eq!(strobe_level(&[50.0, 50.0, 26.0, 26.0, 26.0]), None);
        assert_eq!(strobe_level(&[30.0, 40.0, 50.0, 60.0]), None);
        assert_eq!(strobe_level(&[90.0, 20.0, 90.0]), None);
    }

    #[test]
    fn slow_formats_are_last_resort() {
        let formats = [fmt(FrameFormat::MJPEG, 1280, 720, 5), fmt(FrameFormat::YUYV, 640, 480, 30)];
        assert_eq!(choose_format(&formats), Some(fmt(FrameFormat::YUYV, 640, 480, 30)));
    }
}
