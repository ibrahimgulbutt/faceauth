//! Account lookups through NSS (local files, LDAP, SSSD, systemd-userdb) and
//! process facts needed to identify a polkit subject.

use std::ffi::{CStr, CString};

/// uid of `user`, if the account exists.
pub fn uid_of(user: &str) -> Option<u32> {
    let c_user = CString::new(user).ok()?;
    with_passwd(|pwd, buf, result| unsafe {
        libc::getpwnam_r(c_user.as_ptr(), pwd, buf.as_mut_ptr(), buf.len(), result)
    })
    .map(|pwd| pwd.pw_uid)
}

/// Login name of `uid`, if the account exists.
pub fn name_of(uid: u32) -> Option<String> {
    let mut name = None;
    with_passwd(|pwd, buf, result| {
        let rc = unsafe { libc::getpwuid_r(uid, pwd, buf.as_mut_ptr(), buf.len(), result) };
        if rc == 0 && !result.is_null() {
            // pw_name points into `buf`, so copy it out while the buffer is alive.
            name = unsafe { CStr::from_ptr((*pwd).pw_name) }.to_str().ok().map(str::to_owned);
        }
        rc
    })?;
    name
}

/// Run a `get*_r` lookup, growing the scratch buffer on ERANGE.
fn with_passwd(
    mut lookup: impl FnMut(*mut libc::passwd, &mut Vec<libc::c_char>, *mut *mut libc::passwd) -> libc::c_int,
) -> Option<libc::passwd> {
    let mut size = 16 * 1024;
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0 as libc::c_char; size];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = lookup(&mut pwd, &mut buf, &mut result);
        if rc == libc::ERANGE && size < 1024 * 1024 {
            size *= 4;
            continue;
        }
        return (rc == 0 && !result.is_null()).then_some(pwd);
    }
}

/// Start time of `pid` in clock ticks since boot (field 22 of /proc/<pid>/stat).
/// Together with the pid it identifies a process without pid-reuse races.
pub fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) is in parentheses and may contain spaces.
    let after_comm = &stat[stat.rfind(')')? + 2..];
    after_comm.split_whitespace().nth(19)?.parse().ok()
}

pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_root() {
        assert_eq!(uid_of("root"), Some(0));
        assert_eq!(name_of(0).as_deref(), Some("root"));
    }

    #[test]
    fn unknown_user_is_none() {
        assert_eq!(uid_of("no-such-user-faceauth-test"), None);
    }

    #[test]
    fn reads_own_start_time() {
        let pid = std::process::id() as i32;
        assert!(process_start_time(pid).is_some());
    }
}
