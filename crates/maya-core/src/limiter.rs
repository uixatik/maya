use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

/// Asynchronous global token-bucket bandwidth limiter.
///
/// When configured with `None` or `Some(0)`, [`Self::acquire`] performs only a
/// single relaxed atomic load and returns immediately with zero lock contention.
#[derive(Debug, Clone)]
pub struct TokenBucketLimiter {
    inner: Arc<LimiterInner>,
}

#[derive(Debug)]
struct LimiterInner {
    rate_bps: AtomicU64,
    state: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    rate: u64,
    capacity: f64,
    tokens: f64,
    last_refill: Option<Instant>,
}

impl TokenBucketLimiter {
    /// Create a new limiter capped at `bytes_per_sec` (`None` or `Some(0)` = unlimited).
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        let rate = normalize_rate(bytes_per_sec);
        let capacity = rate as f64;
        Self {
            inner: Arc::new(LimiterInner {
                rate_bps: AtomicU64::new(rate),
                state: Mutex::new(BucketState {
                    rate,
                    capacity,
                    tokens: capacity,
                    last_refill: None,
                }),
            }),
        }
    }

    /// Create an unlimited [`TokenBucketLimiter`].
    pub fn unlimited() -> Self {
        Self::new(None)
    }

    /// Return the active rate limit in bytes per second, or `None` if unlimited.
    pub fn rate(&self) -> Option<u64> {
        let rate = self.inner.rate_bps.load(Ordering::Relaxed);
        if rate == 0 {
            None
        } else {
            Some(rate)
        }
    }

    /// Dynamically update the rate limit (`None` or `Some(0)` disables throttling).
    pub fn set_rate(&self, bytes_per_sec: Option<u64>) {
        let new_rate = normalize_rate(bytes_per_sec);
        let mut guard = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let now = Instant::now();
        if guard.rate > 0 {
            if let Some(last) = guard.last_refill {
                let elapsed = now.saturating_duration_since(last).as_secs_f64();
                guard.tokens = (guard.tokens + elapsed * guard.rate as f64).min(guard.capacity);
            }
        }

        if new_rate == 0 {
            guard.rate = 0;
            guard.capacity = 0.0;
            guard.tokens = 0.0;
            guard.last_refill = None;
        } else {
            let new_capacity = new_rate as f64;
            if guard.rate == 0 {
                guard.tokens = new_capacity;
                guard.last_refill = Some(now);
            } else {
                guard.tokens = guard.tokens.min(new_capacity);
                guard.last_refill = Some(now);
            }
            guard.rate = new_rate;
            guard.capacity = new_capacity;
        }

        self.inner.rate_bps.store(new_rate, Ordering::Release);
    }

    /// Acquire permission to transfer `bytes`, sleeping asynchronously if the
    /// configured rate limit is currently exceeded.
    pub async fn acquire(&self, bytes: u64) {
        if bytes == 0 || self.inner.rate_bps.load(Ordering::Acquire) == 0 {
            return;
        }

        let wait_duration = {
            let mut guard = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            if guard.rate == 0 {
                Duration::ZERO
            } else {
                let now = Instant::now();
                if let Some(last) = guard.last_refill {
                    let elapsed = now.saturating_duration_since(last).as_secs_f64();
                    guard.tokens = (guard.tokens + elapsed * guard.rate as f64).min(guard.capacity);
                }
                guard.last_refill = Some(now);
                guard.tokens -= bytes as f64;

                if guard.tokens >= 0.0 {
                    Duration::ZERO
                } else {
                    let deficit = -guard.tokens;
                    let wait_secs = deficit / guard.rate as f64;
                    if wait_secs.is_finite() && wait_secs > 0.0 {
                        Duration::from_secs_f64(wait_secs)
                    } else {
                        Duration::ZERO
                    }
                }
            }
        };

        if !wait_duration.is_zero() {
            tokio::time::sleep(wait_duration).await;
        }
    }
}

fn normalize_rate(rate: Option<u64>) -> u64 {
    rate.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_returns_immediately() {
        let limiter = TokenBucketLimiter::new(None);
        assert_eq!(limiter.rate(), None);
        limiter.acquire(100_000_000).await;

        limiter.set_rate(Some(0));
        assert_eq!(limiter.rate(), None);
        limiter.acquire(100_000_000).await;
    }

    #[tokio::test]
    async fn throttles_at_configured_rate() {
        // 20_000 B/s -> 2_000 bytes initial capacity (0ms), next 2_000 bytes ~100ms.
        let limiter = TokenBucketLimiter::new(Some(20_000));
        assert_eq!(limiter.rate(), Some(20_000));

        let start = std::time::Instant::now();
        limiter.acquire(20_000).await;
        assert!(start.elapsed() < Duration::from_millis(50));

        // 2_000 bytes at 20_000 B/s = 100ms.
        limiter.acquire(2_000).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }

    #[tokio::test]
    async fn set_rate_updates_throttling_dynamically() {
        let limiter = TokenBucketLimiter::new(Some(10_000));
        limiter.acquire(10_000).await;

        // Increase rate to 40_000 B/s: 4_000 bytes takes ~100ms (with capacity clamped to 0..).
        limiter.set_rate(Some(40_000));
        assert_eq!(limiter.rate(), Some(40_000));

        let start = std::time::Instant::now();
        limiter.acquire(4_000).await;
        assert!(start.elapsed() >= Duration::from_millis(75));

        // Disable limit dynamically.
        limiter.set_rate(None);
        assert_eq!(limiter.rate(), None);
        let fast_start = std::time::Instant::now();
        limiter.acquire(1_000_000).await;
        assert!(fast_start.elapsed() < Duration::from_millis(50));
    }
}
