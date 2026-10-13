//! Leaky bucket — the Generic Cell Rate Algorithm (GCRA).
//!
//! Where a token bucket lets a full burst through and then sustains the rate,
//! the leaky bucket *spaces* admitted units at least one emission interval
//! apart, smoothing traffic toward a steady output rate while still tolerating a
//! burst up to the quota's `burst`. The whole state is one atomic "theoretical
//! arrival time" advanced by compare-and-swap, so the per-key check is
//! lock-free.
//!
//! # Exact units
//!
//! Time is accounted in *emission intervals* rather than nanoseconds: one unit
//! of the theoretical arrival time is exactly `period / limit`. An interval in
//! whole nanoseconds would truncate (`period / limit` rounds down), which
//! over-admits — 600 M/s became 1 G/s, a 1.67x overshoot — and collapses to zero
//! once the rate passes one per nanosecond. In interval units every admitted
//! unit costs exactly one, so there is no drift at any rate. The current time is
//! converted with a precomputed 64.64 fixed-point reciprocal (two multiplies,
//! no division on the check path) and corrected to the exact floor.

use core::time::Duration;

use crate::decision::Decision;
use crate::quota::Quota;
use crate::snapshot::Snapshot;
use crate::sync::{AtomicU64, Ordering};

/// Clamps a duration-since-epoch to nanoseconds, saturating rather than wrapping
/// past the ~584-year `u64` ceiling.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// GCRA leaky-bucket state for one key.
pub(crate) struct LeakyBucket {
    /// Theoretical arrival time of the next conforming unit, in emission
    /// intervals since the limiter epoch.
    tat: AtomicU64,
    /// Effective `limit` (units per period), clamped to at most one unit per
    /// nanosecond so an interval is never shorter than the clock resolution.
    limit: u64,
    /// Period in nanoseconds.
    period_ns: u64,
    /// `limit * 2^64 / period_ns`, the ns→interval reciprocal. At most `2^64`
    /// (exactly `2^64` when one interval is one nanosecond).
    per_ns: u128,
    /// Delay tolerance in intervals: `burst - 1`, the slack a burst rides on.
    tolerance: u64,
    /// Set when the quota admits nothing (`limit == 0`, `burst == 0`, or a zero
    /// period).
    deny_all: bool,
}

impl LeakyBucket {
    pub(crate) fn new(quota: &Quota) -> Self {
        let period_ns = nanos(quota.period());
        let burst = quota.burst();
        // Fail closed on a rate finer than one unit per nanosecond: clamp it to
        // exactly one per nanosecond (admit less, never more).
        let limit = u64::from(quota.limit()).min(period_ns);
        let deny_all = limit == 0 || burst == 0;
        let per_ns = if deny_all {
            0
        } else {
            (u128::from(limit) << 64) / u128::from(period_ns)
        };
        Self {
            // Interval 0 is at or before any `now`, so a fresh key is full.
            tat: AtomicU64::new(0),
            limit,
            period_ns,
            per_ns,
            tolerance: u64::from(burst.saturating_sub(1)),
            deny_all,
        }
    }

    /// `floor(now_ns * limit / period_ns)` — the current time in intervals.
    ///
    /// The reciprocal estimate is low by at most one (`per_ns` is rounded down,
    /// and the error term `now_ns / 2^64` is below one), so one exact multiply
    /// comparison corrects it. With `limit <= period_ns` the result is at most
    /// `now_ns`, so it always fits.
    fn intervals_at(&self, now_ns: u64) -> u64 {
        let wide = u128::from(now_ns);
        let estimate = ((wide * self.per_ns) >> 64) as u64;
        let next = u128::from(estimate) + 1;
        if next * u128::from(self.period_ns) <= wide * u128::from(self.limit) {
            estimate + 1
        } else {
            estimate
        }
    }

    /// The first nanosecond at which interval `interval` has begun:
    /// `ceil(interval * period_ns / limit)`. Off the hot path (denials only).
    fn nanos_at(&self, interval: u64) -> u64 {
        let scaled = u128::from(interval) * u128::from(self.period_ns);
        let ns = scaled.div_ceil(u128::from(self.limit));
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        // A request wider than the burst tolerance can never conform, even
        // from an empty bucket.
        let span = u64::from(n - 1);
        if self.deny_all || span > self.tolerance {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let ceiling = now_iv.saturating_add(self.tolerance);
        loop {
            let tat = self.tat.load(Ordering::Acquire);
            // A late thread (stale `now`) only ever sees a later TAT here, which
            // makes it stricter, never looser.
            let effective = tat.max(now_iv);
            // Conforms iff `effective + span <= now + tolerance`.
            if effective.saturating_add(span) <= ceiling {
                let new_tat = effective.saturating_add(u64::from(n));
                if self
                    .tat
                    .compare_exchange_weak(tat, new_tat, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Decision::Allow;
                }
                core::hint::spin_loop();
                continue;
            }
            // Denied: the request conforms once `now` reaches this interval.
            let ready_iv = effective.saturating_add(span) - self.tolerance;
            let ready_ns = self.nanos_at(ready_iv);
            return Decision::Deny {
                retry_after: Duration::from_nanos(ready_ns.saturating_sub(now_ns).max(1)),
            };
        }
    }

    /// Units that would conform right now, and the wait until the bucket has
    /// drained back to its full burst.
    pub(crate) fn peek(&self, now: Duration) -> Snapshot {
        if self.deny_all {
            return Snapshot::closed();
        }
        let burst = self.tolerance + 1;
        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let tat = self.tat.load(Ordering::Acquire).max(now_iv);
        // `k` units conform iff `tat + (k - 1) <= now + tolerance`.
        let remaining = now_iv.saturating_add(burst).saturating_sub(tat);
        let reset_after = if tat == now_iv {
            Duration::ZERO
        } else {
            Duration::from_nanos(self.nanos_at(tat).saturating_sub(now_ns))
        };
        let burst = u32::try_from(burst).unwrap_or(u32::MAX);
        Snapshot::new(
            burst,
            u32::try_from(remaining).unwrap_or(u32::MAX),
            reset_after,
        )
    }

    /// Drains the bucket: the key has its full burst again.
    pub(crate) fn reset(&self) {
        self.tat.store(0, Ordering::Release);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::LeakyBucket;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use crate::snapshot::Snapshot;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_admits_burst_then_spaces() {
        // 10 per second, burst 10 → one unit every 100ms after the burst.
        let lb = LeakyBucket::new(&Quota::per_second(10));
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
        let lb = LeakyBucket::new(&Quota::per_second(10));
        for _ in 0..10 {
            assert!(lb.acquire(1, at(0)).is_allow());
        }
        // The next unit conforms exactly 100ms out.
        assert_eq!(lb.acquire(1, at(0)).retry_after(), Some(at(100)));
        // Waiting exactly that long is enough.
        assert!(lb.acquire(1, at(100)).is_allow());
    }

    #[test]
    fn test_retry_after_is_honest_for_non_integral_intervals() {
        // 3 per second: the interval is 333_333_333.3ns — not a whole number.
        let lb = LeakyBucket::new(&Quota::per_second(3).with_burst(1));
        assert!(lb.acquire(1, Duration::ZERO).is_allow());
        let wait = lb
            .acquire(1, Duration::ZERO)
            .retry_after()
            .unwrap_or_default();
        assert_eq!(wait, Duration::from_nanos(333_333_334));
        assert!(lb.acquire(1, wait - Duration::from_nanos(1)).is_deny());
        assert!(lb.acquire(1, wait).is_allow());
    }

    #[test]
    fn test_request_larger_than_burst_never_conforms() {
        let lb = LeakyBucket::new(&Quota::per_second(5));
        assert_eq!(
            lb.acquire(6, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }

    #[test]
    fn test_zero_limit_denies() {
        let lb = LeakyBucket::new(&Quota::per_second(0));
        assert!(lb.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_zero_burst_and_zero_period_deny() {
        let lb = LeakyBucket::new(&Quota::per_second(5).with_burst(0));
        assert_eq!(lb.acquire(1, at(0)).retry_after(), Some(Duration::MAX));
        let lb = LeakyBucket::new(&Quota::from_parts(5, Duration::ZERO, 5));
        assert_eq!(lb.acquire(1, at(0)).retry_after(), Some(Duration::MAX));
    }

    #[test]
    fn test_peek_reports_conforming_units_and_drain_time() {
        let lb = LeakyBucket::new(&Quota::per_second(10));
        assert_eq!(lb.peek(at(0)), Snapshot::full(10));
        assert!(lb.acquire(4, at(0)).is_allow());
        let snapshot = lb.peek(at(0));
        assert_eq!(snapshot.remaining(), 6);
        // Four intervals of 100ms to drain back to empty.
        assert_eq!(snapshot.reset_after(), at(400));
        assert_eq!(lb.peek(at(150)).remaining(), 7);
        assert_eq!(lb.peek(at(400)), Snapshot::full(10));
    }

    #[test]
    fn test_reset_restores_the_full_burst() {
        let lb = LeakyBucket::new(&Quota::per_second(3));
        assert!(lb.acquire(3, at(0)).is_allow());
        assert!(lb.acquire(1, at(0)).is_deny());
        lb.reset();
        assert!(lb.acquire(3, at(0)).is_allow());
        assert_eq!(
            LeakyBucket::new(&Quota::per_second(0)).peek(at(0)),
            Snapshot::closed()
        );
    }

    #[test]
    fn test_zero_units_always_admitted() {
        let lb = LeakyBucket::new(&Quota::per_second(1));
        assert_eq!(lb.acquire(0, at(0)), Decision::Allow);
    }

    #[test]
    fn test_rate_above_one_per_ns_is_clamped_not_unlimited() {
        // 2e9 per second used to truncate the interval to zero and admit
        // everything. Clamped to one per nanosecond, a frozen clock admits
        // exactly the burst.
        let lb = LeakyBucket::new(&Quota::per_second(2_000_000_000).with_burst(1));
        let admitted = (0..10_000)
            .filter(|_| lb.acquire(1, at(5)).is_allow())
            .count();
        assert_eq!(admitted, 1);
    }

    #[test]
    fn test_high_rate_does_not_over_admit_from_truncation() {
        // 6e8 per second (interval 1.67ns) used to round down to 1ns and admit
        // ~1.67x the rate. Over 1ms it must admit at most burst + 600_000.
        let lb = LeakyBucket::new(&Quota::per_second(600_000_000).with_burst(1));
        let mut admitted = 0u64;
        for ns in 0..1_000_000u64 {
            if lb.acquire(1, Duration::from_nanos(ns)).is_allow() {
                admitted += 1;
            }
        }
        assert!(admitted <= 600_001, "admitted {admitted}");
        assert!(admitted >= 599_999, "under-admitted {admitted}");
    }

    #[test]
    fn test_interval_conversion_is_exact_floor() {
        let lb = LeakyBucket::new(&Quota::per_second(3));
        for ns in [
            0u64,
            1,
            333_333_333,
            333_333_334,
            999_999_999,
            1_000_000_000,
            u64::MAX,
        ] {
            let exact = (u128::from(ns) * 3 / 1_000_000_000) as u64;
            assert_eq!(lb.intervals_at(ns), exact, "ns={ns}");
        }
        let one_per_ns = LeakyBucket::new(&Quota::from_parts(1_000, Duration::from_micros(1), 1));
        assert_eq!(one_per_ns.intervals_at(u64::MAX), u64::MAX);
    }
}
