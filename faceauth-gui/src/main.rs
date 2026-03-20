use gtk4::prelude::*;
use gtk4::{Button, Box, Orientation, Label, ProgressBar, Picture, Stack, AspectFrame};
use libadwaita::prelude::*;
use libadwaita as adw;
use std::thread;
use std::sync::{Arc, Mutex};
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};
use glib;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use image::{ImageBuffer, Rgb};
use gtk4::gdk::{Texture, MemoryTexture, MemoryFormat};
use std::path::Path;
use std::time::Duration;

// ... imports remain the same ...

// mod detection; // Removed client-side detection
// use detection::FaceDetector;

const APP_ID: &str = "org.faceauth.gui";

fn main() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &adw::Application) {
    let content = Box::new(Orientation::Vertical, 0);
    
    // Header Bar
    let header_bar = adw::HeaderBar::new();
    content.append(&header_bar);

    // Main Stack for "Welcome/Status" vs "Enrollment"
    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
    
    // Page 1: Status & Welcome
    let enrollment_status = get_enrollment_status();
    let is_enrolled = !enrollment_status.contains("No users enrolled") 
        && !enrollment_status.contains("Daemon not running")
        && !enrollment_status.contains("Failed");

    let status_page = adw::StatusPage::builder()
        .title("FaceAuth Enrollment")
        .description(enrollment_status.as_str())
        .icon_name("camera-web-symbolic")
        .build();

    let enroll_btn = Button::builder()
        .label(if is_enrolled { "Re-enroll (Replace Face Data)" } else { "Enroll My Face" })
        .css_classes(vec!["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_bottom(8)
        .build();

    let add_more_btn = Button::builder()
        .label("Add More Angles")
        .css_classes(vec!["pill"])
        .halign(gtk4::Align::Center)
        .margin_bottom(8)
        .visible(is_enrolled)
        .build();

    let delete_btn = Button::builder()
        .label("Delete Enrollment")
        .css_classes(vec!["destructive-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_bottom(20)
        .visible(is_enrolled)
        .build();
    
    let status_box = Box::new(Orientation::Vertical, 12);
    status_box.append(&status_page);
    status_box.append(&enroll_btn);
    status_box.append(&add_more_btn);
    status_box.append(&delete_btn);
    stack.add_named(&status_box, Some("status"));

    // Page 2: Enrollment (Camera)
    let enroll_box = Box::new(Orientation::Vertical, 12);
    enroll_box.set_valign(gtk4::Align::Center);
    enroll_box.set_halign(gtk4::Align::Center);

    let instruction_label = Label::builder()
        .label("Initializing...")
        .css_classes(vec!["title-2"])
        .build();
    
    let progress_bar = ProgressBar::builder()
        .show_text(true)
        .fraction(0.0)
        .build();

    // Aspect Frame to keep camera ratio  (16:9 = 1280×720)
    let aspect_frame = AspectFrame::builder()
        .xalign(0.5)
        .yalign(0.5)
        .ratio(16.0/9.0)
        .obey_child(false)
        .width_request(640)
        .height_request(360)
        .build();
    
    let picture = Picture::builder()
        .can_shrink(true)
        .content_fit(gtk4::ContentFit::Cover)
        .build();
    
    aspect_frame.set_child(Some(&picture));
    
    enroll_box.append(&instruction_label);
    enroll_box.append(&aspect_frame);
    enroll_box.append(&progress_bar);

    stack.add_named(&enroll_box, Some("enroll"));
    content.append(&stack);

    // Clones for button closures
    let stack_clone          = stack.clone();
    let instruction_clone    = instruction_label.clone();
    let progress_clone       = progress_bar.clone();
    let picture_clone        = picture.clone();
    let enroll_btn_clone     = enroll_btn.clone();
    let enroll_btn_for_del   = enroll_btn.clone();
    let add_more_btn_clone   = add_more_btn.clone();
    let add_more_for_enroll  = add_more_btn.clone();
    let add_more_for_del     = add_more_btn.clone();
    let delete_btn_clone     = delete_btn.clone();

    // Shared helper: wire channel to UI and spawn worker.
    // delete_first=true  → wipe existing data first (Re-enroll)
    // delete_first=false → keep existing embeddings, add new angles
    fn start_enrollment(
        tx:           glib::Sender<EnrollmentUpdate>,
        rx:           glib::Receiver<EnrollmentUpdate>,
        delete_first: bool,
        picture:      gtk4::Picture,
        instruction:  gtk4::Label,
        progress:     gtk4::ProgressBar,
        stack:        gtk4::Stack,
        enroll_btn:   gtk4::Button,
        add_more_btn: gtk4::Button,
        delete_btn:   gtk4::Button,
    ) {
        rx.attach(None, move |msg: EnrollmentUpdate| {
            match msg {
                EnrollmentUpdate::Frame(tex) => { picture.set_paintable(Some(&tex)); }
                EnrollmentUpdate::Status(text, frac) => {
                    instruction.set_label(&text);
                    progress.set_fraction(frac as f64);
                    progress.set_text(Some(&format!("{:.0}%", frac * 100.0)));
                }
                EnrollmentUpdate::Success => {
                    instruction.set_label("Enrollment complete! You can now log in.");
                    progress.set_fraction(1.0);
                    progress.set_text(Some("Done"));
                    let (s, eb, ab, db) = (stack.clone(), enroll_btn.clone(), add_more_btn.clone(), delete_btn.clone());
                    glib::timeout_add_seconds_local(2, move || {
                        eb.set_label("Re-enroll (Replace Face Data)");
                        eb.set_sensitive(true);
                        ab.set_visible(true);
                        ab.set_sensitive(true);
                        db.set_visible(true);
                        s.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
                }
                EnrollmentUpdate::Error(e) => {
                    instruction.set_label(&format!("Error: {}", e));
                    let (s, eb, ab) = (stack.clone(), enroll_btn.clone(), add_more_btn.clone());
                    glib::timeout_add_seconds_local(3, move || {
                        eb.set_sensitive(true);
                        ab.set_sensitive(true);
                        s.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
                }
            }
            glib::ControlFlow::Continue
        });
        thread::spawn(move || {
            if let Err(e) = run_enrollment_process(tx, delete_first) {
                eprintln!("Enrollment thread error: {}", e);
            }
        });
    }

    // ── Re-enroll / first-enroll button ──────────────────────────────────────
    enroll_btn.connect_clicked(move |btn| {
        stack_clone.set_visible_child_name("enroll");
        btn.set_sensitive(false);
        add_more_for_enroll.set_sensitive(false);
        let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
        let do_delete = btn.label().map(|l| l.starts_with("Re-enroll")).unwrap_or(false);
        start_enrollment(tx, rx, do_delete,
            picture_clone.clone(), instruction_clone.clone(), progress_clone.clone(),
            stack_clone.clone(), enroll_btn_clone.clone(),
            add_more_btn_clone.clone(), delete_btn_clone.clone());
    });

    // ── Add More Angles (additive — never replaces existing embeddings) ───────
    let stack_add       = stack.clone();
    let picture_add     = picture.clone();
    let instr_add       = instruction_label.clone();
    let prog_add        = progress_bar.clone();
    let enroll_add      = enroll_btn.clone();
    let add_self        = add_more_btn.clone();
    let add_peer        = add_more_btn.clone();
    let delete_add      = delete_btn.clone();
    add_more_btn.connect_clicked(move |btn| {
        stack_add.set_visible_child_name("enroll");
        btn.set_sensitive(false);
        enroll_add.set_sensitive(false);
        let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
        start_enrollment(tx, rx, false,   // false = keep existing data
            picture_add.clone(), instr_add.clone(), prog_add.clone(),
            stack_add.clone(), enroll_add.clone(),
            add_peer.clone(), delete_add.clone());
        let _ = add_self.clone();
    });

    // ── Delete Enrollment button ──────────────────────────────────────────────
    delete_btn.connect_clicked(move |btn| {
        let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(SOCKET_PATH) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let req = AuthRequest::DeleteUser { user };
            if send_request(&mut stream, req).is_ok() {
                let _ = read_response(&mut stream);
            }
            btn.set_visible(false);
            add_more_for_del.set_visible(false);
            enroll_btn_for_del.set_label("Enroll My Face");
        }
    });

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("FaceAuth")
        .default_width(800)
        .default_height(700)
        .content(&content)
        .build();

    window.present();
}


enum EnrollmentUpdate {
    Frame(Texture),
    Status(String, f32),
    Success,
    Error(String),
}

fn run_enrollment_process(tx: glib::Sender<EnrollmentUpdate>, delete_first: bool) -> anyhow::Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    // If re-enrolling, delete existing data first
    if delete_first {
        let _ = tx.send(EnrollmentUpdate::Status("Clearing previous enrollment data...".to_string(), 0.0));
        if let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
            let req = AuthRequest::DeleteUser { user: user.clone() };
            if send_request(&mut stream, req).is_ok() {
                let _ = read_response(&mut stream);
            }
        }
        thread::sleep(std::time::Duration::from_millis(300));
    }

    // Open camera at 1280×720 to match the auth pipeline (same resolution = consistent embeddings)
    let index = CameraIndex::Index(0);
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        nokhwa::utils::CameraFormat::new_from(1280, 720, nokhwa::utils::FrameFormat::MJPEG, 30)
    ));
    let mut camera = Camera::new(index, requested)?;
    camera.open_stream()?;

    // Warmup: discard first few frames (auto-exposure settling)
    for _ in 0..5 {
        let _ = camera.frame();
    }

    // Connect to daemon
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;

    // Enrollment target and pose guide
    // We collect 10 unique face embeddings covering front + 4 angles (2 samples each).
    // The daemon rejects duplicates (cosine > 0.95) so slightly different angles are needed.
    const TARGET_SAMPLES: usize = 10;
    let pose_guides = [
        "Look straight at the camera",        // sample 1
        "Keep looking straight — hold still",  // sample 2 (slight head sway is enough)
        "Turn your head slightly to the LEFT", // sample 3
        "Stay turned left",                    // sample 4
        "Turn your head slightly to the RIGHT",// sample 5
        "Stay turned right",                   // sample 6
        "Tilt your chin slightly UP",           // sample 7
        "Keep chin up",                         // sample 8
        "Lower your chin slightly DOWN",        // sample 9
        "Almost done — chin down",              // sample 10
    ];

    let mut collected = 0usize;
    let mut frame_num = 0usize;
    let mut face_detected = false;

    let _ = tx.send(EnrollmentUpdate::Status(
        format!("0/{TARGET_SAMPLES} - Look straight at the camera"),
        0.0,
    ));

    loop {
        let frame = camera.frame()?;
        let rgb_frame = frame.decode_image::<RgbFormat>()?;

        // Send frame to daemon every ~500ms (every 15 frames at 30fps)
        if frame_num % 15 == 0 {
            let req = AuthRequest::EnrollSample {
                user: user.clone(),
                image_data: rgb_frame.to_vec(),
                width: rgb_frame.width(),
                height: rgb_frame.height(),
            };

            match send_request(&mut stream, req) {
                Ok(_) => {
                    match read_response(&mut stream) {
                        Ok(AuthResponse::Success) => {
                            collected += 1;
                            face_detected = true;
                            let progress = collected as f32 / TARGET_SAMPLES as f32;
                            if collected >= TARGET_SAMPLES {
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    format!("{collected}/{TARGET_SAMPLES} - all samples collected!"),
                                    1.0,
                                ));
                            } else {
                                let guide = pose_guides[collected.min(pose_guides.len() - 1)];
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    format!("{collected}/{TARGET_SAMPLES} - {guide}"),
                                    progress,
                                ));
                            }
                        },
                        Ok(AuthResponse::EnrollmentStatus { message: _, progress: dp }) => {
                            if dp < 0.0 {
                                // No face detected
                                face_detected = false;
                                let guide = pose_guides[collected.min(pose_guides.len() - 1)];
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    format!("No face detected - step closer & look at camera [{guide}]"),
                                    collected as f32 / TARGET_SAMPLES as f32,
                                ));
                            } else {
                                // Duplicate pose
                                face_detected = true;
                                let guide = pose_guides[collected.min(pose_guides.len() - 1)];
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    format!("Same pose - adjust your angle slightly [{guide}]"),
                                    collected as f32 / TARGET_SAMPLES as f32,
                                ));
                            }
                        },
                        Ok(AuthResponse::Failure) => {
                            let _ = tx.send(EnrollmentUpdate::Status(
                                format!("Daemon error, retrying... ({collected}/{TARGET_SAMPLES})"),
                                collected as f32 / TARGET_SAMPLES as f32,
                            ));
                        },
                        _ => {}
                    }
                },
                Err(e) => return Err(anyhow::anyhow!("Daemon connection lost: {}", e)),
            }
        }

        // Draw colored guide oval on the frame (green = face OK, red = no face)
        let mut display_frame = rgb_frame.clone();
        draw_guide_oval(&mut display_frame, face_detected);

        // Scale down for display (1280×720 → 640×360) to keep UI lightweight
        let display = image::imageops::resize(&display_frame, 640, 360, image::imageops::FilterType::Nearest);
        let width = display.width() as i32;
        let height = display.height() as i32;
        let stride_bytes = width as usize * 3;
        let bytes = glib::Bytes::from(&display.into_raw());
        let texture = MemoryTexture::new(width, height, MemoryFormat::R8g8b8, &bytes, stride_bytes);
        let _ = tx.send(EnrollmentUpdate::Frame(texture.upcast()));

        if collected >= TARGET_SAMPLES {
            break;
        }

        frame_num += 1;
        thread::sleep(std::time::Duration::from_millis(33)); // ~30fps
    }

    let _ = tx.send(EnrollmentUpdate::Success);
    Ok(())
}

fn draw_guide_oval(image: &mut ImageBuffer<Rgb<u8>, Vec<u8>>, face_detected: bool) {
    let (w, h) = image.dimensions();
    let cx = w / 2;
    let cy = h / 2;
    // Draw a centered ellipse outline as a face-positioning guide.
    // Green = face detected and accepted; red = no face.
    let color: [u8; 3] = if face_detected { [60, 200, 80] } else { [220, 60, 60] };
    let rx = w * 18 / 100; // horizontal radius ~18% of width
    let ry = h * 38 / 100; // vertical radius ~38% of height (taller than wide = head shape)
    // Approximate with a thick rectangle for simplicity (avoids heavy floating-point per pixel)
    let x1 = cx.saturating_sub(rx);
    let x2 = (cx + rx).min(w - 1);
    let y1 = cy.saturating_sub(ry);
    let y2 = (cy + ry).min(h - 1);
    draw_rect(image, x1, y1, x2, y2, color);
}

fn draw_overlay(image: &mut ImageBuffer<Rgb<u8>, Vec<u8>>) {
    draw_guide_oval(image, false);
}

fn draw_rect(image: &mut ImageBuffer<Rgb<u8>, Vec<u8>>, x1: u32, y1: u32, x2: u32, y2: u32, color: [u8; 3]) {
    let (w, h) = image.dimensions();
    // 2px thickness
    for x in x1..x2 {
        for t in 0..2 {
            if x < w {
                if y1 + t < h { image.put_pixel(x, y1 + t, Rgb(color)); }
                if y2 + t < h && y2 + t > 0 { image.put_pixel(x, y2 - t, Rgb(color)); }
            }
        }
    }
    for y in y1..y2 {
        for t in 0..2 {
            if y < h {
                if x1 + t < w { image.put_pixel(x1 + t, y, Rgb(color)); }
                if x2 + t < w && x2 + t > 0 { image.put_pixel(x2 - t, y, Rgb(color)); }
            }
        }
    }
}

fn send_request(stream: &mut UnixStream, req: AuthRequest) -> anyhow::Result<()> {
    let data = serde_json::to_vec(&req)?;
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&data)?;
    Ok(())
}

fn read_response(stream: &mut UnixStream) -> anyhow::Result<AuthResponse> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    let resp: AuthResponse = serde_json::from_slice(&buf)?;
    Ok(resp)
}

fn get_enrollment_status() -> String {
    match UnixStream::connect(SOCKET_PATH) {
        Ok(mut stream) => {
            // Set short timeout (5s) for status check
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

            let request = AuthRequest::ListEnrolled;
            if send_request(&mut stream, request).is_ok() {
                if let Ok(response) = read_response(&mut stream) {
                    if let AuthResponse::EnrolledList(users) = response {
                        if users.is_empty() {
                            return "No users enrolled.".to_string();
                        } else {
                            let mut status = String::from("Enrolled Users:\n");
                            for (user, count) in users {
                                status.push_str(&format!("{} ({} faces)\n", user, count));
                            }
                            return status.trim().to_string();
                        }
                    }
                }
            }
            "Failed to query daemon.".to_string()
        },
        Err(_) => "Daemon not running.".to_string(),
    }
}
