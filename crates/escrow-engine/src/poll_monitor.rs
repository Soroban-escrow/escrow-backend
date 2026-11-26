//! Indexer poll failure circuit breaker & exponential backoff
//! Resolves Issue #226: alert after repeated poll failures with bounded exponential backoff.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PollFailureMonitor {
    consecutive_failures: u32,
    threshold: u32,
    base_backoff: Duration,
    max_backoff: Duration,
    last_error: Option<String>,
}

impl PollFailureMonitor {
    pub fn new(threshold: u32, base_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            consecutive_failures: 0,
            threshold,
            base_backoff,
            max_backoff,
            last_error: None,
        }
    }

    /// Records a failed poll attempt, computes current backoff delay,
    /// and returns an Alert payload if consecutive failures exceed threshold.
    pub fn on_failure(&mut self, err: String) -> (Duration, Option<String>) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_error = Some(err.clone());

        // Exponential backoff: min(base * 2^(failures - 1), max_backoff)
        let multiplier = 2u64.saturating_pow(self.consecutive_failures.saturating_sub(1));
        let computed = self.base_backoff.saturating_mul(multiplier as u32);
        let backoff = computed.min(self.max_backoff);

        let alert = if self.consecutive_failures >= self.threshold {
            Some(format!(
                "CRITICAL: Indexer poll failed {} consecutive times. Last error: {}",
                self.consecutive_failures, err
            ))
        } else {
            None
        };

        (backoff, alert)
    }

    /// Resets consecutive failure counter on successful poll.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
        self.last_error = None;
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_failure_backoff_and_alert() {
        let mut monitor = PollFailureMonitor::new(3, Duration::from_millis(100), Duration::from_millis(500));
        
        let (b1, a1) = monitor.on_failure("decode error".into());
        assert_eq!(b1, Duration::from_millis(100));
        assert!(a1.is_none());

        let (b2, a2) = monitor.on_failure("timeout".into());
        assert_eq!(b2, Duration::from_millis(200));
        assert!(a2.is_none());

        // Reaches threshold (3) -> alert must fire
        let (b3, a3) = monitor.on_failure("network unreachable".into());
        assert_eq!(b3, Duration::from_millis(400));
        assert!(a3.is_some());
        assert!(a3.unwrap().contains("network unreachable"));

        // Cap check
        let (b4, _) = monitor.on_failure("cap test".into());
        assert_eq!(b4, Duration::from_millis(500)); // Capped at max_backoff

        // Success resets
        monitor.on_success();
        assert_eq!(monitor.consecutive_failures(), 0);
    }
}
