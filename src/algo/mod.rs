//! Per-key algorithm state and dispatch.
//!
//! Each key's state is one variant of [`AlgoState`], chosen by the limiter's
//! [`Algorithm`]. The token bucket is always available and delegates to
//! [`better_bucket`]; the leaky bucket and the window algorithms are compiled in
//! under the `algorithms` feature. Dispatch is a plain `match` (no boxing, no
//! vtable), so adding algorithms costs nothing on the token-bucket path.

use core::time::Duration;

use better_bucket::{Bucket, BucketConfig};
use clock_lib::Clock;

use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::quota::Quota;
use crate::snapshot::Snapshot;

#[cfg(feature = "algorithms")]
mod fixed_window;
#[cfg(feature = "algorithms")]
mod sliding_counter;
#[cfg(feature = "algorithms")]
mod sliding_log;

#[cfg(feature = "algorithms")]
use self::fixed_window::FixedWindow;
#[cfg(feature = "algorithms")]
use self::sliding_counter::SlidingCounter;
#[cfg(feature = "algorithms")]
use self::sliding_log::SlidingLog;
#[cfg(feature = "algorithms")]
use crate::gcra::Gcra;

/// Per-key state for whichever algorithm the limiter runs.
pub(crate) enum AlgoState<C: Clock> {
    TokenBucket(Bucket<C>),
    #[cfg(feature = "algorithms")]
    LeakyBucket(Gcra),
    #[cfg(feature = "algorithms")]
    FixedWindow(FixedWindow),
    #[cfg(feature = "algorithms")]
    SlidingLog(SlidingLog),
    #[cfg(feature = "algorithms")]
    SlidingCounter(SlidingCounter),
}

impl<C: Clock> AlgoState<C> {
    /// Builds fresh per-key state for `algorithm` under `quota`, anchored at the
    /// elapsed time `now`. `now` is unused on the token-bucket path (the bucket
    /// reads its own clock); the window algorithms anchor their first window to
    /// it.
    #[cfg_attr(not(feature = "algorithms"), allow(unused_variables))]
    pub(crate) fn new(algorithm: Algorithm, quota: &Quota, clock: C, now: Duration) -> Self {
        match algorithm {
            Algorithm::TokenBucket => Self::TokenBucket(token_bucket(quota, clock)),
            #[cfg(feature = "algorithms")]
            Algorithm::LeakyBucket => Self::LeakyBucket(Gcra::new(*quota)),
            #[cfg(feature = "algorithms")]
            Algorithm::FixedWindow => Self::FixedWindow(FixedWindow::new(quota, now)),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowLog => Self::SlidingLog(SlidingLog::new(quota)),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowCounter => {
                Self::SlidingCounter(SlidingCounter::new(quota, now))
            }
        }
    }

    /// Checks `n` units as of elapsed time `now`. `now` is unused on the
    /// token-bucket path (the bucket reads its own clock).
    #[cfg_attr(not(feature = "algorithms"), allow(unused_variables))]
    #[inline]
    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        match self {
            Self::TokenBucket(bucket) => bucket.acquire(n).into(),
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket(state) => state.check_n_at(n, now),
            #[cfg(feature = "algorithms")]
            Self::FixedWindow(state) => state.acquire(n, now),
            #[cfg(feature = "algorithms")]
            Self::SlidingLog(state) => state.acquire(n, now),
            #[cfg(feature = "algorithms")]
            Self::SlidingCounter(state) => state.acquire(n, now),
        }
    }

    /// The key's remaining allowance as of `now`, without consuming any.
    #[cfg_attr(not(feature = "algorithms"), allow(unused_variables))]
    pub(crate) fn peek(&self, now: Duration) -> Snapshot {
        match self {
            Self::TokenBucket(bucket) => {
                let capacity = bucket.capacity();
                if capacity == 0 {
                    return Snapshot::closed();
                }
                let reset_after = bucket.time_until(capacity).unwrap_or(Duration::MAX);
                Snapshot::new(capacity, bucket.available(), reset_after)
            }
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket(state) => state.peek_at(now),
            #[cfg(feature = "algorithms")]
            Self::FixedWindow(state) => state.peek(now),
            #[cfg(feature = "algorithms")]
            Self::SlidingLog(state) => state.peek(now),
            #[cfg(feature = "algorithms")]
            Self::SlidingCounter(state) => state.peek(now),
        }
    }

    /// Restores the key's full allowance as of now.
    pub(crate) fn reset(&self) {
        match self {
            Self::TokenBucket(bucket) => bucket.reset(),
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket(state) => state.reset(),
            #[cfg(feature = "algorithms")]
            Self::FixedWindow(state) => state.reset(),
            #[cfg(feature = "algorithms")]
            Self::SlidingLog(state) => state.reset(),
            #[cfg(feature = "algorithms")]
            Self::SlidingCounter(state) => state.reset(),
        }
    }

    /// Whether the key's state is indistinguishable from a fresh key's — its
    /// whole allowance is available — so forgetting it changes no decision.
    pub(crate) fn is_full(&self, now: Duration) -> bool {
        let snapshot = self.peek(now);
        snapshot.remaining() == snapshot.limit()
    }
}

/// What a key that has never been seen reports: its whole allowance under
/// `quota`, or nothing for a quota that admits nothing.
pub(crate) fn fresh_snapshot(algorithm: Algorithm, quota: &Quota) -> Snapshot {
    if quota.limit() == 0 || quota.period().is_zero() {
        return Snapshot::closed();
    }
    let capacity = match algorithm {
        Algorithm::TokenBucket => quota.burst(),
        #[cfg(feature = "algorithms")]
        Algorithm::LeakyBucket => quota.burst(),
        #[cfg(feature = "algorithms")]
        Algorithm::FixedWindow | Algorithm::SlidingWindowLog | Algorithm::SlidingWindowCounter => {
            quota.limit()
        }
    };
    if capacity == 0 {
        Snapshot::closed()
    } else {
        Snapshot::full(capacity)
    }
}

/// Builds a token bucket honouring the quota's `burst` (capacity) and `limit`
/// (refill rate), starting full, anchored at `clock`'s current reading. A zero
/// limit, burst, or period yields a bucket that grants nothing (fail closed).
fn token_bucket<C: Clock>(quota: &Quota, clock: C) -> Bucket<C> {
    let burst = quota.burst();
    match BucketConfig::new(burst, quota.limit(), quota.period(), burst) {
        Ok(config) => Bucket::with_config_and_clock(config, clock),
        // Only a zero limit, burst, or period is rejected: grant nothing.
        Err(_) => Bucket::per_duration(0, quota.period()).with_clock(clock),
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use core::time::Duration;
    use std::sync::Arc;

    use clock_lib::ManualClock;

    use super::{AlgoState, fresh_snapshot};
    use crate::algorithm::Algorithm;
    use crate::quota::Quota;
    use crate::snapshot::Snapshot;

    fn token(quota: Quota) -> (Arc<ManualClock>, AlgoState<Arc<ManualClock>>) {
        let clock = Arc::new(ManualClock::new());
        let state = AlgoState::new(
            Algorithm::TokenBucket,
            &quota,
            Arc::clone(&clock),
            Duration::ZERO,
        );
        (clock, state)
    }

    #[test]
    fn test_token_bucket_peek_reset_and_fullness() {
        let (clock, state) = token(Quota::per_second(10));
        assert_eq!(state.peek(Duration::ZERO), Snapshot::full(10));
        assert!(state.is_full(Duration::ZERO));
        assert!(state.acquire(4, Duration::ZERO).is_allow());
        let snapshot = state.peek(Duration::ZERO);
        assert_eq!(snapshot.remaining(), 6);
        // Four tokens at 10/s take 400ms to come back.
        assert_eq!(snapshot.reset_after(), Duration::from_millis(400));
        assert!(!state.is_full(Duration::ZERO));
        state.reset();
        assert!(state.is_full(Duration::ZERO));
        clock.advance(Duration::from_secs(1));
        assert!(state.is_full(Duration::ZERO));
    }

    #[test]
    fn test_token_bucket_honours_burst_and_fails_closed() {
        let (_clock, state) = token(Quota::per_second(10).with_burst(25));
        assert_eq!(state.peek(Duration::ZERO).limit(), 25);
        assert!(state.acquire(25, Duration::ZERO).is_allow());
        for degenerate in [
            Quota::per_second(0),
            Quota::per_second(5).with_burst(0),
            Quota::from_parts(5, Duration::ZERO, 5),
        ] {
            let (_clock, state) = token(degenerate);
            assert!(state.acquire(1, Duration::ZERO).is_deny());
            assert_eq!(state.peek(Duration::ZERO), Snapshot::closed());
        }
    }

    #[test]
    fn test_token_bucket_burst_above_the_old_packing_ceiling() {
        // better-bucket 1.0 capped capacity at ~4.29M tokens; 1.1 does not.
        let (_clock, state) = token(Quota::per_second(10_000_000));
        assert_eq!(state.peek(Duration::ZERO).limit(), 10_000_000);
        assert!(state.acquire(5_000_000, Duration::ZERO).is_allow());
        assert!(state.acquire(5_000_000, Duration::ZERO).is_allow());
        assert!(state.acquire(1, Duration::ZERO).is_deny());
    }

    #[test]
    fn test_fresh_snapshot_reports_capacity_per_algorithm() {
        let quota = Quota::per_second(10).with_burst(20);
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &quota),
            Snapshot::full(20)
        );
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &Quota::per_second(0)),
            Snapshot::closed()
        );
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &quota.with_burst(0)),
            Snapshot::closed()
        );
        #[cfg(feature = "algorithms")]
        {
            assert_eq!(
                fresh_snapshot(Algorithm::LeakyBucket, &quota),
                Snapshot::full(20)
            );
            for window in [
                Algorithm::FixedWindow,
                Algorithm::SlidingWindowLog,
                Algorithm::SlidingWindowCounter,
            ] {
                assert_eq!(fresh_snapshot(window, &quota), Snapshot::full(10));
            }
        }
    }
}
