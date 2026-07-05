//! A small in-memory fixed-window rate limiter, keyed by user (when signed in) or IP.
//! It is per process: fine for one API instance, and a deliberate trade for not running Redis.
//! Run several instances behind a proxy and each enforces its own budget.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct RateLimiter {
    window: Duration,
    hits: Mutex<HashMap<String, (Instant, u32)>>,
}

const PRUNE_ABOVE: usize = 20_000;

impl RateLimiter {
    pub fn new(window: Duration) -> Self {
        Self { window, hits: Mutex::default() }
    }

    /// Counts one request for `key`; `false` once it exceeds `max` within the window.
    pub fn check(&self, key: &str, max: u32, now: Instant) -> bool {
        let mut hits = self.hits.lock().unwrap();
        if hits.len() > PRUNE_ABOVE {
            hits.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let e = hits.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(e.0) >= self.window {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= max
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.hits.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_the_limit_then_blocks_until_the_window_passes() {
        let l = RateLimiter::new(Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..3 {
            assert!(l.check("a", 3, t0), "request {i}");
        }
        assert!(!l.check("a", 3, t0 + Duration::from_secs(10)));
        assert!(!l.check("a", 3, t0 + Duration::from_secs(59)));
        assert!(l.check("a", 3, t0 + Duration::from_secs(61)), "a fresh window");
    }

    #[test]
    fn keys_are_independent_and_limits_are_per_call() {
        let l = RateLimiter::new(Duration::from_secs(60));
        let t = Instant::now();
        assert!(l.check("a", 1, t) && !l.check("a", 1, t));
        assert!(l.check("b", 1, t));
        assert!(l.check("c", 100, t));
    }

    #[test]
    fn stale_entries_are_pruned_so_memory_stays_bounded() {
        let l = RateLimiter::new(Duration::from_secs(1));
        let t = Instant::now();
        for i in 0..(PRUNE_ABOVE + 10) {
            l.check(&format!("k{i}"), 5, t);
        }
        l.check("late", 5, t + Duration::from_secs(5));
        assert!(l.len() < 100, "{}", l.len());
    }
}
