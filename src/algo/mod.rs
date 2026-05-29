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

#[cfg(feature = "algorithms")]
mod fixed_window;
#[cfg(feature = "algorithms")]
mod leaky;
#[cfg(feature = "algorithms")]
mod sliding_counter;
#[cfg(feature = "algorithms")]
mod sliding_log;

#[cfg(feature = "algorithms")]
use self::fixed_window::FixedWindow;
#[cfg(feature = "algorithms")]
use self::leaky::LeakyBucket;
#[cfg(feature = "algorithms")]
use self::sliding_counter::SlidingCounter;
#[cfg(feature = "algorithms")]
use self::sliding_log::SlidingLog;

/// Per-key state for whichever algorithm the limiter runs.
pub(crate) enum AlgoState<C: Clock> {
    TokenBucket(Bucket<C>),
    #[cfg(feature = "algorithms")]
    LeakyBucket(LeakyBucket),
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
            Algorithm::LeakyBucket => Self::LeakyBucket(LeakyBucket::new(quota, now)),
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
    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        match self {
            Self::TokenBucket(bucket) => bucket.acquire(n).into(),
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket(state) => state.acquire(n, now),
            #[cfg(feature = "algorithms")]
            Self::FixedWindow(state) => state.acquire(n, now),
            #[cfg(feature = "algorithms")]
            Self::SlidingLog(state) => state.acquire(n, now),
            #[cfg(feature = "algorithms")]
            Self::SlidingCounter(state) => state.acquire(n, now),
        }
    }
}

/// Builds a token bucket honouring the quota's `burst` (capacity) and `limit`
/// (refill rate). A capacity that differs from the refill needs the validating
/// config path; a zero limit or burst yields a bucket that grants nothing.
fn token_bucket<C: Clock>(quota: &Quota, clock: C) -> Bucket<C> {
    let limit = quota.limit();
    let burst = quota.burst();
    let period = quota.period();

    if limit == 0 || burst == 0 {
        return Bucket::per_duration(0, period).with_clock(clock);
    }
    if burst == limit {
        return Bucket::per_duration(limit, period).with_clock(clock);
    }
    match BucketConfig::new(burst, limit, period, burst) {
        Ok(config) => Bucket::from_config(config).with_clock(clock),
        // `period` is non-zero by `Quota`'s construction, so this never happens
        // in practice; degrade to the simple bucket rather than panic.
        Err(_) => Bucket::per_duration(limit, period).with_clock(clock),
    }
}
