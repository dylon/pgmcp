use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct WebuiOptions {
    pub token: Option<String>,
    pub allowed_origins: Vec<String>,
    pub heartbeat_secs: u64,
    pub replay_page: i64,
    pub max_msgs_per_sec: u32,
    pub max_connections: usize,
    pub handshake_rate_per_min: u32,
}

impl Default for WebuiOptions {
    fn default() -> Self {
        Self {
            token: None,
            allowed_origins: Vec::new(),
            heartbeat_secs: 15,
            replay_page: 250,
            max_msgs_per_sec: 200,
            max_connections: 16,
            handshake_rate_per_min: 60,
        }
    }
}

#[derive(Debug)]
pub(crate) struct FixedWindowRateLimiter {
    max_per_window: u32,
    window: Duration,
    window_started: Instant,
    count: u32,
}

impl FixedWindowRateLimiter {
    pub(crate) fn new(max_per_window: u32, window: Duration, now: Instant) -> Self {
        Self {
            max_per_window: max_per_window.max(1),
            window: if window.is_zero() {
                Duration::from_secs(1)
            } else {
                window
            },
            window_started: now,
            count: 0,
        }
    }

    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now
            .checked_duration_since(self.window_started)
            .unwrap_or_default();
        if elapsed >= self.window {
            self.window_started = now;
            self.count = 0;
        }
        if self.count >= self.max_per_window {
            return false;
        }
        self.count += 1;
        true
    }
}

struct WebuiRuntime {
    options: WebuiOptions,
    active_connections: AtomicUsize,
    handshake_limiter: Mutex<FixedWindowRateLimiter>,
}

#[derive(Clone)]
pub struct WebuiState {
    pool: PgPool,
    runtime: Arc<WebuiRuntime>,
}

pub(crate) struct ConnectionPermit {
    runtime: Arc<WebuiRuntime>,
}

impl WebuiState {
    pub fn new(pool: PgPool, options: WebuiOptions) -> Self {
        let handshake_limiter = FixedWindowRateLimiter::new(
            options.handshake_rate_per_min,
            Duration::from_secs(60),
            Instant::now(),
        );
        Self {
            pool,
            runtime: Arc::new(WebuiRuntime {
                options,
                active_connections: AtomicUsize::new(0),
                handshake_limiter: Mutex::new(handshake_limiter),
            }),
        }
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub(crate) fn options(&self) -> &WebuiOptions {
        &self.runtime.options
    }

    pub(crate) fn handshake_allowed(&self, now: Instant) -> bool {
        let mut limiter = lock_or_recover(&self.runtime.handshake_limiter);
        limiter.allow(now)
    }

    pub(crate) fn try_acquire_connection(&self) -> Option<ConnectionPermit> {
        let max_connections = self.runtime.options.max_connections.max(1);
        loop {
            let current = self.runtime.active_connections.load(Ordering::Acquire);
            if current >= max_connections {
                return None;
            }
            if self
                .runtime
                .active_connections
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(ConnectionPermit {
                    runtime: Arc::clone(&self.runtime),
                });
            }
        }
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.runtime
            .active_connections
            .fetch_sub(1, Ordering::AcqRel);
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lazy_pool() -> PgPool {
        sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://pgmcp:pgmcp@localhost/pgmcp")
            .expect("lazy pool URL should parse")
    }

    #[test]
    fn fixed_window_limiter_allows_only_configured_events_per_window() {
        let start = Instant::now();
        let mut limiter = FixedWindowRateLimiter::new(2, Duration::from_secs(1), start);
        assert!(limiter.allow(start));
        assert!(limiter.allow(start + Duration::from_millis(500)));
        assert!(!limiter.allow(start + Duration::from_millis(999)));
        assert!(limiter.allow(start + Duration::from_secs(1)));
    }

    #[test]
    fn fixed_window_limiter_treats_zero_limit_as_one() {
        let start = Instant::now();
        let mut limiter = FixedWindowRateLimiter::new(0, Duration::from_secs(1), start);
        assert!(limiter.allow(start));
        assert!(!limiter.allow(start + Duration::from_millis(1)));
        assert!(limiter.allow(start + Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn handshake_limiter_uses_configured_budget() {
        let options = WebuiOptions {
            handshake_rate_per_min: 2,
            ..WebuiOptions::default()
        };
        let state = WebuiState::new(lazy_pool(), options);
        let start = Instant::now();

        assert!(state.handshake_allowed(start));
        assert!(state.handshake_allowed(start + Duration::from_secs(1)));
        assert!(!state.handshake_allowed(start + Duration::from_secs(2)));
        assert!(state.handshake_allowed(start + Duration::from_secs(61)));
    }

    #[tokio::test]
    async fn connection_limit_releases_capacity_when_permit_drops() {
        let options = WebuiOptions {
            max_connections: 2,
            ..WebuiOptions::default()
        };
        let state = WebuiState::new(lazy_pool(), options);

        let first = state.try_acquire_connection();
        let second = state.try_acquire_connection();
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(state.try_acquire_connection().is_none());

        drop(first);
        assert!(state.try_acquire_connection().is_some());
    }
}
