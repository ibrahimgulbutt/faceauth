use serde::Deserialize;
use std::fs;
use std::path::Path;
use anyhow::Result;
use log::info;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    pub detection: DetectionConfig,
    pub recognition: RecognitionConfig,
    pub camera: CameraConfig,
    #[serde(default)]
    pub security: SecurityConfig,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DetectionConfig {
    pub confidence_threshold: f32,
    pub min_face_size: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RecognitionConfig {
    pub match_threshold: f32,
    pub strong_match_threshold: f32,
    pub weak_match_threshold: f32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct CameraConfig {
    pub warmup_frames: usize,
    pub sequence_length: usize,
    pub sequence_interval_ms: u64,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SecurityConfig {
    pub require_liveness: bool,
    pub max_attempts: u32,
    pub lockout_seconds: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            require_liveness: true,
            max_attempts: 3,
            lockout_seconds: 60,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            detection: DetectionConfig {
                confidence_threshold: 0.4,
                min_face_size: 64,
            },
            recognition: RecognitionConfig {
                match_threshold: 0.40,
                strong_match_threshold: 0.55,
                weak_match_threshold: 0.35,
            },
            camera: CameraConfig {
                warmup_frames: 5,   // 5 frames needed after sleep for auto-exposure to settle
                sequence_length: 5,
                sequence_interval_ms: 40,
            },
            security: SecurityConfig {
                require_liveness: true,
                max_attempts: 3,
                lockout_seconds: 60,
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let paths = [
            "/etc/faceauth/config.toml",
            "config.toml",
        ];

        for path in paths {
            if Path::new(path).exists() {
                info!("Loading config from {}", path);
                let content = fs::read_to_string(path)?;
                let config: Config = toml::from_str(&content)?;
                return Ok(config);
            }
        }

        info!("No config file found, using defaults");
        Ok(Config::default())
    }
}
