//! Wire protocol shared by `faceauthd`, the PAM module, the CLI and the GUI.
//!
//! Every message is a `u32` big-endian length followed by that many bytes of
//! JSON. Requests that act on a user are authorised by the daemon from the
//! caller's kernel-verified uid (`SO_PEERCRED`), never from the username in the
//! request alone. See `docs/DESIGN.md`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub mod pam;

/// FaceAuth release version (all crates share the workspace version).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Bumped whenever a message changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 2;

/// Directory holding the daemon socket. Created by systemd's
/// `RuntimeDirectory=faceauth` and owned by the daemon user, so nobody else can
/// create or swap the socket.
pub const SOCKET_DIR: &str = "/run/faceauth";
pub const SOCKET_PATH: &str = "/run/faceauth/faceauth.sock";
/// System account the daemon runs as. The PAM module trusts a daemon running
/// as this user or as root, and nothing else.
pub const DAEMON_USER: &str = "faceauth";
/// Polkit action guarding enrollment changes.
pub const POLKIT_ACTION: &str = "org.faceauth.manage";

/// Socket path for the CLI and GUI. `FACEAUTH_SOCKET` points them at a
/// development daemon. The PAM module never honours it and always uses
/// [`SOCKET_PATH`].
pub fn client_socket_path() -> PathBuf {
    std::env::var_os("FACEAUTH_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(SOCKET_PATH))
}

/// Upper bound for one framed message. The largest legitimate message is a
/// JPEG preview frame (well under 200 KB). Larger length prefixes are rejected
/// before allocating.
pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Most samples one profile may hold. More samples raise the chance that some
/// stored sample happens to resemble another person.
pub const MAX_SAMPLES: usize = 40;

// ─── Requests ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Ping,
    /// Enrollment status of the calling user.
    Status,
    /// Root (PAM) or the user themselves. Answered with `Scanning`, then `Auth`.
    Authenticate { user: String },
    /// Start an enrollment session. `user` must be `None` (or the caller) for
    /// non-root callers. Answered with a stream of `Enroll` events.
    Enroll { mode: EnrollMode, user: Option<String> },
    /// Delete face data. `user` as for `Enroll`.
    Remove { user: Option<String> },
    /// Root sees everyone; other callers see only themselves.
    ListEnrolled,
    Cameras,
    Benchmark,
    /// Follow the caller's own face checks (for the desktop's face
    /// animation). Answered with `Watching`, then one `AuthEvent` per event
    /// for the caller's account, until the client disconnects.
    Watch,
    /// Root only (`sudo faceauth ir-setup`, run by the installer): find the
    /// vendor command that switches on the infrared camera's light. Answered
    /// with a stream of `IrSetup` events ending in `Found`, `NotFound` or
    /// `Failed`.
    SetupIrEmitter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnrollMode {
    /// First setup, or "set up again". Replaces existing samples on success.
    Replace,
    /// "Improve recognition". Adds samples to the existing profile.
    Append,
}

// ─── Responses ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Pong { version: String, protocol: u32 },
    Status(UserStatus),
    /// Sent once the camera is about to capture for an `Authenticate`.
    Scanning,
    Auth(AuthOutcome),
    Enroll(EnrollEvent),
    Removed,
    Users(Vec<EnrolledUser>),
    Cameras(Vec<CameraDevice>),
    Benchmark(BenchmarkReport),
    Watching,
    AuthEvent(AuthEvent),
    IrSetup(IrSetupEvent),
    /// The caller is not allowed to do this.
    Denied { reason: String },
    Error { message: String },
}

/// A face check for the watching user, from whatever asked for it (sudo, the
/// lock screen, an admin prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthEvent {
    /// The camera is looking.
    Scanning,
    /// The check ended (also sent for checks that never reached the camera).
    Finished(AuthOutcome),
}

/// A UVC extension-unit control value: what switches an IR light on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmitterControl {
    pub unit: u8,
    pub selector: u8,
    pub control: Vec<u8>,
}

/// Progress of the automatic search for the IR light switch. Brightness is
/// the face's (or, with no face in view, the picture centre's), 0–255.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IrSetupEvent {
    Started { total: usize, brightness: f32 },
    Progress { tried: usize, total: usize },
    /// The light works. `controls` switch it on; empty when it was on already.
    Found { controls: Vec<EmitterControl>, before: f32, after: f32 },
    NotFound,
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserStatus {
    pub user: String,
    pub enrolled: bool,
    pub samples: usize,
    pub max_samples: usize,
    /// Unix seconds; 0 when unknown.
    pub created: u64,
    pub updated: u64,
    /// False when the profile was made with a different recognition model
    /// or kind of camera (colour vs infrared), so it can't be used.
    pub model_current: bool,
    /// Human-readable name of the camera the daemon will use.
    pub camera: Option<String>,
    /// The camera in use is infrared.
    #[serde(default)]
    pub infrared: bool,
    /// Recognizer in use, e.g. "Accurate (ResNet50)".
    pub recognizer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrolledUser {
    pub user: String,
    pub samples: usize,
    pub updated: u64,
    pub model_current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthOutcome {
    Matched,
    NotMatched,
    /// Frames looked like a static image.
    NotLive,
    NoFace,
    BadLighting,
    LockedOut { seconds: u64 },
    NotEnrolled,
    /// Enrolled with a different recognition model; set up again.
    ModelChanged,
    CameraUnavailable,
    /// Another authentication or an enrollment change is in progress.
    Busy,
}

impl AuthOutcome {
    /// Short sentence shown by PAM, CLI and GUI.
    pub fn message(&self) -> String {
        match self {
            AuthOutcome::Matched => "Face recognized".into(),
            AuthOutcome::NotMatched => "Face not recognized".into(),
            AuthOutcome::NotLive => "Face not recognized (looked like a photo)".into(),
            AuthOutcome::NoFace => "No face found".into(),
            AuthOutcome::BadLighting => "Too dark or too bright to see your face".into(),
            AuthOutcome::LockedOut { seconds } => {
                format!("Too many attempts. Try again in {seconds} s")
            }
            AuthOutcome::NotEnrolled => "Face unlock is not set up".into(),
            AuthOutcome::ModelChanged => {
                "Face unlock needs to be set up again (the camera or face model changed)".into()
            }
            AuthOutcome::CameraUnavailable => "Camera is unavailable".into(),
            AuthOutcome::Busy => "Face unlock is busy".into(),
        }
    }
}

// ─── Enrollment events ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pose {
    Center,
    Left,
    Right,
    Up,
    Down,
}

impl Pose {
    pub fn instruction(&self) -> &'static str {
        match self {
            Pose::Center => "Look straight at the camera",
            Pose::Left => "Slowly turn your head left",
            Pose::Right => "Slowly turn your head right",
            Pose::Up => "Tilt your chin up a little",
            Pose::Down => "Tilt your chin down a little",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Pose::Center => "Straight",
            Pose::Left => "Left",
            Pose::Right => "Right",
            Pose::Up => "Up",
            Pose::Down => "Down",
        }
    }
}

/// Why the current frame was not taken as a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hint {
    /// Frame is fine (or a sample was just taken).
    Good,
    NoFace,
    MultipleFaces,
    MoveCloser,
    LookStraight,
    TooDark,
    TooBright,
    TurnMore,
    TooFar,
    MoveSlightly,
    DifferentPerson,
    /// Infrared camera, face nearly black: its IR light is not on.
    IrLightOff,
}

impl Hint {
    pub fn message(&self) -> &'static str {
        match self {
            Hint::Good => "Hold that",
            Hint::NoFace => "Looking for your face…",
            Hint::MultipleFaces => "Make sure only you are in view",
            Hint::MoveCloser => "Move a little closer",
            Hint::LookStraight => "Face the camera directly",
            Hint::TooDark => "It's too dark — face a light",
            Hint::TooBright => "Too much light — avoid bright light behind you",
            Hint::TurnMore => "A little more",
            Hint::TooFar => "Not quite so far",
            Hint::MoveSlightly => "Move your head slightly",
            Hint::DifferentPerson => "Only the account owner should be in view",
            Hint::IrLightOff => "The infrared light seems to be off — see “Infrared cameras” in the README",
        }
    }

    /// Whether the hint means something is wrong (shown as a warning).
    pub fn is_problem(&self) -> bool {
        !matches!(self, Hint::Good | Hint::TurnMore | Hint::MoveSlightly | Hint::LookStraight)
    }
}

/// Face position in the (mirrored) preview, normalised to 0..1.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FaceBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnrollEvent {
    /// Waiting for the user to confirm with their password.
    Authorizing,
    /// Capture started. `steps[i]` is the pose wanted for sample `i`.
    Started { steps: Vec<Pose> },
    /// Mirrored preview frame.
    Frame {
        #[serde(with = "base64_bytes")]
        jpeg: Vec<u8>,
        width: u32,
        height: u32,
        face: Option<FaceBox>,
    },
    Progress {
        collected: usize,
        target: usize,
        pose: Pose,
        hint: Hint,
    },
    Completed { samples: usize },
    Failed(EnrollFailure),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnrollFailure {
    NotAuthorized,
    AuthCancelled,
    NoAuthAgent,
    CameraUnavailable,
    TimedOut,
    Full { max: usize },
    NotEnrolled,
    /// Another setup or removal for this user is already running.
    InProgress,
    /// Improving: the face in view never matched the saved face data (made
    /// with another model or camera, or someone else is in view).
    DoesNotMatch,
    Internal { message: String },
}

impl EnrollFailure {
    pub fn message(&self) -> String {
        match self {
            EnrollFailure::NotAuthorized => "You are not allowed to change face unlock".into(),
            EnrollFailure::AuthCancelled => "Authentication was cancelled".into(),
            EnrollFailure::NoAuthAgent => {
                "No password prompt is available. Start a polkit authentication agent, \
                 or run `sudo faceauth enroll --user $USER`"
                    .into()
            }
            EnrollFailure::CameraUnavailable => "The camera is unavailable or in use".into(),
            EnrollFailure::TimedOut => "Setup took too long. Try again".into(),
            EnrollFailure::Full { max } => {
                format!("You already have the maximum of {max} samples. Set up again to start fresh")
            }
            EnrollFailure::NotEnrolled => "Face unlock is not set up yet".into(),
            EnrollFailure::InProgress => "Face unlock setup is already running".into(),
            EnrollFailure::DoesNotMatch => {
                "This face doesn't match your saved face data. Set up again to start fresh".into()
            }
            EnrollFailure::Internal { message } => format!("Something went wrong: {message}"),
        }
    }
}

// ─── Misc payloads ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraDevice {
    pub path: String,
    pub name: String,
    pub formats: Vec<String>,
    /// Likely an infrared camera (name mentions IR, or greyscale only).
    pub infrared: bool,
    /// The device the daemon will use with the current config.
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub camera: String,
    pub recognizer: String,
    pub resolution: String,
    pub capture_ms: f32,
    pub detection_ms: f32,
    pub recognition_ms: f32,
    pub total_ms: f32,
    /// Mean brightness (0–255) of the face, if one was in view.
    #[serde(default)]
    pub face_brightness: Option<f32>,
    /// The camera is infrared.
    #[serde(default)]
    pub infrared: bool,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// True if `name` is safe to use as a single path component under the storage
/// directory. Usernames arrive from clients and are joined onto a path, so
/// anything that could escape it (`/`, `..`, absolute paths) must be rejected.
/// NSS/SSSD-style names such as `user@domain` are allowed.
pub fn is_safe_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '/' || c.is_control())
}

/// Write one length-prefixed JSON message.
pub fn send_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let data = encode_message(msg)?;
    w.write_all(&data)?;
    w.flush()
}

/// Serialize a message including its length prefix.
pub fn encode_message<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let json = serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if json.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "message exceeds MAX_MESSAGE_BYTES"));
    }
    let mut out = Vec::with_capacity(json.len() + 4);
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(&json);
    Ok(out)
}

/// Read one length-prefixed JSON message, refusing oversized length prefixes.
pub fn recv_message<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message exceeds MAX_MESSAGE_BYTES"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Serde helper: `Vec<u8>` as a base64 string (JSON arrays of numbers are
/// roughly 4× larger).
mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        STANDARD.decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GNOME Shell extension parses these by hand; keep them stable.
    #[test]
    fn watch_wire_format() {
        assert_eq!(serde_json::to_string(&Request::Watch).unwrap(), r#""Watch""#);
        assert_eq!(serde_json::to_string(&Response::Watching).unwrap(), r#""Watching""#);
        let ev = |e| serde_json::to_string(&Response::AuthEvent(e)).unwrap();
        assert_eq!(ev(AuthEvent::Scanning), r#"{"AuthEvent":"Scanning"}"#);
        assert_eq!(ev(AuthEvent::Finished(AuthOutcome::Matched)), r#"{"AuthEvent":{"Finished":"Matched"}}"#);
        assert_eq!(
            ev(AuthEvent::Finished(AuthOutcome::LockedOut { seconds: 5 })),
            r#"{"AuthEvent":{"Finished":{"LockedOut":{"seconds":5}}}}"#
        );
    }

    #[test]
    fn rejects_path_escapes() {
        for bad in ["", ".", "..", "/", "/etc", "../../etc", "a/b", "bob\n", "bob\0"] {
            assert!(!is_safe_username(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn accepts_real_usernames() {
        for ok in ["alice", "bob_2", "svc-backup", "user@corp.example", "Admin$"] {
            assert!(is_safe_username(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn round_trips_a_message() {
        let mut buf = Vec::new();
        send_message(&mut buf, &Request::Enroll { mode: EnrollMode::Append, user: None }).unwrap();
        let back: Request = recv_message(&mut buf.as_slice()).unwrap();
        assert!(matches!(back, Request::Enroll { mode: EnrollMode::Append, user: None }));
    }

    #[test]
    fn frames_carry_base64_jpeg() {
        let ev = Response::Enroll(EnrollEvent::Frame {
            jpeg: vec![0xff, 0xd8, 0x00, 0x10],
            width: 2,
            height: 2,
            face: None,
        });
        let bytes = encode_message(&ev).unwrap();
        assert!(String::from_utf8_lossy(&bytes[4..]).contains("\"/9gAEA==\""));
        let back: Response = recv_message(&mut bytes.as_slice()).unwrap();
        match back {
            Response::Enroll(EnrollEvent::Frame { jpeg, .. }) => assert_eq!(jpeg, vec![0xff, 0xd8, 0x00, 0x10]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn refuses_oversized_length_prefix() {
        let frame = (u32::MAX).to_be_bytes();
        let err = recv_message::<_, Response>(&mut frame.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
