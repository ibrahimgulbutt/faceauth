use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};
use tokio::net::{UnixListener, UnixStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::fs;
use std::path::Path;
use anyhow::Result;
use log::{info, error, warn};
use std::sync::{Arc, Mutex};

mod camera;
mod recognition;
mod storage;
mod detection;
mod config;
mod liveness;
mod security;

use camera::CameraManager;
use recognition::FaceEngine;
use detection::FaceDetector;
use storage::{SecureStorage, UserProfile};
use config::Config;
use security::{RateLimiter, AuditLogger};

fn check_system_capabilities() -> Result<()> {
    // 1. Check Memory via /proc/meminfo
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemAvailable:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let Ok(kb) = parts[1].parse::<u64>() {
                        let mb = kb / 1024;
                        info!("System Capability Check: {} MB RAM available", mb);
                        if mb < 500 {
                            warn!("Low memory detected ({} MB). Processing might be slow or unstable.", mb);
                        }
                    }
                }
            }
        }
    }

    // 2. Check CPU Cores
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    info!("System Capability Check: {} CPU cores detected", cores);
    if cores < 2 {
        warn!("Single-core CPU detected. Performance might be impacted.");
    }

    // 3. Check Disk Space on / (Root)
    match std::process::Command::new("df").arg("-P").arg("/").output() {
        Ok(output) => {
            if let Ok(stdout) = String::from_utf8(output.stdout) {
                let lines: Vec<&str> = stdout.lines().collect();
                // Expect header + 1 line
                if lines.len() >= 2 {
                    let parts: Vec<&str> = lines[1].split_whitespace().collect();
                    // Fields: Filesystem 1024-blocks Used Available Capacity Mounted on
                    // Available is index 3
                    if parts.len() >= 4 {
                        if let Ok(avail_kb) = parts[3].parse::<u64>() {
                            let avail_mb = avail_kb / 1024;
                            info!("System Capability Check: {} MB Disk Space available on /", avail_mb);
                            if avail_mb < 500 { // Less than 500MB
                                warn!("Critical: Very low disk space ({} MB). System might hang on logging or writes.", avail_mb);
                            } else if avail_mb < 2048 { // Less than 2GB
                                warn!("Low disk space ({} MB).", avail_mb);
                            }
                        }
                    }
                }
            }
        },
        Err(e) => {
            warn!("Could not check disk space: {}", e);
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Force info level if RUST_LOG is not set
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    env_logger::init();
    info!("Starting faceauthd (Phase 4)...");

    // Perform Infrastructure Checks
    if let Err(e) = check_system_capabilities() {
        warn!("Failed to perform system capability checks: {}", e);
    }

    let config = match Config::load() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            error!("Failed to load config: {}", e);
            return Err(e);
        }
    };
    info!("Configuration loaded: {:?}", config);

    // Initialize Rate Limiter
    let rate_limiter = Arc::new(RateLimiter::new(config.security.max_attempts, config.security.lockout_seconds));

    // Initialize Camera
    let mut cam_manager = CameraManager::new();
    if let Err(e) = cam_manager.initialize() {
        error!("Failed to initialize camera: {}. Continuing without camera for now.", e);
    }
    let cam_manager = Arc::new(Mutex::new(cam_manager));

    // Initialize Face Detector
    let detector = match FaceDetector::new(Path::new("/usr/share/faceauth/models/det_500m.onnx"), config.detection.confidence_threshold) {
        Ok(d) => Arc::new(d),
        Err(e) => {
            error!("Failed to initialize Face Detector: {}", e);
            // We can't proceed securely without detection
            return Err(e);
        }
    };

    // Initialize Face Engine
    let face_engine = match FaceEngine::new() {
        Ok(engine) => Arc::new(engine),
        Err(e) => {
            error!("Failed to initialize Face Engine: {}", e);
            return Err(e);
        }
    };

    // Initialize Storage
    let storage = match SecureStorage::new() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            error!("Failed to initialize Secure Storage: {}", e);
            return Err(e);
        }
    };

    if Path::new(SOCKET_PATH).exists() {
        fs::remove_file(SOCKET_PATH)?;
    }

    let listener = UnixListener::bind(SOCKET_PATH)?;
    info!("Listening on {}", SOCKET_PATH);

    use std::os::unix::fs::PermissionsExt;
    // Security: Use 666 (RW-RW-RW-) instead of 777.
    // Executable permission is not needed for sockets.
    // 666 allows PAM (root) and GUI (user) to connect.
    fs::set_permissions(SOCKET_PATH, fs::Permissions::from_mode(0o666))?;

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let cam_clone = cam_manager.clone();
                let detector_clone = detector.clone();
                let engine_clone = face_engine.clone();
                let storage_clone = storage.clone();
                let config_clone = config.clone();
                let rate_limiter_clone = rate_limiter.clone();
                
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, cam_clone, detector_clone, engine_clone, storage_clone, config_clone, rate_limiter_clone).await {
                        error!("Error handling client: {}", e);
                    }
                });
            }
            Err(e) => {
                error!("Accept failed: {}", e);
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream, 
    camera: Arc<Mutex<CameraManager>>,
    detector: Arc<FaceDetector>,
    engine: Arc<FaceEngine>,
    storage: Arc<SecureStorage>,
    config: Arc<Config>,
    rate_limiter: Arc<RateLimiter>
) -> Result<()> {
    loop {
        // Read length prefix
        let mut len_buf = [0u8; 4];
        match stream.read_exact(&mut len_buf).await {
            Ok(_) => {},
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;

        // Read body
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;

        let request: AuthRequest = serde_json::from_slice(&buf)?;
        // info!("Received request: {:?}", request); // Too verbose for frames

        let response = match request {
            AuthRequest::Ping => AuthResponse::Pong,

            AuthRequest::Benchmark => {
                info!("Running benchmark...");
                let start = std::time::Instant::now();
                
                // 1. Capture Sequence (Full Pipeline Simulation)
                let cap_start = std::time::Instant::now();
                // 1. Overlapped Capture & Detection
                let cap_start = std::time::Instant::now();
                
                let camera_clone = camera.clone();
                let config_clone = config.clone();
                let detector_clone = detector.clone();

                let (tx_frame0, rx_frame0) = tokio::sync::oneshot::channel();
                let (tx_full, rx_full) = tokio::sync::oneshot::channel();

                tokio::spawn(async move {
                    let res = tokio::task::spawn_blocking(move || {
                        let cam_mgr = match camera_clone.lock() {
                            Ok(g) => g,
                            Err(poisoned) => { warn!("Camera mutex poisoned, recovering"); poisoned.into_inner() }
                        };
                        let mut active_cam = cam_mgr.start_session()?;
                        active_cam.warmup(config_clone.camera.warmup_frames)?;
                        let frame0 = active_cam.capture_frame()?;
                        let _ = tx_frame0.send(frame0.clone());
                        let remaining = config_clone.camera.sequence_length - 1;
                        let rest = active_cam.capture_sequence(remaining, config_clone.camera.sequence_interval_ms)?;
                        Ok((frame0, rest))
                    }).await;
                    let final_res = match res {
                        Ok(r) => r,
                        Err(e) => Err(anyhow::anyhow!("Task join error: {}", e)),
                    };
                    let _ = tx_full.send(final_res);
                });

                let frame0_res = rx_frame0.await;
                
                if let Ok(frame0) = frame0_res {
                    let det_start = std::time::Instant::now();
                    let frame0_clone = frame0.clone();
                    let detection_task = tokio::task::spawn_blocking(move || {
                        detector_clone.detect(&frame0_clone)
                    });

                    let full_res = rx_full.await;
                    let cap_time = cap_start.elapsed().as_secs_f32() * 1000.0;

                    if let Ok(Ok((_, remaining))) = full_res {
                        let detection = detection_task.await;
                        let det_time = det_start.elapsed().as_secs_f32() * 1000.0;

                        let mut frames = vec![frame0];
                        frames.extend(remaining);

                        let rec_start = std::time::Instant::now();
                        if let Ok(Ok(Some(face))) = detection {
                            let engine_clone = engine.clone();
                            let face_clone = face.clone();
                            let mut tasks = Vec::new();
                            for _ in 0..frames.len() {
                                let e = engine_clone.clone();
                                let f = face_clone.clone();
                                tasks.push(tokio::task::spawn_blocking(move || {
                                    e.get_embedding(&f)
                                }));
                            }
                            futures::future::join_all(tasks).await;
                        }
                        let rec_time = rec_start.elapsed().as_secs_f32() * 1000.0;
                        let total_time = start.elapsed().as_secs_f32() * 1000.0;

                        info!("Benchmark: Cap={:.2}ms, Det={:.2}ms, Rec={:.2}ms, Total={:.2}ms", cap_time, det_time, rec_time, total_time);
                        AuthResponse::BenchmarkResult { 
                            detection_ms: det_time, 
                            recognition_ms: rec_time,
                            capture_ms: cap_time,
                            total_ms: total_time
                        }
                    } else {
                        error!("Benchmark failed: Camera error");
                        AuthResponse::Failure
                    }
                } else {
                    error!("Benchmark failed: Frame 0 error");
                    AuthResponse::Failure
                }
            },
            
            AuthRequest::Enroll { user: _, name: _ } => {
                // Legacy enroll (daemon captures)
                // For now, we just return success to acknowledge
                AuthResponse::EnrollmentStatus { message: "Ready for samples".to_string(), progress: 0.0 }
            },

            AuthRequest::EnrollSample { user, image_data, width, height } => {
                // Process the frame sent by GUI
                if let Some(img) = image::ImageBuffer::<image::Rgb<u8>, Vec<u8>>::from_raw(width, height, image_data) {
                    
                    // 1. Detect Face in the raw frame
                    match detector.detect(&img) {
                        Ok(Some(cropped_face)) => {
                             // 2. Get Embedding from cropped face
                             match engine.get_embedding(&cropped_face) {
                                Ok(embedding) => {
                                    // Load existing profile or create new
                                    let mut profile = storage.load_user(&user)?.unwrap_or(UserProfile {
                                        user: user.clone(),
                                        name: user.clone(),
                                        embeddings: vec![],
                                        last_updated: 0,
                                    });
                                    
                                    // Check for duplicates
                                    let mut is_duplicate = false;
                                    for existing in &profile.embeddings {
                                        let score = engine.compare(&embedding, existing);
                                        if score > 0.95 {
                                            is_duplicate = true;
                                            break;
                                        }
                                    }

                                    if is_duplicate {
                                        AuthResponse::EnrollmentStatus { 
                                            message: "Hold steady or turn slightly".to_string(), 
                                            progress: profile.embeddings.len() as f32 
                                        }
                                    } else {
                                        profile.embeddings.push(embedding);
                                        profile.last_updated = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
                                        
                                        storage.save_user(&profile)?;
                                        AuthResponse::Success
                                    }
                                },
                                Err(e) => {
                                    error!("Embedding failed: {}", e);
                                    AuthResponse::Failure
                                }
                            }
                        },
                        Ok(None) => {
                            // No face detected
                             AuthResponse::EnrollmentStatus { 
                                message: "No face detected".to_string(), 
                                progress: -1.0 
                            }
                        },
                        Err(e) => {
                             error!("Detection failed: {}", e);
                             AuthResponse::Failure
                        }
                    }
                } else {
                    AuthResponse::Failure
                }
            },

            AuthRequest::ListEnrolled => {
                match storage.list_users() {
                    Ok(users) => AuthResponse::EnrolledList(users),
                    Err(e) => {
                        error!("Failed to list users: {}", e);
                        AuthResponse::Failure
                    }
                }
            },

            AuthRequest::DeleteUser { user } => {
                info!("Deleting enrollment data for user: {}", user);
                match storage.delete_user(&user) {
                    Ok(_) => AuthResponse::Success,
                    Err(e) => {
                        error!("Failed to delete user {}: {}", user, e);
                        AuthResponse::Failure
                    }
                }
            },

            AuthRequest::Authenticate { user } => {
                info!("Authenticating user: {}", user);

                // 0. Rate Limit Check
                if !rate_limiter.check_allowed(&user) {
                    warn!("Authentication rejected by rate limiter for user {}", user);
                    return send_response(&mut stream, AuthResponse::Failure).await;
                }
                
                // 1. Load user profile
                let profile = match storage.load_user(&user) {
                    Ok(Some(p)) => {
                        if p.embeddings.is_empty() {
                            warn!("User {} has no enrolled faces", user);
                            return send_response(&mut stream, AuthResponse::Failure).await;
                        }
                        p
                    },
                    Ok(None) => {
                        warn!("User {} not found", user);
                        return send_response(&mut stream, AuthResponse::Failure).await;
                    },
                    Err(e) => {
                        error!("Storage error: {}", e);
                        return send_response(&mut stream, AuthResponse::Failure).await;
                    }
                };

                // 2. Overlapped Capture & Processing Strategy (Phase 7A)
                let camera_clone = camera.clone();
                let config_clone = config.clone();
                
                // Channels for coordinating overlap
                let (tx_frame0, rx_frame0) = tokio::sync::oneshot::channel();
                let (tx_full, rx_full) = tokio::sync::oneshot::channel();

                // Spawn Camera Task
                tokio::spawn(async move {
                    // Move into blocking thread for I/O
                    let res = tokio::task::spawn_blocking(move || {
                        // Acquire lock inside blocking task
                        let cam_mgr = match camera_clone.lock() {
                            Ok(g) => g,
                            Err(poisoned) => { warn!("Camera mutex poisoned, recovering"); poisoned.into_inner() }
                        };
                        
                        // Start Session
                        // Solid Solution Fix: Retry logic for wake-from-sleep race condition
                        let mut active_cam = match cam_mgr.start_session() {
                            Ok(cam) => cam,
                            Err(e) => {
                                warn!("Camera busy/unavailable, retrying after 500ms... ({})", e);
                                std::thread::sleep(std::time::Duration::from_millis(500));
                                match cam_mgr.start_session() {
                                    Ok(cam) => cam,
                                    Err(e2) => {
                                        error!("Camera init permanently failed: {}", e2);
                                        // Sleep to prevent tight loop CPU spike if GDM retries rapidly
                                        std::thread::sleep(std::time::Duration::from_secs(2));
                                        return Err(e2);
                                    }
                                }
                            }
                        };
                        
                        active_cam.warmup(config_clone.camera.warmup_frames)?;
                        
                        // Capture Frame 0
                        let frame0 = active_cam.capture_frame()?;
                        // Send Frame 0 immediately for detection
                        let _ = tx_frame0.send(frame0.clone());
                        
                        // Capture Remaining Frames
                        let remaining = config_clone.camera.sequence_length - 1;
                        let rest = active_cam.capture_sequence(remaining, config_clone.camera.sequence_interval_ms)?;
                        
                        Ok((frame0, rest))
                    }).await;

                    // Handle JoinError and send result
                    let final_res = match res {
                        Ok(r) => r,
                        Err(e) => Err(anyhow::anyhow!("Task join error: {}", e)),
                    };
                    let _ = tx_full.send(final_res);
                });

                // 3. Adaptive Detection & Parallel Recognition
                
                // Step 3a: Wait for Frame 0
                let frame0 = match rx_frame0.await {
                    Ok(f) => f,
                    Err(_) => {
                        // If we didn't get Frame 0, check the full result for the error
                        match rx_full.await {
                            Ok(Err(e)) => error!("Camera error: {}", e),
                            _ => error!("Camera task failed or cancelled"),
                        }
                        return send_response(&mut stream, AuthResponse::Failure).await;
                    }
                };

                // Step 3b: Start Detection on Frame 0 (Async/Parallel)
                // Returns a tight face crop AND the bounding box so we can crop frames 1-N
                // by propagating the bbox (face doesn't move significantly in ~200ms).
                let detector_clone = detector.clone();
                let frame0_for_detect = frame0.clone();
                let detection_task = tokio::task::spawn_blocking(move || {
                    detector_clone.detect_with_bbox(&frame0_for_detect)
                });

                // Step 3c: Wait for Camera Task to Finish (Remaining frames)
                let (frame0_from_cam, remaining_frames) = match rx_full.await {
                    Ok(Ok(res)) => res,
                    Ok(Err(e)) => {
                        error!("Camera capture failed: {}", e);
                        return send_response(&mut stream, AuthResponse::Failure).await;
                    },
                    Err(_) => {
                        error!("Camera task panicked or dropped channel");
                        return send_response(&mut stream, AuthResponse::Failure).await;
                    }
                };

                // Reconstruct frames vector
                let mut frames = vec![frame0_from_cam];
                frames.extend(remaining_frames);

                // 2.5 Liveness Check (Performed after all frames captured)
                let liveness_passed = if config.security.require_liveness {
                    liveness::check_liveness(&frames)
                } else {
                    true
                };

                if !liveness_passed {
                    warn!("Liveness check failed for user {}", user);
                    rate_limiter.record_failure(&user);
                    AuditLogger::log_auth_attempt(&user, false, 0.0, false);
                    return send_response(&mut stream, AuthResponse::Failure).await;
                }

                // Step 3d: Await Detection Result
                let detection_result = detection_task.await;

                let mut crops = Vec::new();

                if let Ok(Ok(Some((face0, bbox0)))) = detection_result {
                    crops.push(face0);

                    // Step 3e: Crop frames 1-N using the propagated bbox from frame 0.
                    // Equivalent to face tracking: the face doesn't move more than a few pixels
                    // across the 200ms capture window, so the bbox is still accurate.
                    // This gives tight face crops (vs. 720×720 center squares) for all frames,
                    // dramatically improving ArcFace embedding quality.
                    for i in 1..frames.len() {
                        crops.push(detector.crop_from_bbox(&frames[i], &bbox0));
                    }
                } else {
                    warn!("No face detected in first frame. Aborting sequence.");
                    rate_limiter.record_failure(&user);
                    AuditLogger::log_auth_attempt(&user, false, 0.0, liveness_passed);
                    return send_response(&mut stream, AuthResponse::Failure).await;
                }

                // Step 4: Sequential Recognition in a single blocking thread.
                // The ONNX session is Mutex-locked — running 5 spawn_blocking tasks in parallel
                // just creates 5 idle threads competing for one lock (heap explosion, zero benefit).
                // One thread doing 5 sequential inferences is faster and uses 8MB stack instead of 40MB.
                let engine_for_rec = engine.clone();
                let recognition_results = tokio::task::spawn_blocking(move || {
                    crops.into_iter().map(|crop| engine_for_rec.get_embedding(&crop)).collect::<Vec<_>>()
                }).await.unwrap_or_default();

                let mut valid_matches = 0;
                let mut best_score = 0.0;

                for res in recognition_results {
                    if let Ok(current_embedding) = res {
                        // 5. Compare with stored embeddings
                        let mut max_score = 0.0;
                        for stored_emb in &profile.embeddings {
                            let score = engine.compare(&current_embedding, stored_emb);
                            if score > max_score {
                                max_score = score;
                            }
                        }
                        
                        if max_score > best_score {
                            best_score = max_score;
                        }

                        info!("Match score for {}: {}", user, max_score);
                        
                        // Threshold from config
                        if max_score > config.recognition.match_threshold {
                            valid_matches += 1;
                        }
                    }
                }

                // Consensus: Need at least 3 matches out of 5 frames (or > 50%)
                let required_matches = (config.camera.sequence_length / 2) + 1;
                info!("Consensus result: {}/{} valid matches (required: {})", valid_matches, config.camera.sequence_length, required_matches);
                
                if valid_matches >= required_matches {
                    info!("Authentication SUCCESS for user {}", user);
                    rate_limiter.reset(&user);
                    AuditLogger::log_auth_attempt(&user, true, best_score, liveness_passed);
                    AuthResponse::Success
                } else {
                    warn!("Authentication FAILED for user {}. Not enough matches.", user);
                    rate_limiter.record_failure(&user);
                    AuditLogger::log_auth_attempt(&user, false, best_score, liveness_passed);
                    AuthResponse::Failure
                }
            }
        };

        send_response(&mut stream, response).await?;
    }
}

async fn send_response(stream: &mut UnixStream, response: AuthResponse) -> Result<()> {
    let response_data = serde_json::to_vec(&response)?;
    let len = response_data.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&response_data).await?;
    Ok(())
}


