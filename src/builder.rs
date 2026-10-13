//! The Tier-2 builder.

use std::fmt;
use std::time::Duration;

use clock_lib::{Clock, SystemClock};

use crate::algorithm::Algorithm;
use crate::direct::DirectLimiter;
use crate::error::RateLimiterError;
use crate::eviction::Eviction;
use crate::limiter::{RateLimiter, default_shard_count};
use crate::quota::Quota;

/// A fluent builder for a [`RateLimiter`] when the Tier-1 constructors are not
/// enough.
///
/// Start it with [`RateLimiter::builder`], chain the knobs you care about —
/// algorithm, quota, burst, shard count, eviction policy, clock — and call
/// [`try_build`](Self::try_build) (or [`build`](Self::build)). Anything left
/// unset keeps a sane default: the token bucket, default sharding, and the
/// bounded-memory [`Eviction`] default. The quota defaults to a limit of `0`
/// (which denies everything), so set it.
///
/// Two ways to finish:
///
/// - [`try_build`](Self::try_build) validates the configuration and returns a
///   [`RateLimiterError`] naming the first problem — a zero limit, period, or
///   burst, a rate or limit beyond the algorithm's ceiling, or an idle TTL
///   shorter than the quota's recovery time. Prefer it whenever the values come
///   from configuration.
/// - [`build`](Self::build) is infallible and **fails closed**: a zero limit,
///   zero period, or zero burst yields a limiter that denies everything (for
///   every algorithm), a leaky-bucket rate above one unit per nanosecond is
///   clamped down to it, and an idle TTL is raised to the recovery time. It
///   never produces a limiter that admits more than configured.
///
/// # Examples
///
/// ```
/// use rate_net::{RateLimiter, Eviction};
/// use std::time::Duration;
///
/// let limiter = RateLimiter::builder()
///     .quota(1000, Duration::from_secs(60)) // 1000 / minute
///     .burst(50)                            // allow short bursts of 50
///     .shards(64)                           // tune for core count
///     .eviction(Eviction::idle(Duration::from_secs(300)))
///     .build();
///
/// assert_eq!(limiter.quota().limit(), 1000);
/// assert_eq!(limiter.quota().burst(), 50);
/// assert_eq!(limiter.shards(), 64);
/// ```
#[must_use = "a builder does nothing until `.build()` is called"]
pub struct Builder<C: Clock + Clone = SystemClock> {
    algorithm: Algorithm,
    limit: u32,
    period: Duration,
    burst: Option<u32>,
    shards: Option<usize>,
    eviction: Eviction,
    clock: C,
}

impl Builder<SystemClock> {
    /// Creates a builder with default settings, driven by the OS monotonic
    /// clock. Usually reached through [`RateLimiter::builder`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Builder;
    ///
    /// let limiter = Builder::new().per_second(10).try_build()?;
    /// assert_eq!(limiter.quota().limit(), 10);
    /// # Ok::<(), rate_net::RateLimiterError>(())
    /// ```
    pub fn new() -> Self {
        Self {
            algorithm: Algorithm::default(),
            limit: 0,
            period: Duration::from_secs(1),
            burst: None,
            shards: None,
            eviction: Eviction::default(),
            clock: SystemClock::new(),
        }
    }
}

impl Default for Builder<SystemClock> {
    /// Same as [`Builder::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock + Clone> fmt::Debug for Builder<C> {
    /// Shows the configuration; the clock is omitted (clocks need not be
    /// `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Builder")
            .field("algorithm", &self.algorithm)
            .field("limit", &self.limit)
            .field("period", &self.period)
            .field("burst", &self.burst)
            .field("shards", &self.shards)
            .field("eviction", &self.eviction)
            .finish_non_exhaustive()
    }
}

impl<C: Clock + Clone> Builder<C> {
    /// Selects the algorithm. Defaults to [`Algorithm::TokenBucket`]. The leaky
    /// bucket and window algorithms require the `algorithms` feature.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "algorithms")] {
    /// use rate_net::{RateLimiter, Algorithm};
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder()
    ///     .algorithm(Algorithm::SlidingWindowCounter)
    ///     .quota(100, Duration::from_secs(1))
    ///     .build();
    /// assert_eq!(limiter.algorithm(), Algorithm::SlidingWindowCounter);
    /// # }
    /// ```
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Sets the quota: `limit` requests per `period`, per key.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder().quota(5, Duration::from_millis(100)).build();
    /// assert_eq!(limiter.quota().limit(), 5);
    /// ```
    pub fn quota(mut self, limit: u32, period: Duration) -> Self {
        self.limit = limit;
        self.period = period;
        self
    }

    /// Sets the quota to `limit` requests per second.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::builder().per_second(100).build();
    /// assert_eq!(limiter.quota().limit(), 100);
    /// ```
    pub fn per_second(self, limit: u32) -> Self {
        self.quota(limit, Duration::from_secs(1))
    }

    /// Sets the quota to `limit` requests per minute.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder().per_minute(600).build();
    /// assert_eq!(limiter.quota().period(), Duration::from_secs(60));
    /// ```
    pub fn per_minute(self, limit: u32) -> Self {
        self.quota(limit, Duration::from_secs(60))
    }

    /// Sets the burst ceiling. Defaults to the quota's limit. Applies to the
    /// token and leaky buckets; the window algorithms admit at most `limit` per
    /// period and ignore it.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::builder().per_second(100).burst(250).build();
    /// assert_eq!(limiter.quota().burst(), 250);
    /// ```
    pub fn burst(mut self, burst: u32) -> Self {
        self.burst = Some(burst);
        self
    }

    /// Sets the shard count (rounded up to a power of two). Defaults to a small
    /// multiple of the core count. `try_build` rejects `0` and anything above
    /// [`MAX_SHARDS`](crate::MAX_SHARDS); `build` clamps into that range.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::builder().per_second(1).shards(128).build();
    /// assert_eq!(limiter.shards(), 128);
    /// ```
    pub fn shards(mut self, shards: usize) -> Self {
        self.shards = Some(shards);
        self
    }

    /// Sets the eviction policy. Defaults to a bounded-memory capacity cap.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Eviction};
    ///
    /// let limiter = RateLimiter::builder().per_second(1).eviction(Eviction::capacity(1_000)).build();
    /// assert_eq!(limiter.eviction().max_keys(), Some(1_000));
    /// ```
    pub fn eviction(mut self, eviction: Eviction) -> Self {
        self.eviction = eviction;
        self
    }

    /// Sets the time source, for deterministic tests with a `ManualClock`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use clock_lib::ManualClock;
    /// use std::sync::Arc;
    ///
    /// let clock = Arc::new(ManualClock::new());
    /// let limiter = RateLimiter::builder().per_second(5).clock(Arc::clone(&clock)).build();
    /// assert!(limiter.check("k").is_allow());
    /// ```
    pub fn clock<C2: Clock + Clone>(self, clock: C2) -> Builder<C2> {
        Builder {
            algorithm: self.algorithm,
            limit: self.limit,
            period: self.period,
            burst: self.burst,
            shards: self.shards,
            eviction: self.eviction,
            clock,
        }
    }

    /// Builds the configured limiter without validation, failing closed.
    ///
    /// A configuration that cannot describe a working limit — a zero limit,
    /// period, or burst — produces a limiter that denies every request, for
    /// every algorithm. A leaky-bucket rate finer than one unit per nanosecond
    /// is clamped to it, and an idle TTL shorter than the quota's recovery time
    /// is raised to it (see [`try_build`](Self::try_build) for the rules). The
    /// result never admits more than configured; use `try_build` to be told
    /// about the problem instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder().quota(10, Duration::from_secs(1)).build();
    /// assert!(limiter.check("k").is_allow());
    ///
    /// // A zero period fails closed rather than admitting everything.
    /// let degenerate = RateLimiter::builder().quota(10, Duration::ZERO).build();
    /// assert!(degenerate.check("k").is_deny());
    /// ```
    #[must_use]
    pub fn build(self) -> RateLimiter<C> {
        let quota = self.quota_value();
        let shards = self.shards.unwrap_or_else(default_shard_count);
        RateLimiter::build(self.algorithm, quota, self.clock, shards, self.eviction)
    }

    /// Validates the configuration, then builds the limiter.
    ///
    /// # Errors
    ///
    /// - [`RateLimiterError::ZeroQuota`] — the limit is `0` (or no quota was
    ///   set).
    /// - [`RateLimiterError::ZeroPeriod`] — the period is zero.
    /// - [`RateLimiterError::ZeroBurst`] — the burst is `0` for the token or
    ///   leaky bucket.
    /// - [`RateLimiterError::RateTooHigh`] — a leaky-bucket rate above one unit
    ///   per nanosecond.
    /// - [`RateLimiterError::LimitTooLarge`] — a sliding-window-log limit above
    ///   [`MAX_SLIDING_LOG_LIMIT`](crate::MAX_SLIDING_LOG_LIMIT).
    /// - [`RateLimiterError::ShardCountOutOfRange`] — a shard count of `0` or
    ///   above [`MAX_SHARDS`](crate::MAX_SHARDS).
    /// - [`RateLimiterError::IdleTtlTooShort`] — an idle TTL shorter than the
    ///   time a key needs to recover its full allowance (`burst / limit`
    ///   periods for the buckets, one period for the fixed window and log, two
    ///   for the sliding-window counter). Evicting sooner would reset a key's
    ///   quota early.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Eviction, RateLimiter, RateLimiterError};
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder().per_second(100).try_build()?;
    /// assert_eq!(limiter.quota().limit(), 100);
    ///
    /// // Forgot the quota.
    /// assert_eq!(
    ///     RateLimiter::builder().try_build().unwrap_err(),
    ///     RateLimiterError::ZeroQuota,
    /// );
    ///
    /// // A 1 s idle TTL would reset a 10-per-minute key after one idle second.
    /// let err = RateLimiter::builder()
    ///     .per_minute(10)
    ///     .eviction(Eviction::idle(Duration::from_secs(1)))
    ///     .try_build()
    ///     .unwrap_err();
    /// assert_eq!(err, RateLimiterError::IdleTtlTooShort);
    /// # Ok::<(), RateLimiterError>(())
    /// ```
    pub fn try_build(self) -> Result<RateLimiter<C>, RateLimiterError> {
        self.validate()?;
        Ok(self.build())
    }

    /// Builds a [`DirectLimiter`] — one un-keyed allowance — from the
    /// algorithm, quota, burst, and clock, failing closed exactly like
    /// [`build`](Self::build). The shard count and eviction policy describe
    /// the keyed store and are ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::builder().per_second(50).burst(5).build_direct();
    /// assert!(limiter.check_n(5).is_allow());
    /// assert!(limiter.check().is_deny());
    /// ```
    #[must_use]
    pub fn build_direct(self) -> DirectLimiter<C> {
        let quota = self.quota_value();
        DirectLimiter::build(self.algorithm, quota, self.clock)
    }

    /// Validates the algorithm and quota, then builds a [`DirectLimiter`].
    ///
    /// # Errors
    ///
    /// The quota errors of [`try_build`](Self::try_build): a zero limit,
    /// period, or burst, or a rate or limit beyond the algorithm's ceiling.
    /// The eviction policy is not consulted (a direct limiter has no store).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, RateLimiterError};
    ///
    /// let limiter = RateLimiter::builder().per_second(10).try_build_direct()?;
    /// assert!(limiter.check().is_allow());
    ///
    /// assert_eq!(
    ///     RateLimiter::builder().try_build_direct().unwrap_err(),
    ///     RateLimiterError::ZeroQuota,
    /// );
    /// # Ok::<(), RateLimiterError>(())
    /// ```
    pub fn try_build_direct(self) -> Result<DirectLimiter<C>, RateLimiterError> {
        self.algorithm.validate(&self.quota_value())?;
        Ok(self.build_direct())
    }

    /// The configured quota, with the burst defaulting to the limit.
    fn quota_value(&self) -> Quota {
        let burst = self.burst.unwrap_or(self.limit);
        Quota::from_parts(self.limit, self.period, burst)
    }

    /// The strict checks behind [`try_build`](Self::try_build).
    fn validate(&self) -> Result<(), RateLimiterError> {
        let quota = self.quota_value();
        self.algorithm.validate(&quota)?;
        if self
            .shards
            .is_some_and(|shards| shards == 0 || shards > crate::store::MAX_SHARDS)
        {
            return Err(RateLimiterError::ShardCountOutOfRange);
        }
        if let Some(ttl) = self.eviction.idle_ttl() {
            if ttl < self.algorithm.recovery_time(&quota) {
                return Err(RateLimiterError::IdleTtlTooShort);
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use clock_lib::ManualClock;

    use super::Builder;
    use crate::algorithm::Algorithm;
    use crate::error::RateLimiterError;
    use crate::eviction::Eviction;
    use crate::limiter::RateLimiter;

    #[test]
    fn test_builder_defaults_to_token_bucket_and_set_quota() {
        let limiter = RateLimiter::builder()
            .quota(50, Duration::from_secs(1))
            .build();
        assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
        assert_eq!(limiter.quota().limit(), 50);
        assert_eq!(limiter.quota().burst(), 50);
    }

    #[test]
    fn test_builder_covers_every_knob() {
        let limiter = RateLimiter::builder()
            .per_minute(1000)
            .burst(50)
            .shards(64)
            .eviction(Eviction::capacity(100_000))
            .build();
        assert_eq!(limiter.quota().limit(), 1000);
        assert_eq!(limiter.quota().period(), Duration::from_secs(60));
        assert_eq!(limiter.quota().burst(), 50);
        assert_eq!(limiter.shards(), 64);
        assert_eq!(limiter.eviction().max_keys(), Some(100_000));
    }

    #[test]
    fn test_builder_clock_injection() {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_second(2)
            .clock(Arc::clone(&clock))
            .build();
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_deny());
        clock.advance(Duration::from_secs(1));
        assert!(limiter.check("k").is_allow());
    }

    #[test]
    fn test_unset_quota_denies() {
        let limiter = RateLimiter::builder().build();
        assert!(limiter.check("k").is_deny());
    }

    #[test]
    fn test_try_build_accepts_a_valid_configuration() {
        let limiter = RateLimiter::builder()
            .per_minute(600)
            .burst(50)
            .eviction(Eviction::idle(Duration::from_secs(60)))
            .try_build()
            .unwrap();
        assert_eq!(limiter.quota().limit(), 600);
        assert!(limiter.check("k").is_allow());
    }

    #[test]
    fn test_try_build_rejects_each_degenerate_field() {
        assert_eq!(
            RateLimiter::builder().try_build().unwrap_err(),
            RateLimiterError::ZeroQuota
        );
        assert_eq!(
            RateLimiter::builder()
                .quota(10, Duration::ZERO)
                .try_build()
                .unwrap_err(),
            RateLimiterError::ZeroPeriod
        );
        assert_eq!(
            RateLimiter::builder()
                .per_second(10)
                .burst(0)
                .try_build()
                .unwrap_err(),
            RateLimiterError::ZeroBurst
        );
    }

    #[test]
    fn test_try_build_rejects_ttl_shorter_than_recovery() {
        // A burst of 20 at 10/s takes two seconds to recover.
        let short = RateLimiter::builder()
            .per_second(10)
            .burst(20)
            .eviction(Eviction::idle(Duration::from_millis(1_999)));
        assert_eq!(
            short.try_build().unwrap_err(),
            RateLimiterError::IdleTtlTooShort
        );
        let exact = RateLimiter::builder()
            .per_second(10)
            .burst(20)
            .eviction(Eviction::idle(Duration::from_secs(2)));
        assert!(exact.try_build().is_ok());
    }

    #[test]
    fn test_build_fails_closed_on_zero_period_and_burst() {
        let zero_period = RateLimiter::builder().quota(10, Duration::ZERO).build();
        let zero_burst = RateLimiter::builder().per_second(10).burst(0).build();
        for _ in 0..100 {
            assert!(zero_period.check("k").is_deny());
            assert!(zero_burst.check("k").is_deny());
        }
    }

    #[test]
    fn test_build_raises_a_short_ttl_to_the_recovery_time() {
        // per_minute(10) with a 1s idle TTL used to reset a drained key after
        // one idle second; the effective TTL is now the full minute.
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_minute(10)
            .shards(1)
            .eviction(Eviction::idle(Duration::from_secs(1)))
            .clock(Arc::clone(&clock))
            .build();
        for _ in 0..10 {
            assert!(limiter.check("victim").is_allow());
        }
        clock.advance(Duration::from_secs(2));
        // A new key triggers the idle sweep; the drained key must survive it.
        let _ = limiter.check("other");
        assert!(limiter.check("victim").is_deny());
    }

    #[test]
    fn test_direct_builders_validate_and_fail_closed() {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_second(3)
            .clock(Arc::clone(&clock))
            .try_build_direct()
            .unwrap();
        assert!(limiter.check_n(3).is_allow());
        assert!(limiter.check().is_deny());
        assert_eq!(
            RateLimiter::builder()
                .per_second(3)
                .burst(0)
                .try_build_direct()
                .unwrap_err(),
            RateLimiterError::ZeroBurst
        );
        // The eviction policy is irrelevant to a direct limiter.
        assert!(
            RateLimiter::builder()
                .per_minute(1)
                .eviction(Eviction::idle(Duration::from_millis(1)))
                .try_build_direct()
                .is_ok()
        );
        let closed = RateLimiter::builder().build_direct();
        assert!(closed.check().is_deny());
    }

    #[test]
    fn test_extreme_shard_counts_never_panic() {
        for shards in [
            0usize,
            1,
            3,
            crate::MAX_SHARDS,
            crate::MAX_SHARDS + 1,
            usize::MAX,
        ] {
            let limiter = RateLimiter::builder().per_second(1).shards(shards).build();
            assert!(limiter.shards() >= 1 && limiter.shards() <= crate::MAX_SHARDS);
            assert!(limiter.check("k").is_allow());
            let valid = (1..=crate::MAX_SHARDS).contains(&shards);
            let checked = RateLimiter::builder()
                .per_second(1)
                .shards(shards)
                .try_build();
            assert_eq!(
                checked.err(),
                (!valid).then_some(RateLimiterError::ShardCountOutOfRange),
                "{shards}"
            );
        }
        assert_eq!(
            RateLimiter::per_second(1).with_shards(usize::MAX).shards(),
            crate::MAX_SHARDS
        );
    }

    #[test]
    fn test_builder_default_and_debug() {
        let builder = Builder::default().per_second(3);
        let rendered = format!("{builder:?}");
        assert!(rendered.contains("Builder"));
        assert!(rendered.contains("limit: 3"));
        assert_eq!(builder.build().quota().limit(), 3);
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_build_fails_closed_for_every_algorithm() {
        for algorithm in [
            Algorithm::TokenBucket,
            Algorithm::LeakyBucket,
            Algorithm::FixedWindow,
            Algorithm::SlidingWindowLog,
            Algorithm::SlidingWindowCounter,
        ] {
            let zero_period = RateLimiter::builder()
                .algorithm(algorithm)
                .quota(10, Duration::ZERO)
                .build();
            let admitted = (0..1_000)
                .filter(|_| zero_period.check("k").is_allow())
                .count();
            assert_eq!(admitted, 0, "{algorithm:?} failed open on a zero period");
        }
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_try_build_applies_algorithm_ceilings() {
        assert_eq!(
            RateLimiter::builder()
                .algorithm(Algorithm::LeakyBucket)
                .per_second(2_000_000_000)
                .try_build()
                .unwrap_err(),
            RateLimiterError::RateTooHigh
        );
        assert_eq!(
            RateLimiter::builder()
                .algorithm(Algorithm::SlidingWindowLog)
                .per_second(crate::MAX_SLIDING_LOG_LIMIT + 1)
                .try_build()
                .unwrap_err(),
            RateLimiterError::LimitTooLarge
        );
        // The sliding-window counter needs two periods to forget a window.
        assert_eq!(
            RateLimiter::builder()
                .algorithm(Algorithm::SlidingWindowCounter)
                .per_second(10)
                .eviction(Eviction::idle(Duration::from_millis(1_500)))
                .try_build()
                .unwrap_err(),
            RateLimiterError::IdleTtlTooShort
        );
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_builder_selects_algorithm() {
        let limiter = RateLimiter::builder()
            .algorithm(Algorithm::FixedWindow)
            .per_second(10)
            .build();
        assert_eq!(limiter.algorithm(), Algorithm::FixedWindow);
    }
}
