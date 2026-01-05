use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, Button, Box, Orientation, Label, ProgressBar, Picture, Stack};
use libadwaita::prelude::*;
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

mod detection;
use detection::FaceDetector;

const APP_ID: &str = "org.faceauth.gui";

fn main() {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let content = Box::new(Orientation::Vertical, 0);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_spacing(12);

    let title = Label::builder()
        .label("FaceAuth Enrollment")
        .css_classes(vec!["title-1"])
        .build();
    content.append(&title);

    let status_text = get_enrollment_status();
    let status_label = Label::builder()
        .label(&status_text)
        .build();
    content.append(&status_label);

    // Camera Preview Area
    let picture = Picture::builder()
        .width_request(640)
        .height_request(480)
        .can_shrink(true)
        .content_fit(gtk4::ContentFit::Contain)
        .build();
    
    // Placeholder for camera
    let placeholder = Label::new(Some("Camera Off"));
    placeholder.set_height_request(480);

    let stack = Stack::new();
    stack.add_child(&placeholder);
    stack.add_child(&picture);
    content.append(&stack);

    let instruction_label = Label::builder()
        .label("Ready to Enroll")
        .css_classes(vec!["title-2"])
        .build();
    
    let progress_bar = ProgressBar::builder()
        .visible(false)
        .build();

    let enroll_btn = Button::builder()
        .label("Start Enrollment")
        .css_classes(vec!["suggested-action", "pill"])
        .build();
    
    let instruction_label_clone = instruction_label.clone();
    let progress_bar_clone = progress_bar.clone();
    let enroll_btn_clone = enroll_btn.clone();
    let picture_clone = picture.clone();
    let stack_clone = stack.clone();
    let placeholder = placeholder.clone(); // Clone for the outer closure

    enroll_btn.connect_clicked(move |_| {
        let instruction_label = instruction_label_clone.clone();
        let progress_bar = progress_bar_clone.clone();
        let enroll_btn = enroll_btn_clone.clone();
        let picture = picture_clone.clone();
        let stack = stack_clone.clone();
        let placeholder = placeholder.clone(); // Clone for the outer closure

        enroll_btn.set_sensitive(false);
        progress_bar.set_visible(true);
        progress_bar.set_fraction(0.0);
        instruction_label.set_label("Initializing Camera...");
        stack.set_visible_child(&picture);

        let (tx, rx) = glib::MainContext::channel(glib::Priority::default());

        let placeholder_inner = placeholder.clone(); // Clone for the inner closure
        rx.attach(None, move |msg: EnrollmentUpdate| {
            match msg {
                EnrollmentUpdate::Frame(texture) => {
                    picture.set_paintable(Some(&texture));
                }
                EnrollmentUpdate::Status(text, progress) => {
                    instruction_label.set_label(&text);
                    progress_bar.set_fraction(progress as f64);
                }
                EnrollmentUpdate::Success => {
                    instruction_label.set_label("Enrollment Complete!");
                    progress_bar.set_fraction(1.0);
                    enroll_btn.set_sensitive(true);
                    stack.set_visible_child(&placeholder_inner);
                }
                EnrollmentUpdate::Error(e) => {
                    instruction_label.set_label(&format!("Error: {}", e));
                    enroll_btn.set_sensitive(true);
                    stack.set_visible_child(&placeholder_inner);
                }
            }
            glib::ControlFlow::Continue
        });

        thread::spawn(move || {
            if let Err(e) = run_enrollment_process(tx) {
                eprintln!("Enrollment thread error: {}", e);
            }
        });
    });

    content.append(&instruction_label);
    content.append(&progress_bar);
    content.append(&enroll_btn);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("FaceAuth")
        .default_width(800)
        .default_height(600)
        .child(&content)
        .build();

    window.present();
}

enum EnrollmentUpdate {
    Frame(Texture),
    Status(String, f32),
    Success,
    Error(String),
}

fn run_enrollment_process(tx: glib::Sender<EnrollmentUpdate>) -> anyhow::Result<()> {
    // 1. Open Camera
    let index = CameraIndex::Index(0);
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate);
    let mut camera = Camera::new(index, requested)?;
    camera.open_stream()?;

    // 2. Initialize Detector
    let mut detector = FaceDetector::new(Path::new("/usr/share/faceauth/models/det_500m.onnx"))
        .map_err(|e| anyhow::anyhow!("Failed to load detector: {}", e))?;

    // 3. Connect to Daemon
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let _ = tx.send(EnrollmentUpdate::Status("Center your face".to_string(), 0.0));

    // 4. Capture Loop
    let steps = vec![
        ("Center", 30),
        ("Turn Left", 30),
        ("Turn Right", 30),
        ("Look Up", 30),
        ("Look Down", 30),
    ];

    let total_frames: usize = steps.iter().map(|(_, c)| c).sum();
    let mut current_frame_count = 0;

    for (step_name, frames_needed) in steps {
        let _ = tx.send(EnrollmentUpdate::Status(format!("Step: {}", step_name), current_frame_count as f32 / total_frames as f32));
        
        for i in 0..frames_needed {
            let frame = camera.frame()?;
            let mut rgb_frame = frame.decode_image::<RgbFormat>()?;
            
            // Run Detection
            if let Ok(Some((x1, y1, x2, y2))) = detector.detect(&rgb_frame) {
                // Draw Bounding Box (Green)
                draw_rect(&mut rgb_frame, x1, y1, x2, y2, [0, 255, 0]);
                
                // Send frame to daemon if it's a "good" frame (e.g., every 5th frame)
                if i % 5 == 0 {
                    let req = AuthRequest::EnrollSample {
                        user: user.clone(),
                        image_data: rgb_frame.to_vec(),
                        width: rgb_frame.width(),
                        height: rgb_frame.height(),
                    };
                    send_request(&mut stream, req)?;
                    // Read response (simple success check)
                    read_response(&mut stream)?;
                }
            }

            // Convert to GDK Texture
            let width = rgb_frame.width() as i32;
            let height = rgb_frame.height() as i32;
            let stride = width as usize * 3;
            let bytes = glib::Bytes::from(&rgb_frame.into_raw());
            
            let texture = MemoryTexture::new(
                width, 
                height, 
                MemoryFormat::R8g8b8, 
                &bytes, 
                stride
            );
            
            let _ = tx.send(EnrollmentUpdate::Frame(texture.upcast()));
            
            current_frame_count += 1;
            if current_frame_count % 5 == 0 {
                 let _ = tx.send(EnrollmentUpdate::Status(format!("Step: {}", step_name), current_frame_count as f32 / total_frames as f32));
            }

            thread::sleep(std::time::Duration::from_millis(30));
        }
    }

    let _ = tx.send(EnrollmentUpdate::Success);
    Ok(())
}

fn draw_rect(image: &mut ImageBuffer<Rgb<u8>, Vec<u8>>, x1: u32, y1: u32, x2: u32, y2: u32, color: [u8; 3]) {
    let (w, h) = image.dimensions();
    for x in x1..x2 {
        if x < w {
            if y1 < h { image.put_pixel(x, y1, Rgb(color)); }
            if y2 < h { image.put_pixel(x, y2, Rgb(color)); }
        }
    }
    for y in y1..y2 {
        if y < h {
            if x1 < w { image.put_pixel(x1, y, Rgb(color)); }
            if x2 < w { image.put_pixel(x2, y, Rgb(color)); }
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
