//! Sliding window counter.
//!
//! Approximates a sliding window in O(1) memory by blending the current fixed
//! window's count with a time-weighted fraction of the previous window's. It
//! smooths the fixed window's boundary burst without the per-request memory of a
//! log — the common production choice — at the cost of being approximate: the
//! true count in a rolling window can reach up to `2 * limit` in the worst case.
//! It ignores the quota's `burst`.

use core::time::Duration;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::decision::Decision;
use crate::quota::Quota;

/// Clamps a duration-since-epoch to nanoseconds.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// The two-window counter state for one key.
struct State {
    /// Index of the window `curr` counts.
    window: u64,
    /// Admitted in the previous window.
    prev: u32,
    /// Admitted in the current window.
    curr: u32,
}

/// Sliding-counter state for one key.
pub(crate) struct SlidingCounter {
    state: Mutex<State>,
    period_ns: u64,
    limit: u32,
}

impl SlidingCounter {
    pub(crate) fn new(quota: &Quota, now: Duration) -> Self {
        let period_ns = nanos(quota.period()).max(1);
        Self {
            state: Mutex::new(State {
                window: nanos(now) / period_ns,
                prev: 0,
                curr: 0,
            }),
            period_ns,
            limit: quota.limit(),
        }
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        if self.limit == 0 || n > self.limit {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        let now_ns = nanos(now);
        let window_number = now_ns / self.period_ns;
        let offset = now_ns % self.period_ns;
        let mut state = self.lock();

        // Roll the windows forward if time has moved into a later window.
        if window_number > state.window {
            state.prev = if window_number == state.window + 1 {
                state.curr
            } else {
                0
            };
            state.curr = 0;
            state.window = window_number;
        }

        // The previous window's contribution fades linearly across the current
        // window: full at the boundary, zero at the next.
        let period = self.period_ns as f64;
        let weight = (self.period_ns - offset) as f64 / period;
        let estimated = f64::from(state.prev) * weight + f64::from(state.curr);

        if estimated + f64::from(n) <= f64::from(self.limit) {
            state.curr = state.curr.saturating_add(n);
            return Decision::Allow;
        }

        // Denied: estimate how long until the weighted count falls enough. While
        // the previous window still contributes, its term decays at `prev /
        // period` per nanosecond; otherwise wait for the next window boundary.
        let deficit = estimated + f64::from(n) - f64::from(self.limit);
        let wait_ns = if state.prev > 0 {
            ((deficit * period / f64::from(state.prev)) as u64).min(self.period_ns)
        } else {
            self.period_ns - offset
        };
        Decision::Deny {
            retry_after: Duration::from_nanos(wait_ns),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::SlidingCounter;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_admits_limit_in_a_quiet_window() {
        let sc = SlidingCounter::new(&Quota::per_second(5), at(0));
        for _ in 0..5 {
            assert_eq!(sc.acquire(1, at(0)), Decision::Allow);
        }
        assert!(sc.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_previous_window_weight_limits_the_next() {
        // Fill window [0,1000). Early in the next window the previous count is
        // still weighted heavily, so few new units are admitted.
        let sc = SlidingCounter::new(&Quota::per_second(10), at(0));
        for _ in 0..10 {
            assert!(sc.acquire(1, at(0)).is_allow());
        }
        // At t=1000 (weight ~1.0 of the previous 10) the estimate is ~10 → full.
        assert!(sc.acquire(1, at(1000)).is_deny());
        // Past the midpoint of the new window the previous weight has faded
        // enough to admit again.
        assert!(sc.acquire(1, at(1600)).is_allow());
    }

    #[test]
    fn test_stale_windows_reset() {
        let sc = SlidingCounter::new(&Quota::per_second(3), at(0));
        for _ in 0..3 {
            assert!(sc.acquire(1, at(0)).is_allow());
        }
        // Far in the future both windows are stale → a fresh allowance.
        assert!(sc.acquire(1, at(10_000)).is_allow());
    }

    #[test]
    fn test_denial_reports_a_bounded_retry_after() {
        let sc = SlidingCounter::new(&Quota::per_second(2), at(0));
        assert!(sc.acquire(2, at(0)).is_allow());
        match sc.acquire(1, at(100)) {
            Decision::Deny { retry_after } => {
                assert!(retry_after > Duration::ZERO);
                assert!(retry_after <= Duration::from_secs(1));
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn test_request_larger_than_limit_never_admits() {
        let sc = SlidingCounter::new(&Quota::per_second(3), at(0));
        assert_eq!(
            sc.acquire(4, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }
}
