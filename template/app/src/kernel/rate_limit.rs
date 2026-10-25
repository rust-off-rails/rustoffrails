//! In-process rate limiting (Rails 8 `rate_limit to:, within:`). The limit is per instance,
//! which is acceptable for this app (see docs/decisions.md).
//!
//! Each key carries its own window: a `moka::Expiry` expires an entry `ttl` after creation, so
//! every call site can use a different window. `check` counts a hit; `exhausted` only peeks,
//! which is what failure-only limits need (peek first, `check` after a failure).

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use moka::{Expiry, future::Cache};

use crate::config::AppEnv;
use crate::error::AppError;

/// Development replaces every configured limit with this one, so clicking through a flow while
/// building it never locks the developer out; test and production keep the real limits.
pub const DEV_LIMIT: u32 = 1000;

#[derive(Clone)]
struct Window {
    hits: Arc<AtomicU32>,
    ttl: Arc<std::sync::atomic::AtomicU64>,
}

impl Window {
    fn new(ttl: Duration) -> Self {
        Self {
            hits: Arc::new(AtomicU32::new(0)),
            ttl: Arc::new(std::sync::atomic::AtomicU64::new(
                u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
            )),
        }
    }

    fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl.load(Ordering::Relaxed))
    }

    fn widen(&self, ttl: Duration) -> bool {
        let want = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
        self.ttl.fetch_max(want, Ordering::Relaxed) < want
    }
}

struct WindowExpiry;

impl Expiry<String, Window> for WindowExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Window,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(value.ttl())
    }

    fn expire_after_update(
        &self,
        _key: &String,
        value: &Window,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(value.ttl())
    }
}

#[derive(Clone)]
pub struct RateLimiter {
    windows: Cache<String, Window>,
    development: bool,
}

impl RateLimiter {
    pub fn new(env: AppEnv) -> Self {
        Self {
            windows: Cache::builder()
                .max_capacity(100_000)
                .expire_after(WindowExpiry)
                .build(),
            development: env == AppEnv::Development,
        }
    }

    fn limit(&self, configured: u32) -> u32 {
        if self.development {
            DEV_LIMIT.max(configured)
        } else {
            configured
        }
    }

    /// Counts one hit for `key`. `Err(TooManyRequests)` once the caller is past `limit` hits
    /// within `window` (the window starts at the first hit).
    pub async fn check(&self, key: &str, limit: u32, window: Duration) -> Result<(), AppError> {
        let w = self
            .windows
            .get_with(key.to_owned(), async move { Window::new(window) })
            .await;
        if w.widen(window) {
            self.windows.insert(key.to_owned(), w.clone()).await;
        }
        let limit = self.limit(limit);
        loop {
            let cur = w.hits.load(Ordering::Relaxed);
            if cur >= limit {
                return Err(AppError::TooManyRequests);
            }
            if w.hits
                .compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }

    /// `true` once `key` has reached `limit` hits in its current window. Doesn't count.
    pub async fn exhausted(&self, key: &str, limit: u32) -> bool {
        self.windows
            .get(key)
            .await
            .is_some_and(|w| w.hits.load(Ordering::Relaxed) >= self.limit(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_after_the_limit_per_key() {
        let rl = RateLimiter::new(AppEnv::Test);
        let window = Duration::from_secs(3600);
        for _ in 0..3 {
            assert!(rl.check("login:1.2.3.4", 3, window).await.is_ok());
        }
        assert!(matches!(
            rl.check("login:1.2.3.4", 3, window).await,
            Err(AppError::TooManyRequests)
        ));
        assert!(rl.check("login:5.6.7.8", 3, window).await.is_ok());
    }

    #[tokio::test]
    async fn development_raises_the_limit_and_production_keeps_it() {
        let window = Duration::from_secs(60);
        let dev = RateLimiter::new(AppEnv::Development);
        for _ in 0..50 {
            dev.check("k", 10, window).await.unwrap();
        }
        let prod = RateLimiter::new(AppEnv::Production);
        for _ in 0..10 {
            prod.check("k", 10, window).await.unwrap();
        }
        assert!(prod.check("k", 10, window).await.is_err());
    }

    #[tokio::test]
    async fn exhausted_peeks_without_counting() {
        let rl = RateLimiter::new(AppEnv::Test);
        let window = Duration::from_secs(3600);
        assert!(!rl.exhausted("k", 2).await);
        rl.check("k", 2, window).await.unwrap();
        assert!(!rl.exhausted("k", 2).await);
        rl.check("k", 2, window).await.unwrap();
        assert!(rl.exhausted("k", 2).await);
        // Peeking never advanced the counter: the next hit is the third, not the fifth.
        assert!(rl.check("k", 2, window).await.is_err());
    }

    #[tokio::test]
    async fn refused_attempts_are_not_counted() {
        let rl = RateLimiter::new(AppEnv::Test);
        let window = Duration::from_secs(3600);
        for _ in 0..3 {
            rl.check("login:counted", 3, window).await.unwrap();
        }
        for _ in 0..5 {
            assert!(rl.check("login:counted", 3, window).await.is_err());
        }
        let w = rl
            .windows
            .get("login:counted")
            .await
            .expect("the window exists");
        assert_eq!(
            w.hits.load(Ordering::Relaxed),
            3,
            "refused attempts incremented the counter"
        );
        assert!(rl.exhausted("login:counted", 3).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_admit_exactly_the_limit() {
        let rl = Arc::new(RateLimiter::new(AppEnv::Test));
        let window = Duration::from_secs(3600);
        let barrier = Arc::new(tokio::sync::Barrier::new(100));
        let mut tasks = Vec::new();
        for _ in 0..100 {
            let rl = rl.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                rl.check("login:race", 10, window).await.is_ok()
            }));
        }
        let mut passed = 0;
        for t in tasks {
            if t.await.unwrap() {
                passed += 1;
            }
        }
        assert_eq!(passed, 10, "exactly the limit may pass concurrently");
    }

    #[tokio::test]
    async fn a_later_callers_window_is_honoured_for_the_same_key() {
        let rl = RateLimiter::new(AppEnv::Test);
        rl.check("shared", 1, Duration::from_millis(60))
            .await
            .unwrap();
        assert!(
            rl.check("shared", 1, Duration::from_secs(3600))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            rl.check("shared", 1, Duration::from_secs(3600))
                .await
                .is_err(),
            "the first caller's 60ms window expired a key the second caller holds for an hour"
        );
    }

    #[tokio::test]
    async fn each_key_gets_its_own_window() {
        let rl = RateLimiter::new(AppEnv::Test);
        rl.check("short", 1, Duration::from_millis(60))
            .await
            .unwrap();
        assert!(
            rl.check("short", 1, Duration::from_millis(60))
                .await
                .is_err()
        );
        rl.check("long", 1, Duration::from_secs(3600))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        // `short` expired on its own ttl; `long` is unaffected.
        assert!(
            rl.check("short", 1, Duration::from_millis(60))
                .await
                .is_ok()
        );
        assert!(
            rl.check("long", 1, Duration::from_secs(3600))
                .await
                .is_err()
        );
    }
}
