//! Ask polkit whether a client process may change face data, letting polkit
//! show the user's own password dialog (action `org.faceauth.manage`,
//! `auth_self`).

use faceauth_core::{DAEMON_USER, POLKIT_ACTION};
use log::{info, warn};
use std::future::Future;
use std::time::Duration;
use tokio::process::Command;

/// How long the user has to answer the password dialog.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Authorized,
    NotAuthorized,
    /// No authentication agent is running for the client's session.
    NoAgent,
    /// The user closed the dialog.
    Dismissed,
    /// The client went away or the dialog timed out.
    Abandoned,
    Error(String),
}

/// Check `org.faceauth.manage` for process `pid` (uid `uid`). Resolves early
/// with `Abandoned` if `client_gone` completes first (the client hung up).
pub async fn check(pid: i32, uid: u32, client_gone: impl Future<Output = ()>) -> Decision {
    if dev_skip_enabled() {
        warn!("FACEAUTH_DEV_SKIP_POLKIT is set: allowing pid {pid} without a password (development daemon)");
        return Decision::Authorized;
    }

    // pid,start-time,uid identifies the process without pid-reuse races;
    // polkit looks the start time up itself when we cannot read it.
    let subject = match crate::users::process_start_time(pid) {
        Some(start) => format!("{pid},{start},{uid}"),
        None => pid.to_string(),
    };
    let mut child = match Command::new("pkcheck")
        .args(["--action-id", POLKIT_ACTION, "--process", &subject, "--allow-user-interaction"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Decision::Error(format!("cannot run pkcheck (is polkit installed?): {e}")),
    };
    let stderr = child.stderr.take();

    let status = tokio::select! {
        s = child.wait() => s,
        _ = client_gone => return Decision::Abandoned,
        _ = tokio::time::sleep(DIALOG_TIMEOUT) => return Decision::Abandoned,
    };
    let detail = match stderr {
        Some(mut e) => {
            use tokio::io::AsyncReadExt;
            let mut s = String::new();
            let _ = e.read_to_string(&mut s).await;
            s.trim().to_string()
        }
        None => String::new(),
    };

    match status.map(|s| s.code()) {
        Ok(Some(0)) => {
            info!("polkit authorized pid {pid} (uid {uid})");
            Decision::Authorized
        }
        Ok(Some(1)) => Decision::NotAuthorized,
        Ok(Some(2)) => Decision::NoAgent,
        Ok(Some(3)) => Decision::Dismissed,
        Ok(code) => Decision::Error(format!("pkcheck exited with {code:?}: {detail}")),
        Err(e) => Decision::Error(format!("pkcheck failed: {e}")),
    }
}

/// `FACEAUTH_DEV_SKIP_POLKIT=1` is honoured only for a development daemon
/// running as an ordinary login user, which PAM never trusts.
fn dev_skip_enabled() -> bool {
    if std::env::var("FACEAUTH_DEV_SKIP_POLKIT").as_deref() != Ok("1") {
        return false;
    }
    let uid = crate::users::effective_uid();
    let me = crate::users::name_of(uid);
    uid != 0 && me.as_deref() != Some(DAEMON_USER)
}
