//! faceauthd — owns the camera and the face models, answers the PAM module,
//! the CLI and the GUI over a Unix socket. See docs/DESIGN.md.

mod auth;
mod camera;
mod config;
mod detection;
mod enroll;
mod liveness;
mod polkit;
mod recognition;
mod security;
mod storage;
mod users;

use anyhow::{Context, Result};
use auth::Pipeline;
use camera::CameraManager;
use config::Config;
use detection::FaceDetector;
use faceauth_core::{
    encode_message, is_safe_username, AuthEvent, AuthOutcome, EnrollEvent, EnrollFailure, EnrollMode, EnrolledUser, Request,
    Response, UserStatus, DAEMON_USER, MAX_MESSAGE_BYTES, MAX_SAMPLES, PROTOCOL_VERSION, SOCKET_DIR, VERSION,
};
use log::{error, info, warn};
use recognition::FaceEngine;
use security::{AuditLogger, ChangeGuard, RateLimiter};
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use storage::{SecureStorage, UserProfile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Semaphore};

/// `Watch` connections stay open, so they are capped separately: a few per
/// user (one per desktop session is normal) and a small total, leaving the
/// client slots for logins.
const MAX_WATCHERS: usize = 16;
const MAX_WATCHERS_PER_USER: usize = 4;
/// A connected client is dropped after this long without sending a request.
const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Concurrent connections beyond this are refused.
const MAX_CLIENTS: usize = 64;
/// How long an authentication waits for a running one to finish.
const AUTH_QUEUE_WAIT: Duration = Duration::from_secs(5);
/// How long to wait for the camera when it is in use.
const CAMERA_WAIT: Duration = Duration::from_millis(1500);

struct Daemon {
    config: Config,
    camera: Mutex<CameraManager>,
    camera_label: Mutex<Option<String>>,
    /// The camera is infrared (decided at startup).
    infrared: bool,
    detector: FaceDetector,
    engine: FaceEngine,
    storage: SecureStorage,
    limiter: RateLimiter,
    /// One face pipeline at a time: parallel ONNX runs just fight for the CPU.
    auth_gate: Semaphore,
    changes: ChangeGuard,
    /// Face-check events, tagged with the uid of the account being checked.
    events: broadcast::Sender<(u32, AuthEvent)>,
    watchers: Watchers,
}

impl Daemon {
    fn pipeline(&self) -> Pipeline<'_> {
        Pipeline { config: &self.config, detector: &self.detector, engine: &self.engine, infrared: self.infrared }
    }

    /// Tell the account's own watchers (its desktop session) about a face check.
    fn emit(&self, user: &str, event: AuthEvent) {
        if let Some(uid) = users::uid_of(user) {
            // No receivers is the normal case without a desktop watcher.
            let _ = self.events.send((uid, event));
        }
    }

    /// The profile was made with this recognizer and this kind of camera.
    fn profile_usable(&self, p: &UserProfile) -> bool {
        p.usable_with(&self.engine.model_id, self.infrared)
    }

    fn camera_label(&self) -> Option<String> {
        if let Some(label) = self.camera_label.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            return Some(label);
        }
        let label = self.camera.try_lock().ok()?.describe();
        *self.camera_label.lock().unwrap_or_else(|p| p.into_inner()) = label.clone();
        label
    }
}

/// Open `Watch` connections per uid.
#[derive(Default)]
struct Watchers {
    per_uid: Mutex<HashMap<u32, usize>>,
}

struct WatchSlot<'a> {
    owner: &'a Watchers,
    uid: u32,
}

impl Watchers {
    fn join(&self, uid: u32) -> Option<WatchSlot<'_>> {
        let mut map = self.per_uid.lock().unwrap_or_else(|p| p.into_inner());
        let total: usize = map.values().sum();
        let mine = map.get(&uid).copied().unwrap_or(0);
        if total >= MAX_WATCHERS || mine >= MAX_WATCHERS_PER_USER {
            return None;
        }
        *map.entry(uid).or_default() += 1;
        Some(WatchSlot { owner: self, uid })
    }
}

impl Drop for WatchSlot<'_> {
    fn drop(&mut self) {
        let mut map = self.owner.per_uid.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = map.get_mut(&self.uid) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.uid);
            }
        }
    }
}

/// The process on the other end of the socket, as reported by the kernel.
#[derive(Clone, Copy, Debug)]
struct Peer {
    uid: u32,
    pid: Option<i32>,
}

impl Peer {
    /// Root (PAM inside sudo/su/GDM/SDDM, polkit helpers) may act for anyone;
    /// others only for themselves (GUI, CLI, screen lockers running as the user).
    fn may_act_for(&self, user: &str) -> bool {
        self.uid == 0 || users::uid_of(user) == Some(self.uid)
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    info!("faceauthd {VERSION} starting");
    log_system_facts();

    let config = Config::load()?;
    info!("Configuration: {config:?}");
    let models = config::models_dir();
    let detector = FaceDetector::new(&models, config.detection.confidence_threshold)?;
    let engine = FaceEngine::new(&models, config.recognition.model, config.recognition.effective_threads())?;
    let storage = SecureStorage::new(config::state_dir())?;
    let camera = CameraManager::new(&config.camera.device, config.camera.ir_emitter.clone())?;
    let label = camera.describe();
    let infrared = camera.is_infrared();
    match &label {
        Some(l) => info!("Camera: {l}{}", if infrared { " [infrared]" } else { "" }),
        None => warn!("No camera found yet; will look again on first use"),
    }

    let daemon = Arc::new(Daemon {
        limiter: RateLimiter::new(config.security.max_attempts, config.security.lockout_seconds),
        config,
        camera: Mutex::new(camera),
        camera_label: Mutex::new(label),
        infrared,
        detector,
        engine,
        storage,
        auth_gate: Semaphore::new(1),
        changes: ChangeGuard::default(),
        events: broadcast::channel(16).0,
        watchers: Watchers::default(),
    });

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(serve(daemon))
}

async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let socket = config::socket_path();
    let listener = bind(&socket)?;
    info!("Listening on {}", socket.display());

    let me = users::name_of(users::effective_uid());
    if users::effective_uid() != 0 && me.as_deref() != Some(DAEMON_USER) {
        warn!("Running as {me:?}: development mode. The PAM module will not trust this daemon.");
    }

    let clients = Arc::new(Semaphore::new(MAX_CLIENTS));
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(s) => s,
                    Err(e) => { error!("accept failed: {e}"); continue; }
                };
                let peer = match stream.peer_cred() {
                    Ok(c) => Peer { uid: c.uid(), pid: c.pid() },
                    Err(e) => { warn!("Rejecting client without peer credentials: {e}"); continue; }
                };
                let Ok(permit) = clients.clone().try_acquire_owned() else {
                    warn!("Too many clients; refusing uid {}", peer.uid);
                    continue;
                };
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, peer, daemon).await {
                        warn!("Client uid {}: {e:#}", peer.uid);
                    }
                    drop(permit);
                });
            }
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    let _ = fs::remove_file(&socket);
    info!("faceauthd stopped");
    Ok(())
}

/// Bind the socket. The production directory is created by systemd
/// (`RuntimeDirectory=faceauth`) and owned by the daemon user; nobody else can
/// create or replace files in it.
fn bind(socket: &Path) -> Result<UnixListener> {
    let dir = socket.parent().context("socket path has no parent directory")?;
    if !dir.exists() {
        fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
        if dir == Path::new(SOCKET_DIR) {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
        }
    }
    match fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("Failed to remove stale {}", socket.display())),
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("Failed to bind {}", socket.display()))?;
    // Anyone may connect; what each caller may do is decided per request.
    fs::set_permissions(socket, fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

async fn handle_client(stream: UnixStream, peer: Peer, d: Arc<Daemon>) -> Result<()> {
    let (mut rd, mut wr) = stream.into_split();
    loop {
        let Some(request) = read_request(&mut rd, peer).await? else { return Ok(()) };
        match request {
            Request::Ping => send(&mut wr, &Response::Pong { version: VERSION.into(), protocol: PROTOCOL_VERSION }).await?,
            Request::Status => send(&mut wr, &status(&d, peer)).await?,
            Request::Authenticate { user } => authenticate(&d, peer, user, &mut wr).await?,
            Request::Enroll { mode, user } => enroll_session(&d, peer, mode, user, &mut rd, &mut wr).await?,
            Request::Remove { user } => remove(&d, peer, user, &mut rd, &mut wr).await?,
            Request::ListEnrolled => send(&mut wr, &list_enrolled(&d, peer)).await?,
            Request::Cameras => {
                let selected = d.camera.try_lock().ok().and_then(|c| c.selected_index());
                let devices = tokio::task::spawn_blocking(move || camera::describe_devices(selected)).await?;
                send(&mut wr, &Response::Cameras(devices)).await?
            }
            Request::Benchmark => send(&mut wr, &benchmark(&d).await).await?,
            Request::Watch => return watch(&d, peer, &mut rd, &mut wr).await,
        }
    }
}

/// Stream the caller's own face-check events until they disconnect. A user
/// only ever learns when their own account is being checked, and the result.
async fn watch(d: &Arc<Daemon>, peer: Peer, rd: &mut OwnedReadHalf, wr: &mut OwnedWriteHalf) -> Result<()> {
    let Some(_slot) = d.watchers.join(peer.uid) else {
        return send(wr, &Response::Denied { reason: "too many watchers".into() }).await;
    };
    let mut events = d.events.subscribe();
    send(wr, &Response::Watching).await?;
    let mut probe = [0u8; 1];
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok((uid, event)) if uid == peer.uid => send(wr, &Response::AuthEvent(event)).await?,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            // Watchers send nothing after `Watch`: EOF (or anything else) ends it.
            _ = rd.read(&mut probe) => return Ok(()),
        }
    }
}

async fn read_request(rd: &mut OwnedReadHalf, peer: Peer) -> Result<Option<Request>> {
    let mut len_buf = [0u8; 4];
    match tokio::time::timeout(CLIENT_IDLE_TIMEOUT, rd.read_exact(&mut len_buf)).await {
        Err(_) => return Ok(None),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Ok(Err(e)) => return Err(e.into()),
        Ok(Ok(_)) => {}
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MESSAGE_BYTES {
        warn!("Dropping uid {}: {len}-byte message exceeds the limit", peer.uid);
        return Ok(None);
    }
    let mut buf = vec![0u8; len];
    tokio::time::timeout(CLIENT_IDLE_TIMEOUT, rd.read_exact(&mut buf))
        .await
        .context("timed out reading a request")??;
    Ok(Some(serde_json::from_slice(&buf).context("malformed request")?))
}

async fn send(wr: &mut OwnedWriteHalf, response: &Response) -> Result<()> {
    wr.write_all(&encode_message(response)?).await?;
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The user a change request acts on: non-root callers act on themselves;
/// root must name an existing user.
fn resolve_target(peer: Peer, requested: Option<String>) -> Result<String, String> {
    let me = users::name_of(peer.uid);
    match (peer.uid, requested) {
        (0, Some(user)) => {
            if is_safe_username(&user) && users::uid_of(&user).is_some() {
                Ok(user)
            } else {
                Err(format!("no such user {user:?}"))
            }
        }
        (0, None) => Err("root must name a user (--user NAME)".into()),
        (_, None) => me.ok_or_else(|| "your account could not be looked up".into()),
        (_, Some(user)) if Some(&user) == me.as_ref() => Ok(user),
        (_, Some(_)) => Err("you can only change your own face data".into()),
    }
}

fn status(d: &Daemon, peer: Peer) -> Response {
    let Some(user) = users::name_of(peer.uid) else {
        return Response::Error { message: "your account could not be looked up".into() };
    };
    let profile = match d.storage.load_user(&user) {
        Ok(p) => p,
        Err(e) => return Response::Error { message: format!("{e:#}") },
    };
    let (enrolled, samples, created, updated, model_current) = match &profile {
        Some(p) if !p.embeddings.is_empty() => {
            (true, p.embeddings.len(), p.created, p.updated, d.profile_usable(p))
        }
        _ => (false, 0, 0, 0, true),
    };
    Response::Status(UserStatus {
        user,
        enrolled,
        samples,
        max_samples: MAX_SAMPLES,
        created,
        updated,
        model_current,
        camera: d.camera_label(),
        infrared: d.infrared,
        recognizer: d.config.recognition.model.describe().to_string(),
    })
}

fn list_enrolled(d: &Daemon, peer: Peer) -> Response {
    match d.storage.list_users() {
        Ok(profiles) => Response::Users(
            profiles
                .into_iter()
                .filter(|p| peer.may_act_for(&p.user))
                .map(|p| EnrolledUser {
                    model_current: d.profile_usable(&p),
                    samples: p.embeddings.len(),
                    updated: p.updated,
                    user: p.user,
                })
                .collect(),
        ),
        Err(e) => Response::Error { message: format!("{e:#}") },
    }
}

// ─── Authentication ─────────────────────────────────────────────────────────

async fn authenticate(d: &Arc<Daemon>, peer: Peer, user: String, wr: &mut OwnedWriteHalf) -> Result<()> {
    if !is_safe_username(&user) || !peer.may_act_for(&user) {
        AuditLogger::denied("authenticate", Some(&user), peer.uid, "caller is neither root nor the user");
        return send(wr, &Response::Denied { reason: "you can only authenticate yourself".into() }).await;
    }
    // Every outcome is also told to the account's desktop watchers.
    let outcome = |o: AuthOutcome| {
        d.emit(&user, AuthEvent::Finished(o));
        Response::Auth(o)
    };

    // Cheap checks first: none of these touch the camera.
    let profile = match d.storage.load_user(&user) {
        Ok(Some(p)) if !p.embeddings.is_empty() => p,
        Ok(_) => return send(wr, &outcome(AuthOutcome::NotEnrolled)).await,
        Err(e) => {
            error!("Profile for {user}: {e:#}");
            return send(wr, &Response::Error { message: "face data could not be read".into() }).await;
        }
    };
    if !d.profile_usable(&profile) {
        warn!(
            "{user} was set up with model {:?} on a {} camera; now model {} on a {} camera",
            profile.model_id,
            if profile.infrared { "infrared" } else { "colour" },
            d.engine.model_id,
            if d.infrared { "infrared" } else { "colour" }
        );
        return send(wr, &outcome(AuthOutcome::ModelChanged)).await;
    }
    if let Some(left) = d.limiter.locked_for(&user) {
        return send(wr, &outcome(AuthOutcome::LockedOut { seconds: left.as_secs().max(1) })).await;
    }
    if d.changes.is_changing(&user) {
        info!("Refusing face authentication for {user}: their face data is being changed");
        return send(wr, &outcome(AuthOutcome::Busy)).await;
    }
    let Ok(Ok(_permit)) = tokio::time::timeout(AUTH_QUEUE_WAIT, d.auth_gate.acquire()).await else {
        return send(wr, &outcome(AuthOutcome::Busy)).await;
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let worker = {
        let d = d.clone();
        tokio::task::spawn_blocking(move || {
            let Some(camera) = camera::lock_camera(&d.camera, CAMERA_WAIT) else {
                return auth::Verdict { outcome: AuthOutcome::CameraUnavailable, best_score: 0.0 };
            };
            let _ = ready_tx.send(());
            auth::verify(&camera, &d.pipeline(), &profile.embeddings)
        })
    };
    if ready_rx.await.is_ok() {
        d.emit(&user, AuthEvent::Scanning);
        // Tell PAM to show "look at the camera" now that capture is starting.
        if send(wr, &Response::Scanning).await.is_err() {
            // PAM gave up; still finish so the camera closes cleanly.
            let _ = worker.await;
            return Ok(());
        }
    }
    let verdict = worker.await?;

    match verdict.outcome {
        AuthOutcome::Matched => d.limiter.reset(&user),
        // Only a face that was seen and rejected is an impostor attempt.
        AuthOutcome::NotMatched | AuthOutcome::NotLive => d.limiter.record_failure(&user),
        AuthOutcome::NoFace | AuthOutcome::BadLighting => {
            // Back-pressure: let the sensor settle before PAM retries.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        _ => {}
    }
    AuditLogger::auth(&user, &verdict.outcome, verdict.best_score, peer.uid);
    send(wr, &outcome(verdict.outcome)).await
}

async fn benchmark(d: &Arc<Daemon>) -> Response {
    let Ok(Ok(_permit)) = tokio::time::timeout(AUTH_QUEUE_WAIT, d.auth_gate.acquire()).await else {
        return Response::Error { message: "face unlock is busy".into() };
    };
    let d2 = d.clone();
    let result = tokio::task::spawn_blocking(move || {
        let camera = camera::lock_camera(&d2.camera, CAMERA_WAIT).context("camera is in use")?;
        auth::benchmark(&camera, &d2.pipeline())
    })
    .await;
    match result {
        Ok(Ok(report)) => Response::Benchmark(report),
        Ok(Err(e)) => Response::Error { message: format!("{e:#}") },
        Err(e) => Response::Error { message: format!("benchmark failed: {e}") },
    }
}

// ─── Changing face data ─────────────────────────────────────────────────────

/// Completes when the client sends anything or hangs up. Clients send nothing
/// during a session, so either means "stop".
async fn client_gone(rd: &mut OwnedReadHalf) {
    let mut b = [0u8; 1];
    let _ = rd.read(&mut b).await;
}

/// Polkit check for a non-root caller. `Ok(())` when allowed, otherwise the
/// failure to report (`None` when the client already left).
async fn authorize(peer: Peer, rd: &mut OwnedReadHalf) -> Result<(), Option<EnrollFailure>> {
    if peer.uid == 0 {
        return Ok(());
    }
    let Some(pid) = peer.pid else { return Err(Some(EnrollFailure::NotAuthorized)) };
    match polkit::check(pid, peer.uid, client_gone(rd)).await {
        polkit::Decision::Authorized => Ok(()),
        polkit::Decision::NotAuthorized => Err(Some(EnrollFailure::NotAuthorized)),
        polkit::Decision::NoAgent => Err(Some(EnrollFailure::NoAuthAgent)),
        polkit::Decision::Dismissed => Err(Some(EnrollFailure::AuthCancelled)),
        polkit::Decision::Abandoned => Err(None),
        polkit::Decision::Error(e) => {
            error!("polkit: {e}");
            Err(Some(EnrollFailure::Internal { message: e }))
        }
    }
}

async fn enroll_session(
    d: &Arc<Daemon>,
    peer: Peer,
    mode: EnrollMode,
    requested: Option<String>,
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
) -> Result<()> {
    let fail = |f: EnrollFailure| Response::Enroll(EnrollEvent::Failed(f));
    let user = match resolve_target(peer, requested.clone()) {
        Ok(u) => u,
        Err(reason) => {
            AuditLogger::denied("enroll", requested.as_deref(), peer.uid, &reason);
            return send(wr, &Response::Denied { reason }).await;
        }
    };
    // Held until the session ends: blocks face auth for this user meanwhile.
    let Some(_ticket) = d.changes.begin(&user) else {
        return send(wr, &fail(EnrollFailure::InProgress)).await;
    };

    let existing = match d.storage.load_user(&user) {
        Ok(p) => p.filter(|p| !p.embeddings.is_empty()),
        Err(e) => return send(wr, &fail(EnrollFailure::Internal { message: format!("{e:#}") })).await,
    };
    let base: Vec<Vec<f32>> = match (mode, &existing) {
        (EnrollMode::Append, None) => return send(wr, &fail(EnrollFailure::NotEnrolled)).await,
        (EnrollMode::Append, Some(p)) if !d.profile_usable(p) => return send(wr, &fail(EnrollFailure::DoesNotMatch)).await,
        (EnrollMode::Append, Some(p)) if p.embeddings.len() + enroll::steps(mode).len() > MAX_SAMPLES => {
            return send(wr, &fail(EnrollFailure::Full { max: MAX_SAMPLES })).await
        }
        (EnrollMode::Append, Some(p)) => p.embeddings.clone(),
        (EnrollMode::Replace, _) => Vec::new(),
    };

    send(wr, &Response::Enroll(EnrollEvent::Authorizing)).await?;
    if let Err(failure) = authorize(peer, rd).await {
        if let Some(f) = &failure {
            AuditLogger::denied("enroll", Some(&user), peer.uid, &f.message());
            send(wr, &fail(f.clone())).await?;
        }
        return Ok(());
    }
    AuditLogger::event("ENROLL_START", &user, peer.uid, &format!("mode={mode:?}"));

    // Capture runs on a blocking thread; events flow back through `rx`.
    let (tx, mut rx) = mpsc::channel::<Response>(4);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker = {
        let (d, cancel, base) = (d.clone(), cancel.clone(), base.clone());
        tokio::task::spawn_blocking(move || {
            let Some(camera) = camera::lock_camera(&d.camera, CAMERA_WAIT) else {
                return enroll::Finish::Failed(EnrollFailure::CameraUnavailable);
            };
            enroll::capture(&camera, &d.pipeline(), mode, &base, &tx, &cancel)
        })
    };
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => if send(wr, &m).await.is_err() { cancel.store(true, Ordering::Relaxed); break; },
                None => break,
            },
            _ = client_gone(rd) => { cancel.store(true, Ordering::Relaxed); break; }
        }
    }
    drop(rx);

    match worker.await? {
        enroll::Finish::Completed(samples) => {
            let profile = UserProfile {
                user: user.clone(),
                created: match (mode, &existing) {
                    (EnrollMode::Append, Some(p)) if p.created > 0 => p.created,
                    _ => now(),
                },
                updated: now(),
                model_id: Some(d.engine.model_id.clone()),
                infrared: d.infrared,
                embeddings: base.into_iter().chain(samples).collect(),
            };
            let total = profile.embeddings.len();
            if let Err(e) = d.storage.save_user(&profile) {
                error!("Saving profile for {user}: {e:#}");
                return send(wr, &fail(EnrollFailure::Internal { message: "face data could not be saved".into() })).await;
            }
            d.limiter.reset(&user);
            AuditLogger::event("ENROLL_DONE", &user, peer.uid, &format!("mode={mode:?} samples={total}"));
            send(wr, &Response::Enroll(EnrollEvent::Completed { samples: total })).await
        }
        enroll::Finish::Failed(f) => {
            AuditLogger::event("ENROLL_FAILED", &user, peer.uid, &format!("{f:?}"));
            send(wr, &fail(f)).await
        }
        enroll::Finish::Cancelled => {
            AuditLogger::event("ENROLL_CANCELLED", &user, peer.uid, "");
            Ok(())
        }
    }
}

async fn remove(
    d: &Arc<Daemon>,
    peer: Peer,
    requested: Option<String>,
    rd: &mut OwnedReadHalf,
    wr: &mut OwnedWriteHalf,
) -> Result<()> {
    let user = match resolve_target(peer, requested.clone()) {
        Ok(u) => u,
        Err(reason) => {
            AuditLogger::denied("remove", requested.as_deref(), peer.uid, &reason);
            return send(wr, &Response::Denied { reason }).await;
        }
    };
    let Some(_ticket) = d.changes.begin(&user) else {
        return send(wr, &Response::Enroll(EnrollEvent::Failed(EnrollFailure::InProgress))).await;
    };
    if let Err(failure) = authorize(peer, rd).await {
        if let Some(f) = failure {
            AuditLogger::denied("remove", Some(&user), peer.uid, &f.message());
            send(wr, &Response::Enroll(EnrollEvent::Failed(f))).await?;
        }
        return Ok(());
    }
    match d.storage.delete_user(&user) {
        Ok(()) => {
            d.limiter.reset(&user);
            AuditLogger::event("ENROLL_REMOVED", &user, peer.uid, "");
            send(wr, &Response::Removed).await
        }
        Err(e) => {
            error!("Removing face data for {user}: {e:#}");
            send(wr, &Response::Error { message: "face data could not be removed".into() }).await
        }
    }
}

fn log_system_facts() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mem_mb = fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
    });
    info!("{cores} CPU cores, {} MB RAM available", mem_mb.map_or("?".into(), |m| m.to_string()));
    if mem_mb.is_some_and(|m| m < 500) {
        warn!("Low memory; face processing may be slow");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchers_are_capped_per_user_and_in_total() {
        let w = Watchers::default();
        let mine: Vec<_> = (0..MAX_WATCHERS_PER_USER).map(|_| w.join(1000).expect("within the per-user cap")).collect();
        assert!(w.join(1000).is_none(), "per-user cap");
        drop(mine);
        assert!(w.join(1000).is_some(), "slots are freed when a watcher leaves");

        let w = Watchers::default();
        let all: Vec<_> = (0..MAX_WATCHERS as u32).map(|i| w.join(i).expect("within the total cap")).collect();
        assert!(w.join(99_999).is_none(), "total cap");
        drop(all);
        assert!(w.per_uid.lock().unwrap().is_empty());
    }
}
