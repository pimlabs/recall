//! The rate limiter the middleware consults.
//!
//! It is a security boundary rather than a politeness feature: limiting
//! runs before auth, so this is what bounds how many token guesses an
//! unauthenticated client gets.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A per-IP fixed window, in memory — enough for a single-owner server, and
/// it needs no external store.
pub(super) struct RateLimiter {
    window: Duration,
    max: u32,
    state: Mutex<LimiterState>,
}

struct LimiterState {
    buckets: HashMap<String, Bucket>,
    last_sweep: Instant,
}

struct Bucket {
    count: u32,
    window_start: Instant,
}

impl RateLimiter {
    pub(super) fn new(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            state: Mutex::new(LimiterState {
                buckets: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        }
    }

    pub(super) fn limited(&self, ip: &str) -> bool {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        // Sweep on the way through rather than from a background task:
        // without it every IP that ever connected would stay in memory for
        // the life of the process, and doing it here keeps the limiter
        // usable with no runtime around it.
        if now.duration_since(state.last_sweep) >= self.window {
            state.last_sweep = now;
            let window = self.window;
            state
                .buckets
                .retain(|_, b| now.duration_since(b.window_start) < 2 * window);
        }

        let bucket = state.buckets.entry(ip.to_string()).or_insert(Bucket {
            count: 0,
            window_start: now,
        });
        if now.duration_since(bucket.window_start) >= self.window {
            bucket.count = 0;
            bucket.window_start = now;
        }
        bucket.count += 1;
        bucket.count > self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_counts_per_ip_and_resets_after_the_window() {
        let rl = RateLimiter::new(Duration::from_millis(40), 2);
        assert!(!rl.limited("a"));
        assert!(!rl.limited("a"));
        assert!(rl.limited("a"), "third request in the window is limited");
        assert!(!rl.limited("b"), "a different IP has its own bucket");

        std::thread::sleep(Duration::from_millis(60));
        assert!(!rl.limited("a"), "the window resets");
    }

    #[test]
    fn rate_limiter_sweeps_stale_buckets() {
        let rl = RateLimiter::new(Duration::from_millis(10), 100);
        for i in 0..50 {
            rl.limited(&format!("10.0.0.{i}"));
        }
        std::thread::sleep(Duration::from_millis(30));
        rl.limited("10.0.1.1");
        let n = rl
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .len();
        assert_eq!(n, 1, "stale buckets should have been swept, got {n}");
    }
}
