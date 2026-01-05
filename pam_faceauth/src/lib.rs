use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::os::unix::net::UnixStream;
use std::io::{Read, Write};
use faceauth_core::{AuthRequest, AuthResponse, SOCKET_PATH};

// Manual definitions to avoid pam-sys import issues
#[allow(non_camel_case_types)]
pub type pam_handle_t = c_void;
pub const PAM_SUCCESS: c_int = 0;
pub const PAM_SERVICE_ERR: c_int = 3;
pub const PAM_AUTH_ERR: c_int = 7;
pub const PAM_USER_UNKNOWN: c_int = 10;
pub const PAM_IGNORE: c_int = 25;

#[link(name = "pam")]
extern "C" {
    fn pam_get_user(
        pamh: *const pam_handle_t,
        user: *mut *const c_char,
        prompt: *const c_char,
    ) -> c_int;
}

#[no_mangle]
pub extern "C" fn pam_sm_authenticate(
    pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    let user = match get_user(pamh) {
        Some(u) => u,
        None => return PAM_USER_UNKNOWN,
    };

    // Connect to daemon
    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(_) => {
            // Daemon not running, ignore this module
            return PAM_IGNORE;
        }
    };

    // Set timeouts to prevent hanging sudo
    let timeout = std::time::Duration::from_secs(5);
    if stream.set_read_timeout(Some(timeout)).is_err() || stream.set_write_timeout(Some(timeout)).is_err() {
        return PAM_SERVICE_ERR;
    }

    let request = AuthRequest::Authenticate { user };
    let req_bytes = match serde_json::to_vec(&request) {
        Ok(b) => b,
        Err(_) => return PAM_SERVICE_ERR,
    };

    // Send length prefix
    let len = req_bytes.len() as u32;
    if stream.write_all(&len.to_be_bytes()).is_err() {
        return PAM_SERVICE_ERR;
    }

    if stream.write_all(&req_bytes).is_err() {
        return PAM_SERVICE_ERR;
    }

    // Read response length
    let mut len_buf = [0u8; 4];
    if stream.read_exact(&mut len_buf).is_err() {
        return PAM_SERVICE_ERR;
    }
    let resp_len = u32::from_be_bytes(len_buf) as usize;

    // Read response body
    let mut resp_buf = vec![0u8; resp_len];
    if stream.read_exact(&mut resp_buf).is_err() {
        return PAM_SERVICE_ERR;
    }

    let response: AuthResponse = match serde_json::from_slice(&resp_buf) {
        Ok(r) => r,
        Err(_) => return PAM_SERVICE_ERR,
    };

    match response {
        AuthResponse::Success => PAM_SUCCESS,
        AuthResponse::Failure => PAM_AUTH_ERR,
        _ => PAM_SERVICE_ERR,
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

#[no_mangle]
pub extern "C" fn pam_sm_acct_mgmt(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *const *const c_char,
) -> c_int {
    PAM_SUCCESS
}

fn get_user(pamh: *mut pam_handle_t) -> Option<String> {
    let mut user_ptr: *const c_char = ptr::null();
    let result = unsafe { pam_get_user(pamh, &mut user_ptr, ptr::null()) };

    if result != PAM_SUCCESS || user_ptr.is_null() {
        return None;
    }

    let c_str = unsafe { CStr::from_ptr(user_ptr) };
    c_str.to_str().ok().map(|s| s.to_string())
}



