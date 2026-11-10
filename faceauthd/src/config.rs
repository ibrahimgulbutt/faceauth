use anyhow::{Context, Result};
use log::{info, warn};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG_PATH: &str = "/etc/faceauth/config.toml";
const DEFAULT_MODELS_DIR: &str = "/usr/share/faceauth/models";
const DEFAULT_STATE_DIR: &str = "/var/lib/faceauth";

/// Daemon configuration. Every section and key is optional; missing values
/// fall back to the defaults below (which match `dist/config.toml`).
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct Config {
    pub camera: CameraConfig,
    pub detection: DetectionConfig,
    pub recognition: RecognitionConfig,
    pub security: SecurityConfig,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct CameraConfig {
    /// `"auto"` (first colour camera), `"ir"` (first infrared camera), or a
    /// device path such as `/dev/video2` or `/dev/v4l/by-id/...`.
    pub device: String,
    pub warmup_frames: usize,
    pub sequence_length: usize,
    pub sequence_interval_ms: u64,
    /// Vendor commands that switch on an IR camera's emitter, sent in order
    /// each time the stream starts. Only needed when the IR light stays off.
    /// Accepts one `[camera.ir_emitter]` table or several `[[camera.ir_emitter]]`.
    #[serde(deserialize_with = "one_or_many")]
    pub ir_emitter: Vec<IrEmitter>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<IrEmitter>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(IrEmitter),
        Many(Vec<IrEmitter>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(e) => vec![e],
        OneOrMany::Many(v) => v,
    })
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self { device: "auto".into(), warmup_frames: 8, sequence_length: 5, sequence_interval_ms: 40, ir_emitter: Vec::new() }
    }
}

/// A UVC extension-unit control: `unit`, `selector` and the bytes to set, as
/// reported by linux-enable-ir-emitter.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct IrEmitter {
    pub unit: u8,
    pub selector: u8,
    pub control: Vec<u8>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct DetectionConfig {
    pub confidence_threshold: f32,
    /// Minimum face width in camera pixels for an enrollment sample.
    pub min_face_size: u32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self { confidence_threshold: 0.5, min_face_size: 80 }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct RecognitionConfig {
    pub match_threshold: f32,
    pub model: RecognitionModel,
    /// CPU threads for one embedding. 0 = choose automatically.
    pub threads: usize,
}

impl Default for RecognitionConfig {
    fn default() -> Self {
        Self { match_threshold: 0.55, model: RecognitionModel::Accurate, threads: 0 }
    }
}

impl RecognitionConfig {
    /// The fast model is cheap enough for one thread; the accurate one gets a
    /// few so a login stays around a second, without taking every core.
    pub fn effective_threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        match self.model {
            RecognitionModel::Fast => 1,
            RecognitionModel::Accurate => {
                let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
                (cores / 2).clamp(1, 4)
            }
        }
    }
}

/// Which face recognizer to use. Changing it requires everyone to set up face
/// unlock again (embeddings from different models are not comparable).
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecognitionModel {
    /// MobileFaceNet (InsightFace buffalo_s/sc), ~4 MB, very fast.
    Fast,
    /// ResNet50 (InsightFace buffalo_l/m), ~170 MB, much better at telling
    /// similar faces apart.
    Accurate,
}

impl RecognitionModel {
    pub fn file_stem(self) -> &'static str {
        match self {
            RecognitionModel::Fast => "arcface",
            RecognitionModel::Accurate => "arcface_r50",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RecognitionModel::Fast => "fast",
            RecognitionModel::Accurate => "accurate",
        }
    }

    pub fn other(self) -> Self {
        match self {
            RecognitionModel::Fast => RecognitionModel::Accurate,
            RecognitionModel::Accurate => RecognitionModel::Fast,
        }
    }

    /// Human-readable description for status output.
    pub fn describe(self) -> &'static str {
        match self {
            RecognitionModel::Fast => "Fast (MobileFaceNet)",
            RecognitionModel::Accurate => "Accurate (ResNet50)",
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct SecurityConfig {
    pub require_liveness: bool,
    pub max_attempts: u32,
    pub lockout_seconds: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self { require_liveness: true, max_attempts: 3, lockout_seconds: 60 }
    }
}

impl Config {
    /// Load `/etc/faceauth/config.toml` (or `$FACEAUTH_CONFIG`). Only this one
    /// path is read: a root-started daemon must not pick up a config from its
    /// working directory.
    pub fn load() -> Result<Self> {
        let path = env_path("FACEAUTH_CONFIG", DEFAULT_CONFIG_PATH);
        let config = if path.exists() {
            info!("Loading config from {}", path.display());
            let content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            toml::from_str(&content).with_context(|| format!("Invalid config in {}", path.display()))?
        } else {
            info!("No config file at {}, using defaults", path.display());
            Config::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Reject values that would crash the daemon or silently accept any face.
    fn validate(&self) -> Result<()> {
        let t = self.recognition.match_threshold;
        if !(t > 0.0 && t <= 1.0) {
            anyhow::bail!("recognition.match_threshold must be in (0, 1], got {t}");
        }
        if t < 0.35 {
            warn!("recognition.match_threshold = {t} is very permissive; other people may be accepted");
        }
        let d = self.detection.confidence_threshold;
        if !(d > 0.0 && d < 1.0) {
            anyhow::bail!("detection.confidence_threshold must be in (0, 1), got {d}");
        }
        if !(1..=30).contains(&self.camera.sequence_length) {
            anyhow::bail!("camera.sequence_length must be between 1 and 30");
        }
        if self.security.require_liveness && self.camera.sequence_length < 2 {
            anyhow::bail!("camera.sequence_length must be at least 2 when require_liveness = true");
        }
        if self.camera.warmup_frames > 60 {
            anyhow::bail!("camera.warmup_frames must be at most 60");
        }
        if self.camera.ir_emitter.len() > 16 {
            anyhow::bail!("camera.ir_emitter can have at most 16 controls");
        }
        if self.camera.ir_emitter.iter().any(|e| e.control.is_empty() || e.control.len() > 64) {
            anyhow::bail!("camera.ir_emitter.control must have 1 to 64 bytes");
        }
        if self.recognition.threads > 16 {
            anyhow::bail!("recognition.threads must be between 0 (automatic) and 16");
        }
        if self.security.max_attempts == 0 {
            anyhow::bail!("security.max_attempts must be at least 1");
        }
        if !(32..=1000).contains(&self.detection.min_face_size) {
            anyhow::bail!("detection.min_face_size must be between 32 and 1000 pixels");
        }
        Ok(())
    }
}

pub fn models_dir() -> PathBuf {
    env_path("FACEAUTH_MODELS_DIR", DEFAULT_MODELS_DIR)
}

pub fn state_dir() -> PathBuf {
    env_path("FACEAUTH_STATE_DIR", DEFAULT_STATE_DIR)
}

pub fn socket_path() -> PathBuf {
    env_path("FACEAUTH_SOCKET", faceauth_core::SOCKET_PATH)
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| Path::new(default).to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("[camera]\ndevice = \"/dev/video2\"\n").unwrap();
        assert_eq!(cfg.camera.device, "/dev/video2");
        assert_eq!(cfg.camera.sequence_length, 5);
        assert_eq!(cfg.recognition.match_threshold, 0.55);
        cfg.validate().unwrap();
    }

    #[test]
    fn legacy_config_still_loads() {
        let legacy = "[detection]\nconfidence_threshold = 0.3\nmin_face_size = 80\n\
                      [recognition]\nmatch_threshold = 0.55\nstrong_match_threshold = 0.65\nweak_match_threshold = 0.45\n\
                      [camera]\nwarmup_frames = 8\nsequence_length = 5\nsequence_interval_ms = 40\n";
        let cfg: Config = toml::from_str(legacy).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn parses_recognition_model() {
        let cfg: Config = toml::from_str("[recognition]\nmodel = \"fast\"\n").unwrap();
        assert_eq!(cfg.recognition.model, RecognitionModel::Fast);
        assert_eq!(cfg.recognition.effective_threads(), 1);
        assert_eq!(Config::default().recognition.model, RecognitionModel::Accurate);
        assert!(toml::from_str::<Config>("[recognition]\nmodel = \"huge\"\n").is_err());
    }

    #[test]
    fn parses_ir_settings() {
        let cfg: Config = toml::from_str(
            "[camera]\ndevice = \"ir\"\n[camera.ir_emitter]\nunit = 14\nselector = 6\ncontrol = [1, 3, 3, 0, 0, 0, 0, 0, 0]\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let e = &cfg.camera.ir_emitter[0];
        assert_eq!((e.unit, e.selector, e.control.len()), (14, 6, 9));
        // linux-enable-ir-emitter can need several controls.
        let many: Config = toml::from_str(
            "[[camera.ir_emitter]]\nunit = 4\nselector = 2\ncontrol = [1]\n\
             [[camera.ir_emitter]]\nunit = 7\nselector = 5\ncontrol = [3, 0]\n",
        )
        .unwrap();
        many.validate().unwrap();
        assert_eq!(many.camera.ir_emitter.len(), 2);
        let empty: Config = toml::from_str("[camera.ir_emitter]\nunit = 1\nselector = 1\ncontrol = []\n").unwrap();
        assert!(empty.validate().is_err());
    }

    #[test]
    fn rejects_unsafe_values() {
        for bad in [
            "[recognition]\nmatch_threshold = -1.0",
            "[recognition]\nmatch_threshold = 0.0",
            "[camera]\nsequence_length = 0",
            "[camera]\nsequence_length = 1",
            "[security]\nmax_attempts = 0",
        ] {
            let cfg: Config = toml::from_str(bad).unwrap();
            assert!(cfg.validate().is_err(), "{bad} should be rejected");
        }
    }
}
