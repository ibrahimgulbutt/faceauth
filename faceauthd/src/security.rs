use std::collections::HashMap;
use std::time::{Duration, Instant};
use std::sync::Mutex;
use log::{info, warn};

pub struct RateLimiter {
    // Map of user -> (failed_attempts, last_attempt_time)
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
    max_attempts: u32,
    lockout_duration: Duration,
}

impl RateLimiter {
    pub fn new(max_attempts: u32, lockout_seconds: u64) -> Self {
        Self {
            attempts: Mutex::new(HashMap::new()),
            max_attempts,
            lockout_duration: Duration::from_secs(lockout_seconds),
        }
    }

    pub fn check_allowed(&self, user: &str) -> bool {
        let mut attempts = match self.attempts.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                 warn!("RateLimiter lock poisoned. Resetting state.");
                 let mut guard = poisoned.into_inner();
                 guard.clear();
                 guard
            }
        };

        if let Some((count, last_time)) = attempts.get(user) {
            if *count >= self.max_attempts {
                if last_time.elapsed() < self.lockout_duration {
                    warn!("Rate limit exceeded for user {}. Locked out for {:?}s", user, (self.lockout_duration - last_time.elapsed()).as_secs());
                    return false;
                } else {
                    // Lockout expired, reset
                    attempts.remove(user);
                }
            }
        }
        true
    }

    pub fn record_failure(&self, user: &str) {
        let mut attempts = match self.attempts.lock() {
             Ok(guard) => guard,
             Err(poisoned) => {
                 warn!("RateLimiter lock poisoned during record_failure.");
                 poisoned.into_inner()
             }
        };
        let entry = attempts.entry(user.to_string()).or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
        warn!("Failed attempt {}/{} for user {}", entry.0, self.max_attempts, user);
    }

    pub fn reset(&self, user: &str) {
        let mut attempts = match self.attempts.lock() {
             Ok(guard) => guard,
             Err(poisoned) => poisoned.into_inner()
        };
        attempts.remove(user);
    }
}

pub struct AuditLogger;

impl AuditLogger {
    pub fn log_auth_attempt(user: &str, success: bool, score: f32, liveness_passed: bool) {
        if success {
            info!(target: "audit", "AUTH_SUCCESS user={} score={:.4} liveness={}", user, score, liveness_passed);
        } else {
            warn!(target: "audit", "AUTH_FAILURE user={} score={:.4} liveness={}", user, score, liveness_passed);
        }
    }

    pub fn log_enrollment(user: &str, success: bool) {
        if success {
            info!(target: "audit", "ENROLL_SUCCESS user={}", user);
        } else {
            warn!(target: "audit", "ENROLL_FAILURE user={}", user);
        }
    }
}
