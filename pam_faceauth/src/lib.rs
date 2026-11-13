//! pam_faceauth — asks faceauthd to recognise the user's face.
//!
//! Usage (before the password module): `auth sufficient pam_faceauth.so [quiet]`
//!
//! Every non-success falls through to the next module, so the password always
//! works. Not-applicable cases (remote session, daemon down, not enrolled)
//! return PAM_IGNORE; a face that was looked for and not accepted returns
//! PAM_AUTH_ERR.

use faceauth_core::{recv_message, send_message, AuthOutcome, Request, Response, DAEMON_USER, SOCKET_PATH};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::time::Duration;

#[allow(non_camel_case_types)]
pub type pam_handle_t = c_void;
pub const PAM_SUCCESS: c_int = 0;
pub const PAM_SERVICE_ERR: c_int = 3;
pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_USER_UNKNOWN: c_int = 10;
pub const PAM_IGNORE: c_int = 25;
const PAM_CONV: c_int = 5;
const PAM_RHOST: c_int = 4;
const PAM_TEXT_INFO: c_int = 4;
const PAM_SILENT: c_int = 0x8000;

/// Time for the daemon to accept the request and get the camera (it may wait
/// for another authentication to finish).
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Time for capture and recognition once scanning has started.
const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: Option<
        unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int,
    >,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_get_user(pamh: *const pam_handle_t, user: *mut *const c_char, prompt: *const c_char) -> c_int;
    fn pam_get_item(pamh: *const pam_handle_t, item_type: c_int, item: *mut *const c_void) -> c_int;
    fn pam_getenv(pamh: *const pam_handle_t, name: *const c_char) -> *const c_char;
}

/// # Safety
/// Called by libpam with a valid handle and `argc` C strings in `argv`.
#[no_mangle]
pub unsafe extern "C" fn pam_sm_authenticate(
    pamh: *mut pam_handle_t,
    flags: c_int,
    argc: c_int,
    argv: *const *const c_char,
) -> c_int {
    // This runs inside the login screen, the lock screen and sudo. A bug here
    // must never take them down (a panic leaving an `extern "C"` function
    // aborts the whole process): anything unexpected falls back to the
    // password.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| authenticate(pamh, flags, argc, argv)))
        .unwrap_or(PAM_IGNORE)
}

unsafe fn authenticate(pamh: *mut pam_handle_t, flags: c_int, argc: c_int, argv: *const *const c_char) -> c_int {
    let quiet = flags & PAM_SILENT != 0 || module_args(argc, argv).iter().any(|a| a == "quiet");
    let say = |text: &str| {
        if !quiet {
            converse(pamh, &format!("FaceAuth: {text}"));
        }
    };

    let Some(user) = get_user(pamh) else { return PAM_USER_UNKNOWN };

    // Never use the local camera to authorise a remote session: otherwise
    // whoever sits at the machine approves sudo for an SSH user.
    if is_remote_session(pamh) {
        return PAM_IGNORE;
    }

    let Ok(mut stream) = UnixStream::connect(SOCKET_PATH) else { return PAM_IGNORE };
    // Only trust an answer from the real daemon. Anything else listening on
    // the socket path falls through to the next module.
    if !trusted_daemon(peer_uid(&stream)) {
        return PAM_IGNORE;
    }
    if stream.set_write_timeout(Some(START_TIMEOUT)).is_err() || stream.set_read_timeout(Some(START_TIMEOUT)).is_err() {
        return PAM_SERVICE_ERR;
    }
    if send_message(&mut stream, &Request::Authenticate { user }).is_err() {
        return PAM_IGNORE;
    }

    loop {
        match recv_message::<_, Response>(&mut stream) {
            Ok(Response::Scanning) => {
                say("look at the camera…");
                let _ = stream.set_read_timeout(Some(SCAN_TIMEOUT));
            }
            Ok(Response::Auth(outcome)) => {
                return match outcome {
                    AuthOutcome::Matched => PAM_SUCCESS,
                    AuthOutcome::NotMatched
                    | AuthOutcome::NotLive
                    | AuthOutcome::NoFace
                    | AuthOutcome::BadLighting
                    | AuthOutcome::LockedOut { .. } => {
                        say(&outcome.message());
                        PAM_AUTH_ERR
                    }
                    AuthOutcome::ModelChanged => {
                        say(&outcome.message());
                        PAM_IGNORE
                    }
                    AuthOutcome::NotEnrolled | AuthOutcome::CameraUnavailable | AuthOutcome::Busy => PAM_IGNORE,
                };
            }
            // Denied, Error, an unexpected message, a timeout or a hang-up:
            // nothing to say, just let the password take over.
            _ => return PAM_IGNORE,
        }
    }
}

#[no_mangle]
pub extern "C" fn pam_sm_setcred(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// This module only authenticates. Returning PAM_IGNORE (not SUCCESS) means a
/// misplaced `account sufficient pam_faceauth.so` line cannot skip account
/// checks such as expiry or pam_access rules.
#[no_mangle]
pub extern "C" fn pam_sm_acct_mgmt(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_IGNORE
}

unsafe fn module_args(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    if argv.is_null() {
        return Vec::new();
    }
    (0..argc.max(0) as usize)
        .filter_map(|i| {
            let p = *argv.add(i);
            (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
        })
        .collect()
}

unsafe fn get_user(pamh: *mut pam_handle_t) -> Option<String> {
    let mut user_ptr: *const c_char = ptr::null();
    if pam_get_user(pamh, &mut user_ptr, ptr::null()) != PAM_SUCCESS || user_ptr.is_null() {
        return None;
    }
    CStr::from_ptr(user_ptr).to_str().ok().map(str::to_owned)
}

/// Show an informational line through the application's conversation
/// function (sudo prints it; GDM shows it under the user's name).
unsafe fn converse(pamh: *mut pam_handle_t, text: &str) {
    let mut item: *const c_void = ptr::null();
    if pam_get_item(pamh, PAM_CONV, &mut item) != PAM_SUCCESS || item.is_null() {
        return;
    }
    let conv = &*(item as *const PamConv);
    let Some(func) = conv.conv else { return };
    let Ok(c_text) = CString::new(text) else { return };
    let message = PamMessage { msg_style: PAM_TEXT_INFO, msg: c_text.as_ptr() };
    let mut message_ptr: *const PamMessage = &message;
    let mut response: *mut PamResponse = ptr::null_mut();
    func(1, &mut message_ptr, &mut response, conv.appdata_ptr);
    if !response.is_null() {
        if !(*response).resp.is_null() {
            libc::free((*response).resp as *mut c_void);
        }
        libc::free(response as *mut c_void);
    }
}

/// uid of the process on the other end of the socket, as reported by the kernel.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

/// The daemon runs as root (older installs) or as the `faceauth` system user.
fn trusted_daemon(uid: Option<u32>) -> bool {
    match uid {
        Some(0) => true,
        Some(uid) => daemon_uid() == Some(uid),
        None => false,
    }
}

fn daemon_uid() -> Option<u32> {
    let name = CString::new(DAEMON_USER).ok()?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as c_char; 16 * 1024];
    let mut result: *mut libc::passwd = ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    (rc == 0 && !result.is_null()).then_some(pwd.pw_uid)
}

/// Best-effort remote-session detection (same approach as Howdy): a remote
/// host set by the PAM application (sshd, telnet login), or SSH variables in
/// the environment of the process running PAM (sudo/su inside SSH).
unsafe fn is_remote_session(pamh: *mut pam_handle_t) -> bool {
    const SSH_VARS: [&str; 3] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

    if let Some(rhost) = pam_string_item(pamh, PAM_RHOST) {
        // Older sudo versions set PAM_RHOST to the local hostname; that is not remote.
        if !rhost.is_empty() && rhost != "localhost" && Some(rhost.as_str()) != local_hostname().as_deref() {
            return true;
        }
    }
    SSH_VARS.iter().any(|var| {
        std::env::var_os(var).is_some_and(|v| !v.is_empty())
            || CString::new(*var).ok().is_some_and(|name| {
                let val = pam_getenv(pamh, name.as_ptr());
                !val.is_null() && *val != 0
            })
    })
}

unsafe fn pam_string_item(pamh: *mut pam_handle_t, item_type: c_int) -> Option<String> {
    let mut item: *const c_void = ptr::null();
    if pam_get_item(pamh, item_type, &mut item) != PAM_SUCCESS || item.is_null() {
        return None;
    }
    CStr::from_ptr(item as *const c_char).to_str().ok().map(str::to_owned)
}

fn local_hostname() -> Option<String> {
    let mut buf = [0 as c_char; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    buf[buf.len() - 1] = 0;
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_or_daemon_user_is_trusted() {
        assert!(trusted_daemon(Some(0)));
        assert!(!trusted_daemon(None));
        // An ordinary login user (this test's own uid, unless run as root).
        let me = unsafe { libc::geteuid() };
        if me != 0 && Some(me) != daemon_uid() {
            assert!(!trusted_daemon(Some(me)));
        }
    }
}
