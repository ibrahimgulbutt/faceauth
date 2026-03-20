use clap::{Parser, Subcommand};
use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use anyhow::{Context, Result};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "faceauth")]
#[command(version)]
#[command(about = "FaceAuth — face authentication for Linux", long_about = "\
FaceAuth lets you log in to sudo, GDM, SDDM and LightDM with your face.

QUICK START
  1. Enroll your face (GUI):      faceauth-gui
  2. Test authentication:         faceauth test --user $USER
  3. Check system health:         faceauth doctor

The daemon (faceauthd) must be running. Check with:  faceauth ping
Service logs:  sudo journalctl -u faceauth.service -f")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check daemon connectivity (returns 'pong' if running)
    Ping,
    /// List all enrolled users and their face-sample counts
    List,
    /// Test face authentication for a user (opens the camera)
    Test {
        /// Username to authenticate (defaults to $USER if omitted)
        #[arg(short, long, default_value_t = std::env::var("USER").unwrap_or_default())]
        user: String,
    },
    /// Run system diagnostics (daemon, camera, PAM, models, config)
    Doctor,
    /// Measure detection and recognition pipeline latency
    Benchmark,
}

fn set_timeout(stream: &mut UnixStream, sec: u64) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(sec)))?;
    stream.set_write_timeout(Some(Duration::from_secs(sec)))?;
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Benchmark => {
            println!("Running FaceAuth Benchmark...");
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            set_timeout(&mut stream, 60)?;

            send_request(&mut stream, &AuthRequest::Benchmark)?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::BenchmarkResult { detection_ms, recognition_ms, capture_ms, total_ms } => {
                    println!("Benchmark Results:");
                    println!("  Capture Time:     {:.2} ms (Warmup + Sequence)", capture_ms);
                    println!("  Detection Time:   {:.2} ms", detection_ms);
                    println!("  Recognition Time: {:.2} ms", recognition_ms);
                    println!("  Total Pipeline:   {:.2} ms", total_ms);
                },
                _ => println!("Unexpected response: {:?}", response),
            }
        },
        Commands::Doctor => {
            println!("Running FaceAuth Doctor...");
            
            // 1. Check Daemon Connection
            print!("Checking Daemon connection... ");
            match UnixStream::connect(SOCKET_PATH) {
                Ok(mut stream) => {
                    if set_timeout(&mut stream, 2).is_ok() 
                       && send_request(&mut stream, &AuthRequest::Ping).is_ok() {
                        if let Ok(AuthResponse::Pong) = read_response(&mut stream) {
                            println!("[OK] (Pong received)");
                        } else {
                            println!("[WARNING] Connected but no PONG received (Timeout?)");
                        }
                    } else {
                         println!("[OK] (Connected)");
                    }
                },
                Err(_) => println!("[ERROR] Could not connect to {}. Is the service running?", SOCKET_PATH),
            }

            // 2. Check Camera
            print!("Checking Camera device... ");
            if std::path::Path::new("/dev/video0").exists() {
                println!("[OK] /dev/video0 found");
            } else {
                println!("[WARNING] /dev/video0 not found. (Libcamera might still work)");
            }

            // 3. Check PAM Config
            print!("Checking PAM configuration... ");
            let pam_path = std::path::Path::new("/etc/pam.d/sudo");
            if pam_path.exists() {
                let content = std::fs::read_to_string(pam_path).unwrap_or_default();
                if content.contains("pam_faceauth.so") {
                    println!("[OK] pam_faceauth.so found in sudo config");
                } else {
                    println!("[WARNING] pam_faceauth.so NOT found in /etc/pam.d/sudo");
                }
            } else {
                println!("[SKIP] /etc/pam.d/sudo not found");
            }

            // 4. Check Models
            print!("Checking Models... ");
            if std::path::Path::new("/usr/share/faceauth/models/arcface.onnx").exists() {
                println!("[OK] ArcFace model found");
            } else {
                println!("[ERROR] Models missing in /usr/share/faceauth/models/");
            }

            // 5. Check Config
            print!("Checking Configuration... ");
            if std::path::Path::new("/etc/faceauth/config.toml").exists() {
                println!("[OK] Config file found");
            } else {
                println!("[WARN] Config file missing, using defaults");
            }
        }
        Commands::Ping => {
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            set_timeout(&mut stream, 5)?;
            
            send_request(&mut stream, &AuthRequest::Ping)?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::Pong => println!("pong"),
                _ => println!("Unexpected response: {:?}", response),
            }
        }
        Commands::Test { user } => {
            println!("Testing face authentication for user: {}", user);
            println!("Look at your camera when it activates…");
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Cannot connect to daemon — is faceauthd running? Try: sudo systemctl start faceauth.service")?;
            set_timeout(&mut stream, 20)?;

            send_request(&mut stream, &AuthRequest::Authenticate { user: user.clone() })?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::Success => println!("✓  Authentication SUCCESS for '{}'", user),
                AuthResponse::Failure => println!("✗  Authentication FAILED for '{}'\n   Tip: re-enroll with more angles using faceauth-gui", user),
                _ => println!("Unexpected response: {:?}", response),
            }
        }
        Commands::List => {
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            set_timeout(&mut stream, 5)?;
            
            send_request(&mut stream, &AuthRequest::ListEnrolled)?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::EnrolledList(users) => {
                    if users.is_empty() {
                        println!("No users enrolled.");
                    } else {
                        println!("{:<20} | {:<10}", "User", "Faces");
                        println!("{:-<20}-|-{:-<10}", "", "");
                        for (user, count) in users {
                            println!("{:<20} | {:<10}", user, count);
                        }
                    }
                },
                _ => println!("Unexpected response: {:?}", response),
            }
        }
    }

    Ok(())
}

fn send_request(stream: &mut UnixStream, req: &AuthRequest) -> Result<()> {
    let data = serde_json::to_vec(req)?;
    let len = data.len() as u32;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&data)?;
    Ok(())
}

fn read_response(stream: &mut UnixStream) -> Result<AuthResponse> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    let resp: AuthResponse = serde_json::from_slice(&buf)?;
    Ok(resp)
}

