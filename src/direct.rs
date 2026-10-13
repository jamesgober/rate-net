//! The un-keyed, single-stream limiter.

use core::fmt;
use core::time::Duration;

use clock_lib::{Clock, Monotonic, SystemClock};

use crate::algo::AlgoState;
use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::quota::Quota;
use crate::snapshot::Snapshot;

/// A rate limiter for one stream: no keys, no map, no hash, no lock.
///
/// Where [`RateLimiter`](crate::RateLimiter) tracks an allowance per key,
/// `DirectLimiter` holds exactly one — the right tool for client-side
/// throttling ("call this API at most 50 times a second"), for guarding one
/// shared resource, or for any loop that needs pacing. A check skips the hash,
/// the shard lookup, and the shard lock entirely and runs the algorithm's
/// accounting directly, so it is the cheapest check the crate offers.
///
/// It runs any [`Algorithm`] (the token bucket by default) on an injectable
/// [`Clock`]. Without the standard library, [`Gcra`](crate::Gcra) is the
/// clock-free equivalent.
///
/// # Examples
///
/// ```
/// use rate_net::DirectLimiter;
///
/// // At most 50 outbound calls per second.
/// let limiter = DirectLimiter::per_second(50);
/// if limiter.check().is_allow() {
///     // make the call
/// }
/// assert_eq!(limiter.peek().remaining(), 49);
/// ```
pub struct DirectLimiter<C: Clock + Clone = SystemClock> {
    state: AlgoState<C>,
    algorithm: Algorithm,
    quota: Quota,
    clock: C,
    epoch: Monotonic,
    /// The token bucket reads its own clock; every other algorithm needs the
    /// elapsed time passed in.
    reads_clock: bool,
}

impl DirectLimiter<SystemClock> {
    /// A direct limiter allowing `limit` units per second, driven by the OS
    /// monotonic clock. A `limit` of `0` denies everything.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// let limiter = DirectLimiter::per_second(2);
    /// assert!(limiter.check().is_allow());
    /// assert!(limiter.check().is_allow());
    /// assert!(limiter.check().is_deny());
    /// ```
    #[must_use]
    pub fn per_second(limit: u32) -> Self {
        Self::with_quota(Quota::per_second(limit))
    }

    /// A direct limiter allowing `limit` units per minute.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = DirectLimiter::per_minute(60);
    /// assert_eq!(limiter.quota().period(), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn per_minute(limit: u32) -> Self {
        Self::with_quota(Quota::per_minute(limit))
    }

    /// A direct token-bucket limiter for an explicit [`Quota`], driven by the
    /// OS monotonic clock. Infallible and fail-closed, like
    /// [`Builder::build`](crate::Builder::build); use
    /// [`Builder::try_build_direct`](crate::Builder::try_build_direct) to
    /// validate.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{DirectLimiter, Quota};
    ///
    /// let limiter = DirectLimiter::with_quota(Quota::per_second(10).with_burst(20));
    /// assert!(limiter.check_n(20).is_allow());
    /// ```
    #[must_use]
    pub fn with_quota(quota: Quota) -> Self {
        Self::build(Algorithm::default(), quota, SystemClock::new())
    }
}

impl<C: Clock + Clone> DirectLimiter<C> {
    /// Assembles a direct limiter. Shared with the builder.
    pub(crate) fn build(algorithm: Algorithm, quota: Quota, clock: C) -> Self {
        let epoch = clock.now();
        let state = AlgoState::new(algorithm, &quota, clock.clone(), Duration::ZERO);
        Self {
            state,
            algorithm,
            quota,
            clock,
            epoch,
            reads_clock: algorithm != Algorithm::TokenBucket,
        }
    }

    /// Replaces the time source, resetting the allowance to full. Intended
    /// immediately after construction — chiefly a `ManualClock` in tests.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    /// use clock_lib::ManualClock;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let clock = Arc::new(ManualClock::new());
    /// let limiter = DirectLimiter::per_second(1).with_clock(Arc::clone(&clock));
    /// assert!(limiter.check().is_allow());
    /// assert!(limiter.check().is_deny());
    /// clock.advance(Duration::from_secs(1));
    /// assert!(limiter.check().is_allow());
    /// ```
    #[must_use]
    pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> DirectLimiter<C2> {
        DirectLimiter::build(self.algorithm, self.quota, clock)
    }

    /// Selects the algorithm, resetting the allowance to full. The leaky
    /// bucket and window algorithms require the `algorithms` feature.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "algorithms")] {
    /// use rate_net::{Algorithm, DirectLimiter};
    ///
    /// let limiter = DirectLimiter::per_second(10).with_algorithm(Algorithm::LeakyBucket);
    /// assert_eq!(limiter.algorithm(), Algorithm::LeakyBucket);
    /// # }
    /// ```
    #[must_use]
    pub fn with_algorithm(self, algorithm: Algorithm) -> Self {
        Self::build(algorithm, self.quota, self.clock)
    }

    /// Checks a single unit. Equivalent to `check_n(1)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Decision, DirectLimiter};
    ///
    /// let limiter = DirectLimiter::per_second(1);
    /// assert_eq!(limiter.check(), Decision::Allow);
    /// assert!(limiter.check().is_deny());
    /// ```
    #[inline]
    pub fn check(&self) -> Decision {
        self.check_n(1)
    }

    /// Checks `n` units in one operation, all or nothing. Requesting `0`
    /// always succeeds; requesting more than the capacity can never succeed
    /// (the denial's `retry_after` is [`Duration::MAX`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = DirectLimiter::per_second(10);
    /// assert!(limiter.check_n(10).is_allow());
    /// assert_eq!(limiter.check_n(11).retry_after(), Some(Duration::MAX));
    /// ```
    #[inline]
    pub fn check_n(&self, n: u32) -> Decision {
        let now = if self.reads_clock {
            self.now()
        } else {
            Duration::ZERO
        };
        self.state.acquire(n, now)
    }

    /// Reads the remaining allowance without consuming any. See
    /// [`Snapshot`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// let limiter = DirectLimiter::per_second(5);
    /// let _ = limiter.check_n(2);
    /// assert_eq!(limiter.peek().remaining(), 3);
    /// ```
    #[must_use]
    pub fn peek(&self) -> Snapshot {
        self.state.peek(self.now())
    }

    /// Restores the full allowance immediately.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// let limiter = DirectLimiter::per_minute(1);
    /// assert!(limiter.check().is_allow());
    /// limiter.reset();
    /// assert!(limiter.check().is_allow());
    /// ```
    pub fn reset(&self) {
        self.state.reset();
    }

    /// The quota this limiter enforces, as configured.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// assert_eq!(DirectLimiter::per_second(9).quota().limit(), 9);
    /// ```
    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.quota
    }

    /// The algorithm this limiter applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Algorithm, DirectLimiter};
    ///
    /// assert_eq!(DirectLimiter::per_second(1).algorithm(), Algorithm::TokenBucket);
    /// ```
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// Monotonic elapsed time since this limiter's epoch.
    fn now(&self) -> Duration {
        self.clock.now().saturating_duration_since(self.epoch)
    }
}

#[cfg(feature = "async")]
impl<C: Clock + Clone> DirectLimiter<C> {
    /// Awaits until a single unit is admitted, or reports that it never can
    /// be. Equivalent to `try_until_ready_n(1)`. Requires the `async` feature.
    ///
    /// # Errors
    ///
    /// [`InsufficientCapacity`](crate::InsufficientCapacity) if the limiter can
    /// never admit even one unit.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// # async fn demo() -> Result<(), rate_net::InsufficientCapacity> {
    /// let limiter = DirectLimiter::per_second(20);
    /// for _ in 0..3 {
    ///     limiter.try_until_ready().await?; // paces the loop to 20/s
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn try_until_ready(&self) -> Result<(), crate::InsufficientCapacity> {
        self.try_until_ready_n(1).await
    }

    /// Awaits until `n` units are admitted, sleeping for the reported
    /// `retry_after` after each denial. Cancel-safe: dropping the future
    /// consumes nothing. Requires the `async` feature.
    ///
    /// # Errors
    ///
    /// [`InsufficientCapacity`](crate::InsufficientCapacity) if `n` exceeds
    /// what the limit can ever grant at once.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::DirectLimiter;
    ///
    /// # async fn demo() {
    /// let limiter = DirectLimiter::per_second(5);
    /// assert!(limiter.try_until_ready_n(5).await.is_ok());
    /// assert!(limiter.try_until_ready_n(6).await.is_err());
    /// # }
    /// ```
    pub async fn try_until_ready_n(&self, n: u32) -> Result<(), crate::InsufficientCapacity> {
        loop {
            match self.check_n(n) {
                Decision::Allow => return Ok(()),
                Decision::Deny { retry_after } => {
                    if retry_after == Duration::MAX {
                        return Err(crate::InsufficientCapacity::new(n));
                    }
                    tokio::time::sleep(retry_after).await;
                }
            }
        }
    }
}

impl<C: Clock + Clone> fmt::Debug for DirectLimiter<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectLimiter")
            .field("algorithm", &self.algorithm)
            .field("quota", &self.quota)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use clock_lib::ManualClock;

    use super::DirectLimiter;
    use crate::algorithm::Algorithm;
    use crate::quota::Quota;

    fn manual(limit: u32) -> (Arc<ManualClock>, DirectLimiter<Arc<ManualClock>>) {
        let clock = Arc::new(ManualClock::new());
        let limiter = DirectLimiter::per_second(limit).with_clock(Arc::clone(&clock));
        (clock, limiter)
    }

    #[test]
    fn test_direct_limiter_admits_quota_then_refills() {
        let (clock, limiter) = manual(3);
        for _ in 0..3 {
            assert!(limiter.check().is_allow());
        }
        assert!(limiter.check().is_deny());
        clock.advance(Duration::from_secs(1));
        assert!(limiter.check_n(3).is_allow());
    }

    #[test]
    fn test_direct_limiter_peek_reset_and_introspection() {
        let (_clock, limiter) = manual(4);
        assert_eq!(limiter.peek().remaining(), 4);
        assert!(limiter.check_n(4).is_allow());
        assert!(limiter.peek().is_exhausted());
        limiter.reset();
        assert_eq!(limiter.peek().remaining(), 4);
        assert_eq!(limiter.quota(), Quota::per_second(4));
        assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
        assert!(format!("{limiter:?}").contains("DirectLimiter"));
    }

    #[test]
    fn test_direct_limiter_fails_closed() {
        let limiter = DirectLimiter::per_second(0);
        assert!(limiter.check().is_deny());
        assert_eq!(limiter.peek().limit(), 0);
        assert_eq!(DirectLimiter::per_minute(1).quota().limit(), 1);
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_direct_limiter_runs_every_algorithm() {
        for algorithm in [
            Algorithm::TokenBucket,
            Algorithm::LeakyBucket,
            Algorithm::FixedWindow,
            Algorithm::SlidingWindowLog,
            Algorithm::SlidingWindowCounter,
        ] {
            let clock = Arc::new(ManualClock::new());
            let limiter = DirectLimiter::per_second(5)
                .with_algorithm(algorithm)
                .with_clock(Arc::clone(&clock));
            let admitted = (0..20).filter(|_| limiter.check().is_allow()).count();
            assert_eq!(admitted, 5, "{algorithm:?}");
            clock.advance(Duration::from_secs(2));
            assert!(limiter.check().is_allow(), "{algorithm:?} did not refill");
        }
    }

    #[cfg(feature = "async")]
    #[tokio::test]
    async fn test_direct_limiter_try_until_ready() {
        let limiter = DirectLimiter::per_second(200);
        assert!(limiter.check_n(200).is_allow());
        let waited = tokio::time::timeout(Duration::from_secs(2), limiter.try_until_ready()).await;
        assert_eq!(waited.unwrap(), Ok(()));
        assert_eq!(
            limiter.try_until_ready_n(201).await,
            Err(crate::InsufficientCapacity::new(201))
        );
    }
}
