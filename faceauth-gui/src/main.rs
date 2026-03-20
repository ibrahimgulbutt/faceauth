use gtk4::prelude::*;
use gtk4::{Button, Box, Orientation, Label, ProgressBar, Picture, Stack, AspectFrame,
           Separator, ScrolledWindow};
use libadwaita::prelude::*;
use libadwaita as adw;
use std::thread;
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};
use glib;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use image::{ImageBuffer, Rgb};
use gtk4::gdk::{Texture, MemoryTexture, MemoryFormat};
use std::time::Duration;

const APP_ID: &str = "org.faceauth.gui";

fn main() {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &adw::Application) {
    // ── Root layout ──────────────────────────────────────────────────────────
    let content = Box::new(Orientation::Vertical, 0);

    let header_bar = adw::HeaderBar::builder()
        .show_end_title_buttons(true)
        .build();
    content.append(&header_bar);

    // Main navigating stack (slides left/right between home and camera page)
    let stack = Stack::new();
    stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
    stack.set_transition_duration(250);

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // PAGE 1 — Home / Status
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    let enrollment_status = get_enrollment_status();
    let is_enrolled = !enrollment_status.contains("No users enrolled")
        && !enrollment_status.contains("Daemon not running")
        && !enrollment_status.contains("Failed");

    // Scrollable home page so it works on small screens too
    let scroll = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .build();

    let home_box = Box::new(Orientation::Vertical, 0);
    home_box.set_halign(gtk4::Align::Center);
    home_box.set_valign(gtk4::Align::Start);
    home_box.set_margin_top(32);
    home_box.set_margin_bottom(32);
    home_box.set_margin_start(24);
    home_box.set_margin_end(24);
    home_box.set_spacing(24);
    home_box.set_size_request(480, -1);

    // ── Hero banner ─────────────────────────────────────────────────────────
    let hero = Box::new(Orientation::Vertical, 8);
    hero.set_halign(gtk4::Align::Center);

    let logo_label = Label::new(Some("󰯄")); // nerd font camera icon fallback
    logo_label.set_css_classes(&["title-1"]);

    let icon_img = gtk4::Image::builder()
        .icon_name("org.faceauth.gui")
        .pixel_size(80)
        .build();
    // Use the themed icon when available, otherwise fallback to a symbolic
    if gtk4::IconTheme::for_display(&gtk4::gdk::Display::default().unwrap())
        .has_icon("org.faceauth.gui")
    {
        hero.append(&icon_img);
    } else {
        let fallback = gtk4::Image::builder()
            .icon_name("camera-web-symbolic")
            .pixel_size(64)
            .css_classes(vec!["dim-label"])
            .build();
        hero.append(&fallback);
    }

    let title_lbl = Label::builder()
        .label("FaceAuth")
        .css_classes(vec!["title-1"])
        .build();
    let subtitle_lbl = Label::builder()
        .label("Face login for Linux")
        .css_classes(vec!["dim-label", "body"])
        .build();
    hero.append(&title_lbl);
    hero.append(&subtitle_lbl);
    home_box.append(&hero);

    // ── Status card ─────────────────────────────────────────────────────────
    let status_card = adw::PreferencesGroup::builder()
        .title("Enrollment Status")
        .build();

    let status_row = adw::ActionRow::builder()
        .title(if is_enrolled { "Face data enrolled" } else { "No face enrolled yet" })
        .subtitle(&enrollment_status)
        .build();
    let status_icon = gtk4::Image::builder()
        .icon_name(if is_enrolled { "emblem-ok-symbolic" } else { "dialog-information-symbolic" })
        .build();
    status_row.add_prefix(&status_icon);
    status_card.add(&status_row);
    home_box.append(&status_card);

    // ── How it works card (shown only on first run) ──────────────────────────
    if !is_enrolled {
        let how_card = adw::PreferencesGroup::builder()
            .title("How to get started")
            .description("Enrollment takes about 30 seconds. Follow these 3 steps:")
            .build();

        let steps = [
            ("1", "camera-web-symbolic",   "Click Enroll below",          "The camera will open automatically"),
            ("2", "face-smile-symbolic",   "Position your face",           "Keep your face inside the guide outline"),
            ("3", "emblem-ok-symbolic",    "Follow the angle prompts",     "Look straight, left, right, up, down"),
        ];
        for (_, icon, title, sub) in &steps {
            let row = adw::ActionRow::builder()
                .title(*title)
                .subtitle(*sub)
                .build();
            let img = gtk4::Image::builder().icon_name(*icon).build();
            row.add_prefix(&img);
            how_card.add(&row);
        }
        home_box.append(&how_card);
    }

    // ── Action buttons ───────────────────────────────────────────────────────
    let actions_card = adw::PreferencesGroup::builder()
        .title(if is_enrolled { "Manage Enrollment" } else { "Get Started" })
        .build();

    // Primary enroll / re-enroll row
    let enroll_row = adw::ActionRow::builder()
        .title(if is_enrolled { "Re-enroll (Replace Face Data)" } else { "Enroll My Face" })
        .subtitle(if is_enrolled {
            "Delete existing data and capture 10 fresh samples"
        } else {
            "Capture 10 face samples across different angles"
        })
        .activatable(true)
        .build();
    let enroll_chevron = gtk4::Image::builder().icon_name("go-next-symbolic").build();
    enroll_row.add_suffix(&enroll_chevron);
    let enroll_icon = gtk4::Image::builder()
        .icon_name(if is_enrolled { "view-refresh-symbolic" } else { "list-add-symbolic" })
        .build();
    enroll_row.add_prefix(&enroll_icon);

    // Add more angles row (only when already enrolled)
    let add_more_row = adw::ActionRow::builder()
        .title("Add More Angles")
        .subtitle("Improve accuracy by adding more face samples")
        .activatable(true)
        .visible(is_enrolled)
        .build();
    let add_chevron = gtk4::Image::builder().icon_name("go-next-symbolic").build();
    add_more_row.add_suffix(&add_chevron);
    let add_icon = gtk4::Image::builder().icon_name("list-add-symbolic").build();
    add_more_row.add_prefix(&add_icon);

    actions_card.add(&enroll_row);
    actions_card.add(&add_more_row);
    home_box.append(&actions_card);

    // ── Danger zone (delete) ─────────────────────────────────────────────────
    let danger_group = adw::PreferencesGroup::builder()
        .title("Danger Zone")
        .build();
    let delete_row = adw::ActionRow::builder()
        .title("Delete Enrollment")
        .subtitle("Remove all stored face data for this user")
        .activatable(true)
        .visible(is_enrolled)
        .build();
    let del_icon = gtk4::Image::builder()
        .icon_name("user-trash-symbolic")
        .css_classes(vec!["error"])
        .build();
    delete_row.add_prefix(&del_icon);
    danger_group.add(&delete_row);
    home_box.append(&danger_group);

    home_box.append(&Separator::new(Orientation::Horizontal)); // bottom breathing room

    scroll.set_child(Some(&home_box));
    stack.add_named(&scroll, Some("status"));

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // PAGE 2 — Enrollment Camera View
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    let enroll_outer = Box::new(Orientation::Vertical, 0);
    enroll_outer.set_vexpand(true);

    // Top strip: step counter + instruction
    let top_strip = Box::new(Orientation::Vertical, 6);
    top_strip.set_margin_top(16);
    top_strip.set_margin_bottom(12);
    top_strip.set_margin_start(24);
    top_strip.set_margin_end(24);

    let instruction_label = Label::builder()
        .label("Initializing camera…")
        .css_classes(vec!["title-2"])
        .halign(gtk4::Align::Center)
        .wrap(true)
        .build();
    let sub_label = Label::builder()
        .label("Keep your face inside the outline")
        .css_classes(vec!["dim-label"])
        .halign(gtk4::Align::Center)
        .build();
    top_strip.append(&instruction_label);
    top_strip.append(&sub_label);
    enroll_outer.append(&top_strip);

    // Camera preview — fills available width
    let aspect_frame = AspectFrame::builder()
        .xalign(0.5)
        .yalign(0.5)
        .ratio(16.0 / 9.0)
        .obey_child(false)
        .hexpand(true)
        .vexpand(true)
        .build();
    let picture = Picture::builder()
        .can_shrink(true)
        .content_fit(gtk4::ContentFit::Cover)
        .hexpand(true)
        .vexpand(true)
        .build();
    aspect_frame.set_child(Some(&picture));
    enroll_outer.append(&aspect_frame);

    // Bottom strip: progress bar + sample counter
    let bottom_strip = Box::new(Orientation::Vertical, 8);
    bottom_strip.set_margin_top(12);
    bottom_strip.set_margin_bottom(20);
    bottom_strip.set_margin_start(24);
    bottom_strip.set_margin_end(24);

    let progress_bar = ProgressBar::builder()
        .show_text(false)
        .fraction(0.0)
        .build();
    progress_bar.set_css_classes(&["osdbar"]);

    let sample_counter = Label::builder()
        .label("0 / 10 samples")
        .css_classes(vec!["caption", "dim-label"])
        .halign(gtk4::Align::Center)
        .build();

    let cancel_btn = Button::builder()
        .label("Cancel")
        .css_classes(vec!["pill"])
        .halign(gtk4::Align::Center)
        .build();

    bottom_strip.append(&progress_bar);
    bottom_strip.append(&sample_counter);
    bottom_strip.append(&cancel_btn);
    enroll_outer.append(&bottom_strip);

    stack.add_named(&enroll_outer, Some("enroll"));
    content.append(&stack);

    // ── Window ────────────────────────────────────────────────────────────────
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("FaceAuth")
        .default_width(760)
        .default_height(680)
        .content(&content)
        .build();
    window.present();

    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
    // Button wiring
    // ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

    fn start_enrollment(
        tx:           glib::Sender<EnrollmentUpdate>,
        rx:           glib::Receiver<EnrollmentUpdate>,
        delete_first: bool,
        picture:      gtk4::Picture,
        instruction:  gtk4::Label,
        sub_label:    gtk4::Label,
        progress:     gtk4::ProgressBar,
        counter:      gtk4::Label,
        stack:        gtk4::Stack,
        enroll_row:   adw::ActionRow,
        add_row:      adw::ActionRow,
        delete_row:   adw::ActionRow,
    ) {
        rx.attach(None, move |msg: EnrollmentUpdate| {
            match msg {
                EnrollmentUpdate::Frame(tex) => {
                    picture.set_paintable(Some(&tex));
                }
                EnrollmentUpdate::Status(main_text, hint, frac) => {
                    instruction.set_label(&main_text);
                    sub_label.set_label(&hint);
                    progress.set_fraction(frac as f64);
                    let n = (frac * 10.0).round() as u32;
                    counter.set_label(&format!("{n} / 10 samples"));
                }
                EnrollmentUpdate::Success => {
                    instruction.set_label("All done!");
                    sub_label.set_label("Face enrollment complete. You can now log in with your face.");
                    progress.set_fraction(1.0);
                    counter.set_label("10 / 10 samples");
                    let (s, er, ar, dr) = (
                        stack.clone(),
                        enroll_row.clone(),
                        add_row.clone(),
                        delete_row.clone(),
                    );
                    glib::timeout_add_seconds_local(2, move || {
                        er.set_title("Re-enroll (Replace Face Data)");
                        er.set_subtitle("Delete existing data and capture 10 fresh samples");
                        er.set_sensitive(true);
                        ar.set_visible(true);
                        ar.set_sensitive(true);
                        dr.set_visible(true);
                        s.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
                }
                EnrollmentUpdate::Error(e) => {
                    instruction.set_label("Enrollment failed");
                    sub_label.set_label(&e);
                    let (s, er, ar) = (stack.clone(), enroll_row.clone(), add_row.clone());
                    glib::timeout_add_seconds_local(3, move || {
                        er.set_sensitive(true);
                        ar.set_sensitive(true);
                        s.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
                }
            }
            glib::ControlFlow::Continue
        });
        thread::spawn(move || {
            if let Err(e) = run_enrollment_process(tx, delete_first) {
                eprintln!("Enrollment thread error: {e}");
            }
        });
    }

    // ── Enroll / Re-enroll row ────────────────────────────────────────────────
    {
        let stack2   = stack.clone();
        let pic2     = picture.clone();
        let instr2   = instruction_label.clone();
        let sub2     = sub_label.clone();
        let prog2    = progress_bar.clone();
        let ctr2     = sample_counter.clone();
        let er2      = enroll_row.clone();
        let ar2      = add_more_row.clone();
        let dr2      = delete_row.clone();
        enroll_row.connect_activated(move |row| {
            stack2.set_visible_child_name("enroll");
            let do_delete = row.title().as_str().starts_with("Re-enroll");
            row.set_sensitive(false);
            ar2.set_sensitive(false);
            let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
            start_enrollment(tx, rx, do_delete,
                pic2.clone(), instr2.clone(), sub2.clone(),
                prog2.clone(), ctr2.clone(), stack2.clone(),
                er2.clone(), ar2.clone(), dr2.clone());
        });
    }

    // ── Add More Angles row ───────────────────────────────────────────────────
    {
        let stack3   = stack.clone();
        let pic3     = picture.clone();
        let instr3   = instruction_label.clone();
        let sub3     = sub_label.clone();
        let prog3    = progress_bar.clone();
        let ctr3     = sample_counter.clone();
        let er3      = enroll_row.clone();
        let ar3      = add_more_row.clone();
        let dr3      = delete_row.clone();
        add_more_row.connect_activated(move |row| {
            stack3.set_visible_child_name("enroll");
            row.set_sensitive(false);
            er3.set_sensitive(false);
            let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
            start_enrollment(tx, rx, false,
                pic3.clone(), instr3.clone(), sub3.clone(),
                prog3.clone(), ctr3.clone(), stack3.clone(),
                er3.clone(), ar3.clone(), dr3.clone());
        });
    }

    // ── Delete row ────────────────────────────────────────────────────────────
    {
        let er4  = enroll_row.clone();
        let ar4  = add_more_row.clone();
        let dr4  = delete_row.clone();
        delete_row.connect_activated(move |row| {
            let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
            if let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let req = AuthRequest::DeleteUser { user };
                if send_request(&mut stream, req).is_ok() {
                    let _ = read_response(&mut stream);
                }
                row.set_visible(false);
                ar4.set_visible(false);
                er4.set_title("Enroll My Face");
                er4.set_subtitle("Capture 10 face samples across different angles");
            }
        });
    }

    // ── Cancel button (returns to home without finishing) ────────────────────
    {
        let stack4 = stack.clone();
        cancel_btn.connect_clicked(move |_| {
            stack4.set_visible_child_name("status");
        });
    }
}


enum EnrollmentUpdate {
    Frame(Texture),
    /// (main instruction text, subtitle hint, progress 0.0–1.0)
    Status(String, String, f32),
    Success,
    Error(String),
}

fn run_enrollment_process(tx: glib::Sender<EnrollmentUpdate>, delete_first: bool) -> anyhow::Result<()> {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    // If re-enrolling, delete existing data first
    if delete_first {
        let _ = tx.send(EnrollmentUpdate::Status(
            "Clearing previous data…".to_string(),
            "Starting fresh enrollment".to_string(),
            0.0,
        ));
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

    // Open camera at 1280×720 to match the auth pipeline
    let index = CameraIndex::Index(0);
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        nokhwa::utils::CameraFormat::new_from(1280, 720, nokhwa::utils::FrameFormat::MJPEG, 30)
    ));
    let mut camera = Camera::new(index, requested)?;
    camera.open_stream()?;

    // Warmup — discard first few frames (auto-exposure settling)
    for _ in 0..5 {
        let _ = camera.frame();
    }

    // Connect to daemon
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;

    // 10 unique embeddings covering 5 angles × 2 samples each.
    const TARGET: usize = 10;
    // (main instruction, subtitle hint)
    let guides: [(&str, &str); TARGET] = [
        ("Face straight at the camera",  "Look directly into the lens"),
        ("Hold still",                   "Keep the same position"),
        ("Turn head LEFT",               "Rotate slowly to your left"),
        ("Hold — stay left",             "Keep the head turned left"),
        ("Turn head RIGHT",              "Rotate slowly to your right"),
        ("Hold — stay right",            "Keep the head turned right"),
        ("Chin UP",                      "Tilt your chin upward slightly"),
        ("Hold — chin up",               "Keep chin raised"),
        ("Chin DOWN",                    "Lower your chin slightly"),
        ("Hold — almost done!",          "Stay still for the last sample"),
    ];

    let mut collected = 0usize;
    let mut frame_num = 0usize;
    let mut face_detected = false;

    let (g0, g1) = guides[0];
    let _ = tx.send(EnrollmentUpdate::Status(g0.to_string(), g1.to_string(), 0.0));

    loop {
        let frame = camera.frame()?;
        let rgb_frame = frame.decode_image::<RgbFormat>()?;

        // Send frame to daemon every ~500 ms (every 15 frames at 30 fps)
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
                            let progress = collected as f32 / TARGET as f32;
                            if collected >= TARGET {
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    "All samples collected!".to_string(),
                                    "Finishing up…".to_string(),
                                    1.0,
                                ));
                            } else {
                                let (gm, gs) = guides[collected.min(TARGET - 1)];
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    gm.to_string(),
                                    gs.to_string(),
                                    progress,
                                ));
                            }
                        }
                        Ok(AuthResponse::EnrollmentStatus { message: _, progress: dp }) => {
                            let (gm, _) = guides[collected.min(TARGET - 1)];
                            if dp < 0.0 {
                                face_detected = false;
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    "No face detected".to_string(),
                                    format!("Step closer and {}", gm.to_lowercase()),
                                    collected as f32 / TARGET as f32,
                                ));
                            } else {
                                face_detected = true;
                                let _ = tx.send(EnrollmentUpdate::Status(
                                    "Same angle — move slightly".to_string(),
                                    format!("Try: {}", gm.to_lowercase()),
                                    collected as f32 / TARGET as f32,
                                ));
                            }
                        }
                        Ok(AuthResponse::Failure) => {
                            let _ = tx.send(EnrollmentUpdate::Status(
                                format!("Retrying… ({collected}/{TARGET})"),
                                "Daemon busy, please wait".to_string(),
                                collected as f32 / TARGET as f32,
                            ));
                        }
                        _ => {}
                    }
                }
                Err(e) => return Err(anyhow::anyhow!("Daemon connection lost: {}", e)),
            }
        }

        // Mirror display frame (selfie view); raw unflipped frame was already sent above
        let mut display_frame = image::imageops::flip_horizontal(&rgb_frame);
        draw_guide_oval(&mut display_frame, face_detected);

        // Scale to 640×360 for display
        let display = image::imageops::resize(&display_frame, 640, 360, image::imageops::FilterType::Nearest);
        let width  = display.width() as i32;
        let height = display.height() as i32;
        let stride = width as usize * 3;
        let bytes   = glib::Bytes::from(&display.into_raw());
        let texture = MemoryTexture::new(width, height, MemoryFormat::R8g8b8, &bytes, stride);
        let _ = tx.send(EnrollmentUpdate::Frame(texture.upcast()));

        if collected >= TARGET {
            break;
        }

        frame_num += 1;
        thread::sleep(std::time::Duration::from_millis(33)); // ~30 fps
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
