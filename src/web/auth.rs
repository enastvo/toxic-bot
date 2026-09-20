//! Session-based auth middleware and a simple per-IP login rate limiter.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tower_sessions::Session;

const ADMIN_KEY: &str = "admin";

/// Marks the current session as an authenticated admin.
pub async fn mark_admin(session: &Session) -> Result<(), tower_sessions::session::Error> {
    session.insert(ADMIN_KEY, true).await
}

/// Whether the current session is an authenticated admin.
pub async fn is_admin(session: &Session) -> bool {
    session.get::<bool>(ADMIN_KEY).await.ok().flatten().unwrap_or(false)
}

/// Axum middleware: redirects (303) to `/login` unless the session carries
/// `admin=true`.
pub async fn require_admin(session: Session, req: Request, next: Next) -> Response {
    if is_admin(&session).await {
        next.run(req).await
    } else {
        Redirect::to("/login").into_response()
    }
}

/// Simple in-memory sliding-window rate limiter for login attempts, keyed by
/// client IP.
pub struct LoginLimiter {
    max_attempts: u32,
    window: Duration,
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl Default for LoginLimiter {
    fn default() -> Self {
        Self::new(10, Duration::from_secs(60))
    }
}

impl LoginLimiter {
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self { max_attempts, window, hits: Mutex::new(HashMap::new()) }
    }

    /// Records an attempt from `ip` and returns whether it should be allowed.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut guard = self.hits.lock().unwrap();
        let entry = guard.entry(ip).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() as u32 >= self.max_attempts {
            return false;
        }
        entry.push(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_blocks_after_max_attempts() {
        let limiter = LoginLimiter::new(3, Duration::from_secs(60));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(!limiter.allow(ip));
    }

    #[test]
    fn rate_limiter_tracks_ips_independently() {
        let limiter = LoginLimiter::new(1, Duration::from_secs(60));
        let a: IpAddr = "127.0.0.1".parse().unwrap();
        let b: IpAddr = "127.0.0.2".parse().unwrap();
        assert!(limiter.allow(a));
        assert!(!limiter.allow(a));
        assert!(limiter.allow(b));
    }
}
