use clap::{Parser, Subcommand};
use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use anyhow::{Result, Context};

#[derive(Parser)]
#[command(name = "faceauth")]
#[command(about = "CLI for FaceAuth system", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ping the daemon to check connectivity
    Ping,
    /// Enroll a new face for the current user
    Enroll {
        #[arg(short, long)]
        name: String,
    },
    /// List all enrolled users and their face counts
    List,
    /// Test authentication for a user
    Test {
        #[arg(short, long)]
        user: String,
    },
    /// Run system diagnostics
    Doctor,
    /// Run performance benchmark
    Benchmark,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Benchmark => {
            println!("Running FaceAuth Benchmark...");
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            
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
                Ok(_) => println!("[OK]"),
                Err(_) => println!("[ERROR] Could not connect to {}. Is the service running?", SOCKET_PATH),
            }

            // 2. Check Camera (via Daemon if possible, or local check)
            // For now, we just check if /dev/video0 exists
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
            
            send_request(&mut stream, &AuthRequest::Ping)?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::Pong => println!("pong"),
                _ => println!("Unexpected response: {:?}", response),
            }
        }
        Commands::Enroll { name } => {
            println!("NOTE: CLI enrollment is deprecated. Please use the GUI for better results.");
            println!("Run: faceauth-gui");
            
            let user = std::env::var("USER").context("Could not determine current user")?;
            println!("Enrolling face for user: {} ({})", user, name);
            
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            
            send_request(&mut stream, &AuthRequest::Enroll { user, name })?;
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::EnrollmentStatus { message, progress } => {
                    if progress >= 1.0 {
                        println!("Success: {}", message);
                    } else {
                        println!("Failed: {}", message);
                    }
                },
                _ => println!("Unexpected response: {:?}", response),
            }
        }
        Commands::Test { user } => {
            println!("Testing authentication for user: {}", user);
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            
            send_request(&mut stream, &AuthRequest::Authenticate { user: user.clone() })?;
            
            println!("Waiting for authentication result (look at camera)...");
            let response = read_response(&mut stream)?;

            match response {
                AuthResponse::Success => println!("Authentication SUCCESS!"),
                AuthResponse::Failure => println!("Authentication FAILED."),
                _ => println!("Unexpected response: {:?}", response),
            }
        }
        Commands::List => {
            let mut stream = UnixStream::connect(SOCKET_PATH)
                .context("Failed to connect to daemon. Is it running?")?;
            
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

