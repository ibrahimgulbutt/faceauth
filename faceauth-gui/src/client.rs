//! Talks to faceauthd on worker threads and hands results to the UI thread
//! through async channels. Nothing here touches GTK.

use faceauth_core::{
    client_socket_path, recv_message, send_message, AuthOutcome, EnrollEvent, EnrollMode, FaceBox, Hint, Pose,
    Request, Response, UserStatus,
};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn connect(read_timeout: Duration) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(client_socket_path()).map_err(|_| "The FaceAuth service isn't running".to_string())?;
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    Ok(stream)
}

fn describe(r: Response) -> String {
    match r {
        Response::Denied { reason } => format!("Not allowed: {reason}"),
        Response::Error { message } => message,
        Response::Enroll(EnrollEvent::Failed(f)) => f.message(),
        other => format!("Unexpected reply: {other:?}"),
    }
}

/// Run `f` on a thread and deliver its single result.
fn one_shot<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> async_channel::Receiver<T> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(f());
    });
    rx
}

pub fn status() -> async_channel::Receiver<Result<UserStatus, String>> {
    if crate::demo::mode().is_some() {
        return one_shot(crate::demo::status);
    }
    one_shot(|| {
        let mut s = connect(Duration::from_secs(5))?;
        send_message(&mut s, &Request::Status).map_err(|e| e.to_string())?;
        match recv_message(&mut s).map_err(|e| e.to_string())? {
            Response::Status(st) => Ok(st),
            other => Err(describe(other)),
        }
    })
}

pub fn remove() -> async_channel::Receiver<Result<(), String>> {
    if crate::demo::mode().is_some() {
        return one_shot(|| Ok(()));
    }
    one_shot(|| {
        // The password dialog can take a while.
        let mut s = connect(Duration::from_secs(150))?;
        send_message(&mut s, &Request::Remove { user: None }).map_err(|e| e.to_string())?;
        match recv_message(&mut s).map_err(|e| e.to_string())? {
            Response::Removed => Ok(()),
            other => Err(describe(other)),
        }
    })
}

pub enum TestEvent {
    Scanning,
    Outcome(AuthOutcome),
    Failed(String),
}

pub fn test(user: String) -> async_channel::Receiver<TestEvent> {
    let (tx, rx) = async_channel::bounded(4);
    if crate::demo::mode().is_some() {
        std::thread::spawn(move || crate::demo::test(tx));
        return rx;
    }
    std::thread::spawn(move || {
        let fail = |e: String| TestEvent::Failed(e);
        let mut s = match connect(Duration::from_secs(20)) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send_blocking(fail(e));
                return;
            }
        };
        if let Err(e) = send_message(&mut s, &Request::Authenticate { user }) {
            let _ = tx.send_blocking(fail(e.to_string()));
            return;
        }
        loop {
            let event = match recv_message::<_, Response>(&mut s) {
                Ok(Response::Scanning) => TestEvent::Scanning,
                Ok(Response::Auth(o)) => TestEvent::Outcome(o),
                Ok(other) => fail(describe(other)),
                Err(e) => fail(e.to_string()),
            };
            let last = !matches!(event, TestEvent::Scanning);
            if tx.send_blocking(event).is_err() || last {
                return;
            }
        }
    });
    rx
}

/// A decoded preview frame in cairo's RGB24 layout (B, G, R, unused).
pub struct Frame {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub face: Option<FaceBox>,
}

pub enum SessionEvent {
    Authorizing,
    Started(Vec<Pose>),
    Frame(Frame),
    Progress { collected: usize, target: usize, pose: Pose, hint: Hint },
    Completed(usize),
    Failed(String),
}

/// A running enrollment. Dropping or cancelling it closes the connection,
/// which makes the daemon stop the camera and discard the samples.
pub struct Session {
    stream: Option<UnixStream>,
    demo: Option<async_channel::Receiver<SessionEvent>>,
}

impl Session {
    pub fn cancel(&self) {
        if let Some(s) = &self.stream {
            let _ = s.shutdown(Shutdown::Both);
        }
        if let Some(d) = &self.demo {
            d.close();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn enroll(mode: EnrollMode) -> Result<(Session, async_channel::Receiver<SessionEvent>), String> {
    if crate::demo::mode().is_some() {
        let (tx, rx) = async_channel::bounded(8);
        let steps = match mode {
            EnrollMode::Replace => vec![Pose::Center, Pose::Center, Pose::Left, Pose::Left, Pose::Right, Pose::Right, Pose::Up, Pose::Up, Pose::Down, Pose::Down],
            EnrollMode::Append => vec![Pose::Center, Pose::Center, Pose::Left, Pose::Right, Pose::Up, Pose::Down],
        };
        let stop_at = std::env::var("FACEAUTH_GUI_DEMO_HOLD").ok().and_then(|v| v.parse().ok());
        std::thread::spawn(move || crate::demo::enroll(tx, steps, stop_at));
        return Ok((Session { stream: None, demo: Some(rx.clone()) }, rx));
    }
    // Long timeout: the password dialog may stay open for a while.
    let mut stream = connect(Duration::from_secs(150))?;
    send_message(&mut stream, &Request::Enroll { mode, user: None }).map_err(|e| e.to_string())?;
    let mut reader = stream.try_clone().map_err(|e| e.to_string())?;
    let (tx, rx) = async_channel::bounded(8);
    std::thread::spawn(move || loop {
        let event = match recv_message::<_, Response>(&mut reader) {
            Ok(Response::Enroll(EnrollEvent::Frame { jpeg, face, .. })) => match decode(&jpeg, face) {
                Some(frame) => {
                    // Drop frames rather than queue them if the UI is behind.
                    if let Err(async_channel::TrySendError::Closed(_)) = tx.try_send(SessionEvent::Frame(frame)) {
                        return;
                    }
                    continue;
                }
                None => continue,
            },
            Ok(Response::Enroll(EnrollEvent::Authorizing)) => SessionEvent::Authorizing,
            Ok(Response::Enroll(EnrollEvent::Started { steps })) => SessionEvent::Started(steps),
            Ok(Response::Enroll(EnrollEvent::Progress { collected, target, pose, hint })) => {
                SessionEvent::Progress { collected, target, pose, hint }
            }
            Ok(Response::Enroll(EnrollEvent::Completed { samples })) => SessionEvent::Completed(samples),
            Ok(other) => SessionEvent::Failed(describe(other)),
            // A cancelled session ends here too; the UI has already moved on.
            Err(_) => SessionEvent::Failed("Lost connection to the FaceAuth service".into()),
        };
        let last = matches!(event, SessionEvent::Completed(_) | SessionEvent::Failed(_));
        if tx.send_blocking(event).is_err() || last {
            return;
        }
    });
    Ok((Session { stream: Some(stream), demo: None }, rx))
}

fn decode(jpeg: &[u8], face: Option<FaceBox>) -> Option<Frame> {
    let rgb = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
    let (w, h) = rgb.dimensions();
    let stride = gtk4::cairo::Format::Rgb24.stride_for_width(w).ok()?;
    let mut data = vec![0u8; stride as usize * h as usize];
    for (y, row) in rgb.as_raw().chunks_exact(w as usize * 3).enumerate() {
        let out = &mut data[y * stride as usize..];
        for (x, px) in row.chunks_exact(3).enumerate() {
            out[x * 4] = px[2];
            out[x * 4 + 1] = px[1];
            out[x * 4 + 2] = px[0];
        }
    }
    Some(Frame { data, width: w as i32, height: h as i32, stride, face })
}
