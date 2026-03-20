use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum AuthRequest {
    Authenticate { user: String },
    Enroll { user: String, name: String },
    EnrollSample { user: String, image_data: Vec<u8>, width: u32, height: u32 },
    DeleteUser { user: String },
    ListEnrolled,
    Ping,
    Benchmark,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AuthResponse {
    Success,
    Failure,
    Pong,
    EnrollmentStatus { message: String, progress: f32 },
    EnrolledList(Vec<(String, usize)>),
    BenchmarkResult { 
        detection_ms: f32, 
        recognition_ms: f32,
        capture_ms: f32,
        total_ms: f32 
    },
}

pub const SOCKET_PATH: &str = "/tmp/faceauth.sock";
