use log::{info, warn};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Per-user lockout after repeated impostor attempts. Only attempts where a
/// face was seen and rejected count; a dark room or an empty chair does not.
pub struct RateLimiter {
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
    max_attempts: u32,
    lockout: Duration,
}

impl RateLimiter {
    pub fn new(max_attempts: u32, lockout_seconds: u64) -> Self {
        Self { attempts: Mutex::new(HashMap::new()), max_attempts, lockout: Duration::from_secs(lockout_seconds) }
    }

    /// Remaining lockout for `user`, or `None` if they may try.
    pub fn locked_for(&self, user: &str) -> Option<Duration> {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let (count, last) = *attempts.get(user)?;
        let elapsed = last.elapsed();
        if elapsed >= self.lockout {
            // Expired (or a stale partial count): forget it.
            attempts.remove(user);
            return None;
        }
        (count >= self.max_attempts).then(|| self.lockout - elapsed)
    }

    pub fn record_failure(&self, user: &str) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|p| p.into_inner());
        let entry = attempts.entry(user.to_string()).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        warn!("Failed attempt {}/{} for {user}", entry.0, self.max_attempts);
    }

    pub fn reset(&self, user: &str) {
        self.attempts.lock().unwrap_or_else(|p| p.into_inner()).remove(user);
    }
}

/// Users whose face data is being changed right now. Face authentication is
/// refused for them meanwhile, so a polkit stack that includes pam_faceauth
/// cannot approve an enrollment change with the very face being changed.
#[derive(Clone, Default)]
pub struct ChangeGuard(Arc<Mutex<HashSet<String>>>);

pub struct ChangeTicket {
    guard: ChangeGuard,
    user: String,
}

impl ChangeGuard {
    /// Mark `user` as being changed; `None` if a change is already running.
    pub fn begin(&self, user: &str) -> Option<ChangeTicket> {
        let mut set = self.0.lock().unwrap_or_else(|p| p.into_inner());
        set.insert(user.to_string()).then(|| ChangeTicket { guard: self.clone(), user: user.to_string() })
    }

    pub fn is_changing(&self, user: &str) -> bool {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).contains(user)
    }
}

impl Drop for ChangeTicket {
    fn drop(&mut self) {
        self.guard.0.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.user);
    }
}

/// Audit events go to the journal under the `audit` log target.
pub struct AuditLogger;

impl AuditLogger {
    pub fn auth(user: &str, outcome: &faceauth_core::AuthOutcome, score: f32, peer_uid: u32) {
        let line = format!("AUTH user={user:?} outcome={outcome:?} best_score={score:.4} peer_uid={peer_uid}");
        if matches!(outcome, faceauth_core::AuthOutcome::Matched) {
            info!(target: "audit", "{line}");
        } else {
            warn!(target: "audit", "{line}");
        }
    }

    pub fn event(kind: &str, user: &str, peer_uid: u32, detail: &str) {
        info!(target: "audit", "{kind} user={user:?} peer_uid={peer_uid} {detail}");
    }

    pub fn denied(what: &str, user: Option<&str>, peer_uid: u32, reason: &str) {
        warn!(target: "audit", "DENIED {what} user={user:?} peer_uid={peer_uid} reason={reason:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_attempts() {
        let rl = RateLimiter::new(2, 60);
        assert!(rl.locked_for("u").is_none());
        rl.record_failure("u");
        assert!(rl.locked_for("u").is_none());
        rl.record_failure("u");
        assert!(rl.locked_for("u").is_some());
        rl.reset("u");
        assert!(rl.locked_for("u").is_none());
    }

    #[test]
    fn change_guard_is_exclusive_and_released() {
        let g = ChangeGuard::default();
        let t = g.begin("alice").unwrap();
        assert!(g.is_changing("alice"));
        assert!(g.begin("alice").is_none());
        drop(t);
        assert!(!g.is_changing("alice"));
    }
}
