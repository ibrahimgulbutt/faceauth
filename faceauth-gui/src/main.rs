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
    let status_page = adw::StatusPage::builder()
        .title("FaceAuth Enrollment")
        .description(get_enrollment_status().as_str())
        .icon_name("camera-web-symbolic")
        .build();

    let enroll_btn = Button::builder()
        .label("Start Enrollment")
        .css_classes(vec!["suggested-action", "pill"])
        .halign(gtk4::Align::Center)
        .margin_bottom(20)
        .build();
    
    let status_box = Box::new(Orientation::Vertical, 12);
    status_box.append(&status_page);
    status_box.append(&enroll_btn);
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

    // Aspect Frame to keep camera ratio
    let aspect_frame = AspectFrame::builder()
        .xalign(0.5)
        .yalign(0.5)
        .ratio(4.0/3.0)
        .obey_child(false)
        .width_request(640)
        .height_request(480)
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

    // Clones for Closure
    let stack_clone = stack.clone();
    let instruction_label_clone = instruction_label.clone();
    let progress_bar_clone = progress_bar.clone();
    let picture_clone = picture.clone();
    let enroll_btn_clone = enroll_btn.clone();

    // Logic
    enroll_btn.connect_clicked(move |_| {
        stack_clone.set_visible_child_name("enroll");
        enroll_btn_clone.set_sensitive(false);
        
        let (tx, rx) = glib::MainContext::channel(glib::Priority::default());
        
        // Clone widgets for the channel callback
        let picture_inner = picture_clone.clone();
        let instruction_inner = instruction_label_clone.clone();
        let progress_inner = progress_bar_clone.clone();
        let stack_inner = stack_clone.clone();

        rx.attach(None, move |msg: EnrollmentUpdate| {
            match msg {
                EnrollmentUpdate::Frame(texture) => {
                    picture_inner.set_paintable(Some(&texture));
                }
                EnrollmentUpdate::Status(text, progress) => {
                    instruction_inner.set_label(&text);
                    progress_inner.set_fraction(progress as f64);
                }
                EnrollmentUpdate::Success => {
                    instruction_inner.set_label("Enrollment Complete!");
                    progress_inner.set_fraction(1.0);
                    // Return to start after 2s
                    let final_stack = stack_inner.clone();
                    glib::timeout_add_seconds_local(2, move || {
                        final_stack.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
                }
                EnrollmentUpdate::Error(e) => {
                    instruction_inner.set_label(&format!("Error: {}", e));
                    // Return to main menu on error
                    let final_stack = stack_inner.clone();
                     glib::timeout_add_seconds_local(3, move || {
                        final_stack.set_visible_child_name("status");
                        glib::ControlFlow::Break
                    });
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

fn run_enrollment_process(tx: glib::Sender<EnrollmentUpdate>) -> anyhow::Result<()> {
    // 1. Open Camera
    let index = CameraIndex::Index(0);
    // Request 640x480 specifically for GUI to reduce bandwidth/processing
    let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(
        nokhwa::utils::CameraFormat::new_from(640, 480, nokhwa::utils::FrameFormat::MJPEG, 30)
    ));
    let mut camera = Camera::new(index, requested)?;
    camera.open_stream()?;

    // 2. Connect to Daemon
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    // Set 60s timeout for enrollment IPC to prevent hanging
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;

    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());

    let _ = tx.send(EnrollmentUpdate::Status("Center your face".to_string(), 0.0));

    // 3. Capture Loop
    let steps = vec![
        ("Center", 40),
        ("Turn Left", 40),
        ("Turn Right", 40),
        ("Look Up", 40),
        ("Look Down", 40),
    ];

    let total_frames: usize = steps.iter().map(|(_, c)| c).sum();
    let mut current_frame_count = 0;
    
    // Tracking
    // let mut last_bbox: Option<(u32, u32, u32, u32)> = None; // removed

    for (step_name, frames_needed) in steps {
        let _ = tx.send(EnrollmentUpdate::Status(format!("Step: {}", step_name), current_frame_count as f32 / total_frames as f32));
        
        let mut frames_collected = 0;
        // Loop until we get enough "good" frames or timeout (optional, let's keep it simple for now)
        // Check "Active Liveness" via daemon response
        
        while frames_collected < frames_needed {
            let frame = camera.frame()?;
            let mut rgb_frame = frame.decode_image::<RgbFormat>()?;
            
            // Draw Static Guidance Overlay
            draw_overlay(&mut rgb_frame);
            
            // Send sample to daemon periodically (every 10th frame ~ 3 times per second)
            if frames_collected % 10 == 0 {
                let req = AuthRequest::EnrollSample {
                    user: user.clone(),
                    image_data: rgb_frame.to_vec(),
                    width: rgb_frame.width(),
                    height: rgb_frame.height(),
                };
                
                // If daemon fails, we might just retry
                match send_request(&mut stream, req) {
                     Ok(_) => {
                         match read_response(&mut stream) {
                             Ok(AuthResponse::Success) => {
                                 // Good frame, advance
                                 frames_collected += 1;
                                 current_frame_count += 1;
                             },
                             Ok(AuthResponse::EnrollmentStatus { message, progress }) => {
                                 if progress < 0.0 {
                                     // No face detected
                                     let _ = tx.send(EnrollmentUpdate::Status("No Face Detected".to_string(), current_frame_count as f32 / total_frames as f32));
                                     // Do NOT advance frames_collected
                                 } else {
                                     // Duplicate or other message
                                     let _ = tx.send(EnrollmentUpdate::Status(message, current_frame_count as f32 / total_frames as f32));
                                     // Do NOT advance
                                 }
                             },
                             Ok(AuthResponse::Failure) => {
                                 // Daemon error?
                                  let _ = tx.send(EnrollmentUpdate::Status("Server Error".to_string(), current_frame_count as f32 / total_frames as f32));
                             },
                             _ => {}
                         }
                     },
                     Err(e) => {
                         // Connection lost?
                         return Err(anyhow::anyhow!("Daemon connection lost: {}", e));
                     }
                }
            } else {
                // For non-sample frames, just advance visual fluidity
                // But we don't advance "progress" unless we confirm via daemon.
                // Actually to make it smooth, we can just advance the loop but not "count" it towards the step completion?
                // The "frames_needed" is just a duration proxy here.
                // Let's just advance frames_collected for visual frames, but maybe slow down if NO face.
                frames_collected += 1;
                current_frame_count += 1; // Update visual progress for smoothness
            }

            // Convert and Send Frame to UI
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
            
            if current_frame_count % 5 == 0 {
                 let _ = tx.send(EnrollmentUpdate::Status(format!("Step: {}", step_name), current_frame_count as f32 / total_frames as f32));
            }
            
            thread::sleep(std::time::Duration::from_millis(15));
        }
    }

    let _ = tx.send(EnrollmentUpdate::Success);
    Ok(())
}

fn draw_overlay(image: &mut ImageBuffer<Rgb<u8>, Vec<u8>>) {
    let (w, h) = image.dimensions();
    let cx = w / 2;
    let cy = h / 2;
    let box_size = 300; // 300x300 box
    let half = box_size / 2;
    
    // Draw Center Box (Static Guidance)
    if cx > half && cy > half {
         draw_rect(image, cx - half, cy - half, cx + half, cy + half, [255, 255, 255]);
    }
    
    // Draw crosshair
    draw_rect(image, cx - 20, cy, cx + 20, cy + 2, [200, 200, 200]);
    draw_rect(image, cx, cy - 20, cx + 2, cy + 20, [200, 200, 200]);
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
