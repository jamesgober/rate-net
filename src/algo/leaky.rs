//! Leaky bucket — the Generic Cell Rate Algorithm (GCRA).
//!
//! Where a token bucket lets a full burst through and then sustains the rate,
//! the leaky bucket *spaces* admitted units at least one emission interval
//! apart, smoothing traffic toward a steady output rate while still tolerating a
//! burst up to the quota's `burst`. The whole state is one atomic "theoretical
//! arrival time" advanced by compare-and-swap, so the check path is lock-free.

use core::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::decision::Decision;
use crate::quota::Quota;

/// Clamps a duration-since-epoch to nanoseconds, saturating rather than wrapping
/// past the ~584-year `u64` ceiling.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// GCRA leaky-bucket state for one key.
pub(crate) struct LeakyBucket {
    /// Theoretical arrival time of the next conforming unit, ns since the
    /// limiter epoch.
    tat_ns: AtomicU64,
    /// Emission interval per unit, ns (`period / limit`). Zero means the rate
    /// exceeds nanosecond resolution — admit freely.
    interval_ns: u64,
    /// Delay tolerance, ns: `(burst - 1) * interval`, the slack a burst rides on.
    tolerance_ns: u64,
    /// Set when the quota admits nothing (`limit == 0` or `burst == 0`).
    deny_all: bool,
}

impl LeakyBucket {
    pub(crate) fn new(quota: &Quota, now: Duration) -> Self {
        let limit = quota.limit();
        let burst = quota.burst();
        if limit == 0 || burst == 0 {
            return Self {
                tat_ns: AtomicU64::new(0),
                interval_ns: 0,
                tolerance_ns: 0,
                deny_all: true,
            };
        }
        let period_ns = nanos(quota.period());
        let interval_ns = period_ns / u64::from(limit);
        let tolerance_ns = u64::from(burst - 1).saturating_mul(interval_ns);
        Self {
            // Start "empty": the first arrival conforms immediately.
            tat_ns: AtomicU64::new(nanos(now)),
            interval_ns,
            tolerance_ns,
            deny_all: false,
        }
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        if self.deny_all {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }
        if self.interval_ns == 0 {
            // The rate is finer than nanosecond resolution — effectively
            // unlimited at the clock granularity we have.
            return Decision::Allow;
        }

        let now_ns = nanos(now);
        let interval = self.interval_ns;
        // Time the n units occupy beyond the first: (n - 1) * interval.
        let span = u64::from(n - 1).saturating_mul(interval);

        // A request wider than the burst tolerance can never conform, even from
        // an empty bucket.
        if span > self.tolerance_ns {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        loop {
            let tat = self.tat_ns.load(Ordering::Acquire);
            let effective = tat.max(now_ns);
            // Conforms iff `effective + span <= now + tolerance`.
            if effective.saturating_add(span) <= now_ns.saturating_add(self.tolerance_ns) {
                let new_tat = effective.saturating_add(u64::from(n).saturating_mul(interval));
                match self.tat_ns.compare_exchange_weak(
                    tat,
                    new_tat,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Decision::Allow,
                    Err(_) => continue, // contended; re-read and retry
                }
            }
            // Denied: the request conforms once enough time passes.
            let ready = effective
                .saturating_add(span)
                .saturating_sub(self.tolerance_ns);
            return Decision::Deny {
                retry_after: Duration::from_nanos(ready.saturating_sub(now_ns)),
            };
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::LeakyBucket;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_admits_burst_then_spaces() {
        // 10 per second, burst 10 → one unit every 100ms after the burst.
        let lb = LeakyBucket::new(&Quota::per_second(10), at(0));
        for _ in 0..10 {
            assert_eq!(lb.acquire(1, at(0)), Decision::Allow);
        }
        // 11th at the same instant is denied.
        assert!(lb.acquire(1, at(0)).is_deny());
        // After 100ms exactly one more conforms.
        assert_eq!(lb.acquire(1, at(100)), Decision::Allow);
        assert!(lb.acquire(1, at(100)).is_deny());
        assert_eq!(lb.acquire(1, at(200)), Decision::Allow);
    }

    #[test]
    fn test_retry_after_points_at_next_conforming_instant() {
        let lb = LeakyBucket::new(&Quota::per_second(10), at(0));
        for _ in 0..10 {
            assert!(lb.acquire(1, at(0)).is_allow());
        }
        match lb.acquire(1, at(0)) {
            Decision::Deny { retry_after } => {
                // Next unit conforms ~100ms out.
                assert!(retry_after <= Duration::from_millis(100));
                assert!(retry_after >= Duration::from_millis(99));
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn test_request_larger_than_burst_never_conforms() {
        let lb = LeakyBucket::new(&Quota::per_second(5), at(0));
        assert_eq!(
            lb.acquire(6, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }

    #[test]
    fn test_zero_limit_denies() {
        let lb = LeakyBucket::new(&Quota::per_second(0), at(0));
        assert!(lb.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_zero_units_always_admitted() {
        let lb = LeakyBucket::new(&Quota::per_second(1), at(0));
        assert_eq!(lb.acquire(0, at(0)), Decision::Allow);
    }
}
