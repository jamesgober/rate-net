//! The Tier-2 builder.

use std::time::Duration;

use clock_lib::{Clock, SystemClock};

use crate::algorithm::Algorithm;
use crate::eviction::Eviction;
use crate::limiter::{RateLimiter, default_shard_count};
use crate::quota::Quota;

/// A fluent builder for a [`RateLimiter`] when the Tier-1 constructors are not
/// enough.
///
/// Start it with [`RateLimiter::builder`], chain the knobs you care about —
/// algorithm, quota, burst, shard count, eviction policy, clock — and call
/// [`build`](Self::build). Anything left unset keeps a sane default: the token
/// bucket, default sharding, and the bounded-memory [`Eviction`] default. The
/// quota defaults to a limit of `0` (which denies everything), so set it.
///
/// `build` is infallible: a zero limit produces a deny-everything limiter, and a
/// zero period a degenerate one, rather than an error.
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
    /// clock. Reached through [`RateLimiter::builder`].
    pub(crate) fn new() -> Self {
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
    /// token and leaky buckets; window algorithms ignore it.
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
    /// multiple of the core count.
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

    /// Builds the configured limiter.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder().quota(10, Duration::from_secs(1)).build();
    /// assert!(limiter.check("k").is_allow());
    /// ```
    #[must_use]
    pub fn build(self) -> RateLimiter<C> {
        let burst = self.burst.unwrap_or(self.limit);
        let quota = Quota::from_parts(self.limit, self.period, burst);
        let shards = self.shards.unwrap_or_else(default_shard_count);
        RateLimiter::build(self.algorithm, quota, self.clock, shards, self.eviction)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use clock_lib::ManualClock;

    use crate::algorithm::Algorithm;
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
