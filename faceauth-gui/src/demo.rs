//! Demo mode for UI work and screenshots, without a daemon or a camera:
//! `FACEAUTH_GUI_DEMO=new|ready|outdated|down faceauth-gui`.
//!
//! Replies are scripted and the "camera" is a synthetic silhouette, so
//! screenshots never contain anyone's face. The GUI has no authority of its
//! own, so this cannot weaken anything.

use crate::client::{Frame, SessionEvent, TestEvent};
use faceauth_core::{AuthOutcome, FaceBox, Hint, Pose, UserStatus, MAX_SAMPLES};
use std::time::Duration;

pub fn mode() -> Option<String> {
    std::env::var("FACEAUTH_GUI_DEMO").ok().filter(|v| !v.is_empty())
}

pub fn status() -> Result<UserStatus, String> {
    let m = mode().unwrap_or_default();
    if m == "down" {
        return Err("The FaceAuth service isn't running".into());
    }
    let enrolled = m != "new";
    let three_days_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(3 * 86_400))
        .unwrap_or(0);
    Ok(UserStatus {
        user: "alex".into(),
        enrolled,
        samples: if enrolled { 16 } else { 0 },
        max_samples: MAX_SAMPLES,
        created: if enrolled { three_days_ago } else { 0 },
        updated: if enrolled { three_days_ago } else { 0 },
        model_current: m != "outdated",
        camera: Some("Integrated RGB Camera (/dev/video0)".into()),
        recognizer: "Accurate (ResNet50)".into(),
    })
}

pub fn test(tx: async_channel::Sender<TestEvent>) {
    std::thread::sleep(Duration::from_millis(900));
    let _ = tx.send_blocking(TestEvent::Scanning);
    std::thread::sleep(Duration::from_millis(1400));
    let _ = tx.send_blocking(TestEvent::Outcome(AuthOutcome::Matched));
}

/// Scripted enrollment: password, then poses with a few realistic hints.
pub fn enroll(tx: async_channel::Sender<SessionEvent>, steps: Vec<Pose>, stop_at: Option<usize>) {
    let send = |e| tx.send_blocking(e).is_ok();
    if !send(SessionEvent::Authorizing) {
        return;
    }
    std::thread::sleep(Duration::from_millis(1500));
    if !send(SessionEvent::Started(steps.clone())) {
        return;
    }
    let target = steps.len();
    let mut collected = 0;
    let mut t = 0.0f32;
    let mut since_sample = 0.0f32;
    loop {
        let pose = steps[collected.min(target - 1)];
        // Head drifts toward the requested pose.
        let (dx, dy) = match pose {
            Pose::Center => (0.0, 0.0),
            Pose::Left => (-0.07, 0.0),
            Pose::Right => (0.07, 0.0),
            Pose::Up => (0.0, -0.05),
            Pose::Down => (0.0, 0.05),
        };
        let ease = (since_sample / 0.9).min(1.0);
        let (fx, fy) = (dx * ease + (t * 1.3).sin() * 0.004, dy * ease);
        if tx.try_send(SessionEvent::Frame(frame(fx, fy))).is_err() && tx.is_closed() {
            return;
        }
        std::thread::sleep(Duration::from_millis(66));
        t += 0.066;
        since_sample += 0.066;

        let hint = match since_sample {
            s if s < 0.5 => Hint::TurnMore,
            s if s < 0.8 && collected == 3 => Hint::TooDark,
            _ => Hint::Good,
        };
        if stop_at == Some(collected) && since_sample > 0.6 {
            // Hold here for screenshots.
            let _ = send(SessionEvent::Progress { collected, target, pose, hint: Hint::TurnMore });
            continue;
        }
        if since_sample > 1.1 {
            collected += 1;
            since_sample = 0.0;
            if collected == target {
                let _ = send(SessionEvent::Completed(16));
                return;
            }
        }
        if !send(SessionEvent::Progress { collected, target, pose: steps[collected], hint }) {
            return;
        }
    }
}

/// A soft studio backdrop with a head-and-shoulders silhouette, offset by
/// (dx, dy) as a fraction of the frame.
fn frame(dx: f32, dy: f32) -> Frame {
    let (w, h) = (640i32, 360i32);
    let stride = w * 4;
    let mut data = vec![0u8; (stride * h) as usize];
    let (hx, hy) = (0.5 + dx, 0.42 + dy);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            // Backdrop: warm-to-cool vertical gradient.
            let mut c = [60.0 + 50.0 * (1.0 - v), 70.0 + 40.0 * (1.0 - v), 90.0 + 30.0 * u];
            let head = ((u - hx) / 0.085).powi(2) + ((v - hy) / 0.2).powi(2);
            let shoulders = ((u - 0.5 - dx * 0.4) / 0.25).powi(2) + ((v - 1.05) / 0.32).powi(2);
            if head < 1.0 || shoulders < 1.0 {
                let shade = if head < 1.0 { 1.0 - head * 0.35 } else { 0.8 };
                c = [200.0 * shade, 160.0 * shade, 140.0 * shade];
            }
            let i = (y * stride + x * 4) as usize;
            data[i] = c[2] as u8;
            data[i + 1] = c[1] as u8;
            data[i + 2] = c[0] as u8;
        }
    }
    Some(FaceBox { x: hx - 0.085, y: hy - 0.2, w: 0.17, h: 0.4 })
        .map(|face| Frame { data, width: w, height: h, stride, face: Some(face) })
        .expect("frame")
}
