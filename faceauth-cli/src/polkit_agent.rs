//! Terminal polkit agent for `faceauth enroll` / `remove` when no graphical
//! agent is running (SSH-less TTY, minimal window managers).

use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct TtyAgent(Option<Child>);

impl TtyAgent {
    /// Start `pkttyagent --fallback` for this process and wait until it has
    /// registered. With `--fallback` it steps aside when a desktop agent
    /// (GNOME Shell, KDE, …) already handles the session. Missing
    /// `pkttyagent` or no terminal is fine: polkit then uses the desktop agent.
    pub fn spawn() -> Self {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Self(None);
        }
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Self(None);
        }
        let (read_fd, write_fd) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let notify = fds[1];
        let mut cmd = Command::new("pkttyagent");
        cmd.args(["--process", &std::process::id().to_string(), "--fallback", "--notify-fd", &notify.to_string()])
            .stdout(Stdio::inherit())
            .stderr(Stdio::null());
        // Let the child inherit the notify pipe despite O_CLOEXEC.
        unsafe {
            cmd.pre_exec(move || {
                if libc::fcntl(notify, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().ok();
        drop(write_fd);
        if child.is_some() {
            // pkttyagent closes the notify fd once it has registered.
            let mut f = std::fs::File::from(read_fd);
            let mut buf = [0u8; 16];
            let _ = f.read(&mut buf);
        }
        Self(child)
    }
}

impl Drop for TtyAgent {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
