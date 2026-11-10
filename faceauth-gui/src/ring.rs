//! The face ring: a circular, mirrored camera preview that keeps the face
//! centred, surrounded by progress ticks, with a chevron showing which way to
//! turn. Also used (without preview) for the waiting and result states.

use crate::client::Frame;
use faceauth_core::{FaceBox, Pose};
use gtk4::prelude::*;
use gtk4::{cairo, gdk, glib, DrawingArea};
use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

const TICKS: usize = 72;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Nothing happening yet.
    Idle,
    /// A spinner arc: waiting for the password dialog or the camera.
    Waiting,
    /// Live preview with progress.
    Capturing,
    /// Final state: all ticks green (true) or red (false).
    Result(bool),
}

struct State {
    mode: Mode,
    surface: Option<cairo::ImageSurface>,
    face: Option<FaceBox>,
    /// Smoothed crop centre and side, in frame pixels.
    focus: Option<(f64, f64, f64)>,
    progress: f64,
    shown_progress: f64,
    pose: Option<Pose>,
    warning: bool,
    time: f64,
    last_frame_time: Option<i64>,
}

#[derive(Clone)]
pub struct FaceRing {
    pub widget: DrawingArea,
    state: Rc<RefCell<State>>,
}

impl FaceRing {
    pub fn new() -> Self {
        let widget = DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .width_request(220)
            .height_request(220)
            .accessible_role(gtk4::AccessibleRole::Img)
            .build();
        let state = Rc::new(RefCell::new(State {
            mode: Mode::Idle,
            surface: None,
            face: None,
            focus: None,
            progress: 0.0,
            shown_progress: 0.0,
            pose: None,
            warning: false,
            time: 0.0,
            last_frame_time: None,
        }));

        let s = state.clone();
        widget.set_draw_func(move |area, cr, w, h| {
            let _ = draw(&s.borrow(), area, cr, w as f64, h as f64);
        });

        // Drive animations (progress easing, face follow, pulses, spinner).
        let s = state.clone();
        widget.add_tick_callback(move |area, clock| {
            let now = clock.frame_time();
            let mut st = s.borrow_mut();
            let dt = st.last_frame_time.map_or(0.016, |t| ((now - t) as f64 / 1e6).clamp(0.0, 0.1));
            st.last_frame_time = Some(now);
            st.time += dt;
            // Exponential easing, frame-rate independent.
            let k = 1.0 - (-dt * 8.0).exp();
            st.shown_progress += (st.progress - st.shown_progress) * k;
            if let (Some(surface), Some(face)) = (&st.surface, st.face) {
                let (fw, fh) = (surface.width() as f64, surface.height() as f64);
                let target = crop_for(face, fw, fh);
                let kf = 1.0 - (-dt * 5.0).exp();
                st.focus = Some(match st.focus {
                    Some((x, y, side)) => (x + (target.0 - x) * kf, y + (target.1 - y) * kf, side + (target.2 - side) * kf),
                    None => target,
                });
            }
            // Only redraw while something is moving.
            let animating = matches!(st.mode, Mode::Waiting | Mode::Capturing) || (st.progress - st.shown_progress).abs() > 0.001;
            drop(st);
            if animating {
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });

        Self { widget, state }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.widget.queue_draw();
        let mut st = self.state.borrow_mut();
        st.mode = mode;
        if let Mode::Result(_) = mode {
            st.progress = 1.0;
            if !animations_enabled() {
                st.shown_progress = 1.0;
            }
        }
        if mode == Mode::Idle {
            st.surface = None;
            st.face = None;
            st.focus = None;
            st.progress = 0.0;
            st.shown_progress = 0.0;
            st.pose = None;
            st.warning = false;
        }
    }

    pub fn set_frame(&self, frame: Frame) {
        let Frame { data, width, height, stride, face } = frame;
        let Ok(surface) = cairo::ImageSurface::create_for_data(data, cairo::Format::Rgb24, width, height, stride) else {
            return;
        };
        let mut st = self.state.borrow_mut();
        if face.is_some() {
            st.face = face;
        }
        if !animations_enabled() {
            st.focus = st.face.map(|f| crop_for(f, width as f64, height as f64));
        }
        st.surface = Some(surface);
        drop(st);
        self.widget.queue_draw();
    }

    pub fn set_progress(&self, collected: usize, target: usize, pose: Pose, warning: bool) {
        let mut st = self.state.borrow_mut();
        st.progress = if target == 0 { 0.0 } else { collected as f64 / target as f64 };
        if !animations_enabled() {
            st.shown_progress = st.progress;
        }
        st.pose = (collected < target).then_some(pose);
        st.warning = warning;
        drop(st);
        self.widget.queue_draw();
    }
}

/// Honour the desktop's reduced-motion setting: no easing, final states only.
fn animations_enabled() -> bool {
    gtk4::Settings::default().is_none_or(|s| s.is_gtk_enable_animations())
}

/// Square crop (centre x, centre y, side) that frames the face comfortably.
fn crop_for(face: FaceBox, fw: f64, fh: f64) -> (f64, f64, f64) {
    let short = fw.min(fh);
    let side = (face.h as f64 * fh * 2.3).clamp(short * 0.5, short);
    let cx = (face.x as f64 + face.w as f64 / 2.0) * fw;
    // Slightly above the box centre: eyes sit in the middle of the circle.
    let cy = (face.y as f64 + face.h as f64 * 0.45) * fh;
    (cx.clamp(side / 2.0, fw - side / 2.0), cy.clamp(side / 2.0, fh - side / 2.0), side)
}

struct Palette {
    fg: gdk::RGBA,
    success: (f64, f64, f64),
    error: (f64, f64, f64),
    accent: (f64, f64, f64),
    warning: (f64, f64, f64),
}

fn palette(area: &DrawingArea) -> Palette {
    let dark = libadwaita::StyleManager::default().is_dark();
    let hex = |light: u32, dark_v: u32| {
        let v = if dark { dark_v } else { light };
        (((v >> 16) & 0xff) as f64 / 255.0, ((v >> 8) & 0xff) as f64 / 255.0, (v & 0xff) as f64 / 255.0)
    };
    Palette {
        fg: area.color(),
        success: hex(0x26a269, 0x2ec27e),
        error: hex(0xe01b24, 0xff7b63),
        accent: hex(0x1c71d8, 0x78aeed),
        warning: hex(0xc88800, 0xf8e45c),
    }
}

fn draw(st: &State, area: &DrawingArea, cr: &cairo::Context, w: f64, h: f64) -> Result<(), cairo::Error> {
    let p = palette(area);
    let size = w.min(h);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r_out = size / 2.0 - 2.0;
    let tick_len = (size * 0.05).max(8.0);
    let r_in = r_out - tick_len;
    let r_img = r_in - size * 0.035;
    let fg = |a: f64| (p.fg.red() as f64, p.fg.green() as f64, p.fg.blue() as f64, a);

    // Disc behind the preview.
    cr.arc(cx, cy, r_img, 0.0, 2.0 * PI);
    let (r, g, b, a) = fg(0.06);
    cr.set_source_rgba(r, g, b, a);
    cr.fill()?;

    // Camera preview, clipped to the circle and centred on the face.
    if st.mode == Mode::Capturing {
        if let Some(surface) = &st.surface {
            let (fw, fh) = (surface.width() as f64, surface.height() as f64);
            let (fx, fy, side) = st.focus.unwrap_or((fw / 2.0, fh / 2.0, fw.min(fh)));
            cr.save()?;
            cr.arc(cx, cy, r_img, 0.0, 2.0 * PI);
            cr.clip();
            let scale = 2.0 * r_img / side;
            cr.translate(cx, cy);
            cr.scale(scale, scale);
            cr.translate(-fx, -fy);
            cr.set_source_surface(surface, 0.0, 0.0)?;
            cr.source().set_filter(cairo::Filter::Good);
            cr.paint()?;
            cr.restore()?;

            // Soft inner vignette so the edge reads as a lens.
            let grad = cairo::RadialGradient::new(cx, cy, r_img * 0.72, cx, cy, r_img);
            grad.add_color_stop_rgba(0.0, 0.0, 0.0, 0.0, 0.0);
            grad.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.28);
            cr.arc(cx, cy, r_img, 0.0, 2.0 * PI);
            cr.set_source(&grad)?;
            cr.fill()?;
        }
    }

    // Progress ticks around the circle, starting at the top.
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_width((size * 0.012).max(2.0));
    let filled = st.shown_progress * TICKS as f64;
    let pose_angle = st.pose.and_then(pose_angle);
    let pulse = 0.5 + 0.5 * (st.time * 4.0).sin();
    for i in 0..TICKS {
        let angle = -PI / 2.0 + i as f64 * 2.0 * PI / TICKS as f64;
        let (r, g, b, a) = match st.mode {
            Mode::Result(true) => (p.success.0, p.success.1, p.success.2, 1.0),
            Mode::Result(false) => (p.error.0, p.error.1, p.error.2, 1.0),
            _ if (i as f64) < filled => {
                // The tick being filled fades in.
                let a = (filled - i as f64).clamp(0.0, 1.0);
                (p.success.0, p.success.1, p.success.2, 0.25 + 0.75 * a)
            }
            Mode::Capturing => match pose_angle {
                Some(target) if angular_distance(angle, target) < PI / 5.0 => {
                    let c = if st.warning { p.warning } else { p.accent };
                    (c.0, c.1, c.2, 0.35 + 0.45 * pulse)
                }
                _ => fg(0.16),
            },
            _ => fg(0.16),
        };
        cr.set_source_rgba(r, g, b, a);
        cr.move_to(cx + r_in * angle.cos(), cy + r_in * angle.sin());
        cr.line_to(cx + r_out * angle.cos(), cy + r_out * angle.sin());
        cr.stroke()?;
    }

    // Spinner arc while waiting.
    if st.mode == Mode::Waiting {
        let start = st.time * 3.2;
        let sweep = PI * (0.55 + 0.25 * (st.time * 1.7).sin());
        cr.set_line_width(tick_len * 0.55);
        cr.set_source_rgba(p.accent.0, p.accent.1, p.accent.2, 0.9);
        cr.arc(cx, cy, (r_in + r_out) / 2.0, start, start + sweep);
        cr.stroke()?;
    }

    // Chevron pointing the way to turn.
    if st.mode == Mode::Capturing && st.surface.is_some() {
        if let Some(angle) = pose_angle {
            let dist = r_img * 0.74;
            let (ax, ay) = (cx + dist * angle.cos(), cy + dist * angle.sin());
            let arm = size * 0.055;
            let spread = 0.75;
            let back = angle + PI;
            let path = |cr: &cairo::Context| {
                cr.move_to(ax + arm * (back - spread).cos(), ay + arm * (back - spread).sin());
                cr.line_to(ax, ay);
                cr.line_to(ax + arm * (back + spread).cos(), ay + arm * (back + spread).sin());
            };
            cr.set_line_join(cairo::LineJoin::Round);
            cr.set_line_width((size * 0.022).max(3.0) + 3.0);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.35 * (0.6 + 0.4 * pulse));
            path(cr);
            cr.stroke()?;
            cr.set_line_width((size * 0.022).max(3.0));
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.65 + 0.35 * pulse);
            path(cr);
            cr.stroke()?;
        }
    }
    Ok(())
}

/// Screen direction for a pose. The preview is mirrored, so turning to your
/// left moves your face toward the left of the screen.
fn pose_angle(pose: Pose) -> Option<f64> {
    match pose {
        Pose::Center => None,
        Pose::Left => Some(PI),
        Pose::Right => Some(0.0),
        Pose::Up => Some(-PI / 2.0),
        Pose::Down => Some(PI / 2.0),
    }
}

fn angular_distance(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(2.0 * PI);
    d.min(2.0 * PI - d)
}
