//! Automatic search for the vendor command that switches on an infrared
//! camera's light (emitter), for `faceauth ir-setup`.
//!
//! Windows drivers send a vendor command (a UVC extension-unit control) to
//! light the emitter; Linux doesn't know which. Like linux-enable-ir-emitter,
//! this tries the camera's writable vendor controls one at a time, nudging
//! one byte by a small step, but instead of asking the user it judges the
//! result itself: the face (or, with no face in view, the middle of the
//! picture) must get clearly brighter, go dark again when the control is put
//! back, and light up again when it is re-applied. Every control is restored
//! to its original value straight after each try, found or not; the daemon
//! re-sends a found command each time it starts the camera.

use crate::camera::{mean_brightness, IrStream, UvcDevice, XuControl};
use crate::detection::{self, FaceDetector};
use anyhow::{anyhow, Context, Result};
use faceauth_core::{EmitterControl, IrSetupEvent, Response};
use image::RgbImage;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;

/// Values tried per byte: the original value + 1 ..= + this (the same
/// default as linux-enable-ir-emitter).
const TRIES_PER_BYTE: u8 = 5;
/// Only the first bytes of each control are varied; emitter switches are
/// near the start.
const BYTES_PER_CONTROL: usize = 4;
/// Longer controls are left alone: emitter switches are short, and long
/// blobs are where calibration and firmware data live.
const MAX_CONTROL_LEN: usize = 16;
/// A face (or picture) at least this bright, and clearly brighter than
/// before, counts as lit.
const LIT: f32 = 55.0;
const LIT_FACTOR: f32 = 1.7;
const LIT_GAIN: f32 = 20.0;
/// This bright before anything changes: the light is already on (even with
/// no face found).
const ALREADY_ON: f32 = 90.0;
/// Time for a changed control to take effect.
const SETTLE: Duration = Duration::from_millis(150);
const TIME_LIMIT: Duration = Duration::from_secs(8 * 60);

pub fn search(detector: &FaceDetector, tx: &Sender<Response>, cancel: &AtomicBool) -> Result<IrSetupEvent> {
    let mut stream = IrStream::open()?;
    let device = UvcDevice::open(stream.index)?;
    let mut controls: Vec<XuControl> =
        device.controls().into_iter().filter(|c| c.init.len() <= MAX_CONTROL_LEN).collect();
    // Small controls first: that's where emitter switches usually are.
    controls.sort_by_key(|c| c.init.len());

    let (base, faces) = baseline(&mut stream, detector)?;
    info!(
        "IR setup: {} vendor controls on /dev/video{}, brightness {base:.0}, face in {faces} of 3 looks",
        controls.len(),
        stream.index
    );
    // The camera already sees the face: its light works (it only lights what
    // is close, so the face can be dim and still be recognised; dim faces
    // are brightened before recognition). Nothing to search for.
    if faces >= 2 || base >= ALREADY_ON {
        return Ok(IrSetupEvent::Found { controls: Vec::new(), before: base, after: base });
    }

    let plan: Vec<(usize, Vec<u8>)> =
        controls.iter().enumerate().flat_map(|(i, c)| trials(c).into_iter().map(move |v| (i, v))).collect();
    let total = plan.len();
    send(tx, IrSetupEvent::Started { total, brightness: base });

    let started = Instant::now();
    for (tried, (i, value)) in plan.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(IrSetupEvent::Failed { message: "cancelled".into() });
        }
        if started.elapsed() > TIME_LIMIT {
            warn!("IR setup: time limit reached after {tried} of {total} tries");
            break;
        }
        if tried % 4 == 0 {
            send(tx, IrSetupEvent::Progress { tried, total });
        }
        let ctrl = &controls[i];
        if device.set(ctrl.unit, ctrl.selector, &value).is_err() {
            restore(&device, ctrl)?;
            continue;
        }
        std::thread::sleep(SETTLE);
        let level = measure_or_recover(&mut stream, detector, &device, ctrl)?;
        log::debug!("IR setup: unit {} selector {} {:?} → {level:.0}", ctrl.unit, ctrl.selector, value);
        if !is_lit(level, base) {
            restore(&device, ctrl)?;
            continue;
        }

        // It got brighter. Make sure it's this control: off again, on again.
        restore(&device, ctrl)?;
        std::thread::sleep(SETTLE);
        let off = measure_or_recover(&mut stream, detector, &device, ctrl)?;
        device.set(ctrl.unit, ctrl.selector, &value).context("re-applying a camera setting")?;
        std::thread::sleep(SETTLE);
        let on = measure_or_recover(&mut stream, detector, &device, ctrl)?;
        restore(&device, ctrl)?;
        if !is_lit(off, base) && is_lit(on, base) {
            info!(
                "IR setup: unit {} selector {} = {:?} lights the emitter ({base:.0} → {on:.0})",
                ctrl.unit, ctrl.selector, value
            );
            return Ok(IrSetupEvent::Found {
                controls: vec![EmitterControl { unit: ctrl.unit, selector: ctrl.selector, control: value }],
                before: base,
                after: on,
            });
        }
        info!("IR setup: unit {} selector {} brightened once but not repeatably", ctrl.unit, ctrl.selector);
    }
    Ok(IrSetupEvent::NotFound)
}

/// Values to try for one control: each of the first bytes in turn, raised
/// by 1..=TRIES_PER_BYTE from its original value, within the control's max.
fn trials(ctrl: &XuControl) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for byte in 0..ctrl.init.len().min(BYTES_PER_CONTROL) {
        // The camera's own maximum for this byte (0 means it can't change).
        let limit = ctrl.max.as_ref().and_then(|m| m.get(byte).copied()).unwrap_or(255);
        for step in 1..=TRIES_PER_BYTE {
            let Some(v) = ctrl.init[byte].checked_add(step) else { break };
            if v > limit {
                break;
            }
            let mut value = ctrl.init.clone();
            value[byte] = v;
            out.push(value);
        }
    }
    out
}

fn is_lit(level: f32, base: f32) -> bool {
    level >= LIT && level >= base * LIT_FACTOR && level - base >= LIT_GAIN
}

fn restore(device: &UvcDevice, ctrl: &XuControl) -> Result<()> {
    device.set(ctrl.unit, ctrl.selector, &ctrl.init).map_err(|e| {
        anyhow!(
            "a camera setting (unit {}, selector {}) could not be put back ({e}). \
             Shut the computer down fully and start it again to reset the camera",
            ctrl.unit,
            ctrl.selector
        )
    })
}

/// Median brightness of three looks, and in how many a face was found.
fn baseline(stream: &mut IrStream, detector: &FaceDetector) -> Result<(f32, usize)> {
    let looks = [look(stream, detector)?, look(stream, detector)?, look(stream, detector)?];
    let faces = looks.iter().filter(|(_, face)| *face).count();
    let mut levels = looks.map(|(level, _)| level);
    levels.sort_by(|a, b| a.total_cmp(b));
    Ok((levels[1], faces))
}

/// A measurement that survives a control knocking the stream over once:
/// put the control back and reopen the camera.
fn measure_or_recover(
    stream: &mut IrStream,
    detector: &FaceDetector,
    device: &UvcDevice,
    ctrl: &XuControl,
) -> Result<f32> {
    match measure(stream, detector) {
        Ok(level) => Ok(level),
        Err(e) => {
            warn!("IR setup: camera stopped responding after unit {} selector {}: {e:#}", ctrl.unit, ctrl.selector);
            restore(device, ctrl)?;
            stream.reopen().context("the camera stopped responding")?;
            measure(stream, detector)
        }
    }
}

/// Brightness of the face in view (or the picture centre), from the
/// brightest of a few fresh frames, so an emitter that flashes on every
/// other frame still counts.
fn measure(stream: &mut IrStream, detector: &FaceDetector) -> Result<f32> {
    Ok(look(stream, detector)?.0)
}

/// Brightness as for `measure`, and whether a face was found.
fn look(stream: &mut IrStream, detector: &FaceDetector) -> Result<(f32, bool)> {
    // Frames queued before the change are stale.
    for _ in 0..2 {
        stream.frame()?;
    }
    let mut best: Option<(f32, RgbImage)> = None;
    for _ in 0..3 {
        let frame = stream.frame()?;
        let level = mean_brightness(&frame);
        if best.as_ref().is_none_or(|(b, _)| level > *b) {
            best = Some((level, frame));
        }
    }
    let (_, frame) = best.expect("three frames were read");
    let faces = detector.detect(&frame).unwrap_or_default();
    Ok(match detection::primary(&faces) {
        Some(face) => (detection::face_brightness(&frame, face), true),
        None => (centre_brightness(&frame), false),
    })
}

fn centre_brightness(img: &RgbImage) -> f32 {
    let (w, h) = img.dimensions();
    let centre = image::imageops::crop_imm(img, w / 4, h / 4, w / 2, h / 2).to_image();
    mean_brightness(&centre)
}

fn send(tx: &Sender<Response>, event: IrSetupEvent) {
    let _ = tx.blocking_send(Response::IrSetup(event));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl(init: &[u8], max: Option<&[u8]>) -> XuControl {
        XuControl { unit: 4, selector: 2, init: init.to_vec(), max: max.map(|m| m.to_vec()) }
    }

    #[test]
    fn tries_small_steps_on_the_first_bytes() {
        let t = trials(&ctrl(&[0, 3, 3, 0, 0, 0, 0, 0, 0], None));
        assert_eq!(t.len(), 4 * TRIES_PER_BYTE as usize, "first four bytes, five values each");
        assert_eq!(t[0], vec![1, 3, 3, 0, 0, 0, 0, 0, 0]);
        assert_eq!(t[5], vec![0, 4, 3, 0, 0, 0, 0, 0, 0]);
        assert!(t.iter().all(|v| v.len() == 9));
    }

    #[test]
    fn respects_the_maximum_and_never_wraps() {
        assert_eq!(trials(&ctrl(&[1], Some(&[3]))), vec![vec![2], vec![3]]);
        assert!(trials(&ctrl(&[0, 0], Some(&[0, 0]))).is_empty(), "a maximum of 0 means fixed");
        // The Lenovo/Chicony pattern: only the third byte may rise, to 3.
        assert_eq!(
            trials(&ctrl(&[1, 3, 1, 0, 0, 0, 0, 0, 0], Some(&[1, 3, 3, 0, 0, 0, 0, 0, 0]))),
            vec![vec![1, 3, 2, 0, 0, 0, 0, 0, 0], vec![1, 3, 3, 0, 0, 0, 0, 0, 0]]
        );
        assert_eq!(trials(&ctrl(&[254], None)), vec![vec![255]]);
        assert!(trials(&ctrl(&[255], None)).is_empty());
    }

    #[test]
    fn lit_means_clearly_brighter() {
        assert!(is_lit(120.0, 20.0));
        assert!(!is_lit(40.0, 15.0), "too dark to use");
        assert!(!is_lit(60.0, 50.0), "barely changed");
        assert!(!is_lit(70.0, 60.0));
    }
}

#[cfg(test)]
mod hardware {
    use super::*;

    /// Read-only: list the IR camera's writable vendor controls and the tries
    /// planned for them. Nothing is written.
    /// `cargo test -p faceauthd list_ir_controls -- --ignored --nocapture`
    #[test]
    #[ignore = "needs an infrared camera"]
    fn list_ir_controls() {
        let ir = crate::camera::list_devices().into_iter().find(|d| d.infrared).expect("an infrared camera");
        let device = UvcDevice::open(ir.index).unwrap();
        let controls: Vec<XuControl> =
            device.controls().into_iter().filter(|c| c.init.len() <= MAX_CONTROL_LEN).collect();
        let total: usize = controls.iter().map(|c| trials(c).len()).sum();
        for c in &controls {
            println!("unit {:3} selector {:3} len {:2} init {:?} max {:?}", c.unit, c.selector, c.init.len(), c.init, c.max);
        }
        println!("{} writable controls on /dev/video{}, {total} tries planned", controls.len(), ir.index);
    }
}
