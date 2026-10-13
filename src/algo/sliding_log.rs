//! Sliding window log.
//!
//! Keeps the timestamp of every admitted unit in the trailing `period` and
//! admits only while fewer than `limit` remain. This is the exact algorithm —
//! the admitted count in any rolling window never exceeds `limit`, with no
//! boundary burst — at the cost of per-key memory that grows with the traffic
//! and a short per-key lock on each check. It ignores the quota's `burst`.
//!
//! # Memory
//!
//! Units admitted at the same instant share one record, so the log holds one
//! 16-byte `(timestamp, running total)` record per *distinct admit time* in the
//! trailing window: at most `limit` records, and a single `check_n` of any size
//! costs one. The worst case per key is therefore `16 * limit` bytes, which is
//! why the validating builder caps the limit at
//! [`MAX_SLIDING_LOG_LIMIT`](crate::MAX_SLIDING_LOG_LIMIT).

use core::time::Duration;
use std::collections::VecDeque;

use crate::decision::Decision;
use crate::quota::Quota;
use crate::snapshot::Snapshot;
use crate::sync::{self, Mutex};

/// Clamps a duration-since-epoch to nanoseconds.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// One coalesced record: every unit admitted at `at_ns`, and the running total
/// of units ever admitted up to and including them.
#[derive(Clone, Copy)]
struct Run {
    at_ns: u64,
    total: u64,
}

/// The log: live records oldest first, plus the running total at the end of the
/// newest record already aged out. `back.total - expired_total` is the number
/// of units in the window, without walking the records.
struct Log {
    runs: VecDeque<Run>,
    expired_total: u64,
}

impl Log {
    /// The running total at the newest record (or at the last expiry when the
    /// log is empty).
    fn total(&self) -> u64 {
        self.runs.back().map_or(self.expired_total, |run| run.total)
    }

    /// Units currently inside the window.
    fn in_window(&self) -> u64 {
        self.total() - self.expired_total
    }

    /// Drops every record at least `period_ns` old as of `now_ns`. Comparing
    /// `ts + period <= now` (rather than a `now - period` window start) stays
    /// correct when `now < period`, where a subtracted start would underflow.
    fn expire(&mut self, now_ns: u64, period_ns: u64) {
        while let Some(front) = self.runs.front().copied() {
            if front.at_ns.saturating_add(period_ns) > now_ns {
                break;
            }
            self.expired_total = front.total;
            let _expired = self.runs.pop_front();
        }
    }

    /// Records `n` units admitted at `now_ns`, coalescing with the newest
    /// record when it carries the same timestamp.
    fn record(&mut self, n: u32, now_ns: u64) {
        let total = self.total().saturating_add(u64::from(n));
        match self.runs.back_mut() {
            Some(back) if back.at_ns == now_ns => back.total = total,
            _ => self.runs.push_back(Run {
                at_ns: now_ns,
                total,
            }),
        }
    }
}

/// Sliding-log state for one key.
pub(crate) struct SlidingLog {
    log: Mutex<Log>,
    period_ns: u64,
    limit: u32,
}

impl SlidingLog {
    pub(crate) fn new(quota: &Quota) -> Self {
        Self {
            log: Mutex::new(Log {
                runs: VecDeque::new(),
                expired_total: 0,
            }),
            period_ns: nanos(quota.period()),
            limit: quota.limit(),
        }
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        // A zero period is degenerate (fail closed); a zero limit or `n > limit`
        // can never fit.
        if self.limit == 0 || self.period_ns == 0 || n > self.limit {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        let real_now_ns = nanos(now);
        let mut log = sync::lock(&self.log);
        // A late thread (its `now` read before another thread's newer record)
        // is treated as arriving with that record: the log stays sorted and the
        // window can never be judged against an older instant.
        let now_ns = log
            .runs
            .back()
            .map_or(real_now_ns, |back| back.at_ns.max(real_now_ns));
        log.expire(now_ns, self.period_ns);

        let in_window = log.in_window();
        let wanted = u64::from(n);
        let limit = u64::from(self.limit);
        if in_window + wanted <= limit {
            log.record(n, now_ns);
            return Decision::Allow;
        }

        // Denied: the oldest `in_window + n - limit` units must age out first.
        // They have all left once the record whose running total reaches that
        // many past the last expiry crosses `period`.
        let target = log.expired_total + (in_window + wanted - limit);
        let index = log.runs.partition_point(|run| run.total < target);
        let expire_at = log
            .runs
            .get(index)
            .map_or(now_ns, |run| run.at_ns.saturating_add(self.period_ns));
        Decision::Deny {
            retry_after: Duration::from_nanos(expire_at.saturating_sub(real_now_ns)),
        }
    }

    /// Units still admissible right now, and the wait until every record has
    /// aged out (the full allowance is back).
    pub(crate) fn peek(&self, now: Duration) -> Snapshot {
        if self.limit == 0 || self.period_ns == 0 {
            return Snapshot::closed();
        }
        let real_now_ns = nanos(now);
        let mut log = sync::lock(&self.log);
        let now_ns = log
            .runs
            .back()
            .map_or(real_now_ns, |back| back.at_ns.max(real_now_ns));
        log.expire(now_ns, self.period_ns);
        let used = u32::try_from(log.in_window()).unwrap_or(u32::MAX);
        let reset_after = log.runs.back().map_or(Duration::ZERO, |newest| {
            let full_at = newest.at_ns.saturating_add(self.period_ns);
            Duration::from_nanos(full_at.saturating_sub(real_now_ns))
        });
        Snapshot::new(self.limit, self.limit.saturating_sub(used), reset_after)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::SlidingLog;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use crate::snapshot::Snapshot;
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
    fn test_retry_after_for_multi_unit_request_spans_coalesced_runs() {
        let sl = SlidingLog::new(&Quota::per_second(10));
        assert!(sl.acquire(4, at(0)).is_allow()); // one run of 4 at t=0
        assert!(sl.acquire(4, at(200)).is_allow()); // one run of 4 at t=200
        assert!(sl.acquire(2, at(400)).is_allow()); // full: 10 in window
        // 6 more need 6 to age out: the t=0 run (4) and two of the t=200 run,
        // so the wait ends when the t=200 run leaves at t=1200.
        assert_eq!(sl.acquire(6, at(500)).retry_after(), Some(at(700)));
        // 4 more need only the t=0 run to leave, at t=1000.
        assert_eq!(sl.acquire(4, at(500)).retry_after(), Some(at(500)));
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

    #[test]
    fn test_large_check_n_is_one_record() {
        // A single huge request used to push one timestamp per unit (hundreds
        // of megabytes for 50M units). Coalesced, it is one record.
        let sl = SlidingLog::new(&Quota::per_second(u32::MAX));
        assert!(sl.acquire(50_000_000, at(0)).is_allow());
        assert_eq!(super::sync::lock(&sl.log).runs.len(), 1);
        assert!(sl.acquire(50_000_000, at(0)).is_allow());
        assert_eq!(super::sync::lock(&sl.log).runs.len(), 1);
    }

    #[test]
    fn test_late_thread_never_unsorts_the_log() {
        let sl = SlidingLog::new(&Quota::per_second(2));
        assert!(sl.acquire(1, at(800)).is_allow());
        // A late arrival stamped t=100 is recorded at t=800, so the window is
        // still judged in order and the unit ages out with its neighbour.
        assert!(sl.acquire(1, at(100)).is_allow());
        assert!(sl.acquire(1, at(900)).is_deny());
        assert!(sl.acquire(1, at(1_800)).is_allow());
    }

    #[test]
    fn test_zero_period_denies_everything() {
        let quota = Quota::from_parts(10, Duration::ZERO, 10);
        let sl = SlidingLog::new(&quota);
        assert_eq!(sl.acquire(1, at(0)).retry_after(), Some(Duration::MAX));
        assert_eq!(sl.peek(at(0)), Snapshot::closed());
    }

    #[test]
    fn test_peek_reports_remaining_and_full_recovery() {
        let sl = SlidingLog::new(&Quota::per_second(5));
        assert_eq!(sl.peek(at(0)), Snapshot::full(5));
        assert!(sl.acquire(2, at(0)).is_allow());
        assert!(sl.acquire(1, at(400)).is_allow());
        // 3 in window; the newest (t=400) leaves at t=1400.
        let snapshot = sl.peek(at(500));
        assert_eq!((snapshot.remaining(), snapshot.reset_after()), (2, at(900)));
        // At t=1000 the t=0 run has aged out.
        let snapshot = sl.peek(at(1_000));
        assert_eq!((snapshot.remaining(), snapshot.reset_after()), (4, at(400)));
        assert_eq!(sl.peek(at(1_400)), Snapshot::full(5));
    }
}
