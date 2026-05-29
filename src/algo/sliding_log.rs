//! Sliding window log.
//!
//! Keeps the timestamp of every admitted unit in the trailing `period` and
//! admits only while fewer than `limit` remain. This is the exact algorithm —
//! the admitted count in any rolling window never exceeds `limit`, with no
//! boundary burst — at the cost of up to `limit` timestamps of memory per key
//! and a short per-key lock on each check. It ignores the quota's `burst`.

use core::time::Duration;
use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::decision::Decision;
use crate::quota::Quota;

/// Clamps a duration-since-epoch to nanoseconds.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// Sliding-log state for one key: admit timestamps (ns), oldest at the front.
pub(crate) struct SlidingLog {
    log: Mutex<VecDeque<u64>>,
    period_ns: u64,
    limit: u32,
}

impl SlidingLog {
    pub(crate) fn new(quota: &Quota) -> Self {
        Self {
            log: Mutex::new(VecDeque::new()),
            period_ns: nanos(quota.period()).max(1),
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
        let mut log = self.lock();

        // Drop every timestamp at least `period` old — it has aged out of the
        // trailing window. Comparing `ts + period <= now` (rather than a
        // `now - period` window start) stays correct when `now < period`, where
        // a subtracted start would underflow to zero.
        while log
            .front()
            .is_some_and(|&ts| ts.saturating_add(self.period_ns) <= now_ns)
        {
            let _ = log.pop_front();
        }

        let count = log.len();
        let n = n as usize;
        if count + n <= self.limit as usize {
            for _ in 0..n {
                log.push_back(now_ns);
            }
            return Decision::Allow;
        }

        // Denied: `count + n - limit` of the oldest entries must age out before
        // the request fits. That many expire once the (need_expire)-th oldest
        // crosses `period`.
        let need_expire = count + n - self.limit as usize;
        let expire_at = log[need_expire - 1].saturating_add(self.period_ns);
        Decision::Deny {
            retry_after: Duration::from_nanos(expire_at.saturating_sub(now_ns)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<u64>> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::SlidingLog;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_admits_limit_within_window_then_denies() {
        let sl = SlidingLog::new(&Quota::per_second(3));
        for _ in 0..3 {
            assert_eq!(sl.acquire(1, at(0)), Decision::Allow);
        }
        assert!(sl.acquire(1, at(500)).is_deny());
    }

    #[test]
    fn test_admits_again_as_oldest_ages_out() {
        let sl = SlidingLog::new(&Quota::per_second(3));
        assert!(sl.acquire(1, at(0)).is_allow());
        assert!(sl.acquire(1, at(100)).is_allow());
        assert!(sl.acquire(1, at(200)).is_allow());
        // Full until the first (t=0) entry ages out at t=1000.
        assert!(sl.acquire(1, at(999)).is_deny());
        assert!(sl.acquire(1, at(1000)).is_allow()); // t=0 entry now out of window
    }

    #[test]
    fn test_retry_after_points_at_oldest_expiry() {
        let sl = SlidingLog::new(&Quota::per_second(2));
        assert!(sl.acquire(1, at(0)).is_allow());
        assert!(sl.acquire(1, at(300)).is_allow());
        match sl.acquire(1, at(500)) {
            Decision::Deny { retry_after } => {
                // Oldest (t=0) leaves the window at t=1000 → 500ms out.
                assert_eq!(retry_after, Duration::from_millis(500));
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn test_no_boundary_burst() {
        // Unlike a fixed window, a sliding log never admits more than `limit` in
        // any window: fill at the end of one notional window, none free at the
        // start of the next.
        let sl = SlidingLog::new(&Quota::per_second(5));
        for _ in 0..5 {
            assert!(sl.acquire(1, at(900)).is_allow());
        }
        // 100ms later (a fixed window would have rolled) the log is still full.
        assert!(sl.acquire(1, at(1000)).is_deny());
    }

    #[test]
    fn test_request_larger_than_limit_never_admits() {
        let sl = SlidingLog::new(&Quota::per_second(3));
        assert_eq!(
            sl.acquire(4, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }
}
