//! How much a key may do, and how fast it recovers.

use core::time::Duration;

use crate::error::RateLimiterError;

/// A rate limit: `limit` requests per `period`, per key, with a `burst` ceiling.
///
/// A quota describes the sustained rate a key is allowed and how much it may
/// spend at once. Under the default token-bucket algorithm each key starts with
/// a full allowance of `burst`, spends one unit per admitted request, and
/// accrues `limit` units back over `period` — so a key may burst up to `burst`
/// immediately and then sustain `limit` per `period` thereafter. `burst`
/// defaults to `limit` (the classic "burst equals rate" bucket); raise it with
/// [`with_burst`](Self::with_burst) to allow larger spikes, or lower it to shape
/// traffic more tightly.
///
/// The convenience constructors [`per_second`](Self::per_second) and
/// [`per_minute`](Self::per_minute) are infallible (a `limit` of `0` yields a
/// quota that admits nothing). The general [`rate`](Self::rate) constructor
/// validates its inputs and returns a [`RateLimiterError`] for values that
/// cannot describe a working limit.
///
/// The window algorithms (fixed and sliding window) admit at most `limit` per
/// `period` and ignore `burst`; it applies to the token and leaky buckets.
///
/// # Examples
///
/// ```
/// use rate_net::Quota;
/// use std::time::Duration;
///
/// let per_sec = Quota::per_second(100);
/// assert_eq!(per_sec.limit(), 100);
/// assert_eq!(per_sec.period(), Duration::from_secs(1));
/// assert_eq!(per_sec.burst(), 100); // defaults to the limit
///
/// // 1000 requests per minute, but bursts capped at 50.
/// let shaped = Quota::rate(1000, Duration::from_secs(60))?.with_burst(50);
/// assert_eq!(shaped.limit(), 1000);
/// assert_eq!(shaped.burst(), 50);
/// # Ok::<(), rate_net::RateLimiterError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    limit: u32,
    period: Duration,
    burst: u32,
}

impl Quota {
    /// A quota of `limit` requests per second, per key.
    ///
    /// Infallible: a `limit` of `0` produces a quota that admits nothing, which
    /// is well-defined. Use [`rate`](Self::rate) when you want a zero limit
    /// rejected as an error.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    /// use std::time::Duration;
    ///
    /// let quota = Quota::per_second(50);
    /// assert_eq!(quota.limit(), 50);
    /// assert_eq!(quota.period(), Duration::from_secs(1));
    /// ```
    #[must_use]
    pub const fn per_second(limit: u32) -> Self {
        Self {
            limit,
            period: Duration::from_secs(1),
            burst: limit,
        }
    }

    /// A quota of `limit` requests per minute, per key.
    ///
    /// Infallible, with the same zero-limit semantics as
    /// [`per_second`](Self::per_second).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    /// use std::time::Duration;
    ///
    /// let quota = Quota::per_minute(600);
    /// assert_eq!(quota.period(), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub const fn per_minute(limit: u32) -> Self {
        Self {
            limit,
            period: Duration::from_secs(60),
            burst: limit,
        }
    }

    /// A quota of `limit` requests per arbitrary `period`, validated.
    ///
    /// Use this when the natural window is neither a second nor a minute — for
    /// example 5 requests per 100 milliseconds, or 10 000 per hour.
    ///
    /// # Errors
    ///
    /// - [`RateLimiterError::ZeroQuota`] if `limit` is `0`.
    /// - [`RateLimiterError::ZeroPeriod`] if `period` is zero.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Quota, RateLimiterError};
    /// use std::time::Duration;
    ///
    /// let quota = Quota::rate(5, Duration::from_millis(100))?;
    /// assert_eq!(quota.limit(), 5);
    ///
    /// // A zero limit is rejected.
    /// assert_eq!(
    ///     Quota::rate(0, Duration::from_secs(1)),
    ///     Err(RateLimiterError::ZeroQuota),
    /// );
    /// # Ok::<(), RateLimiterError>(())
    /// ```
    pub const fn rate(limit: u32, period: Duration) -> Result<Self, RateLimiterError> {
        if limit == 0 {
            return Err(RateLimiterError::ZeroQuota);
        }
        if period.is_zero() {
            return Err(RateLimiterError::ZeroPeriod);
        }
        Ok(Self {
            limit,
            period,
            burst: limit,
        })
    }

    /// The number of requests admitted per [`period`](Self::period).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    ///
    /// assert_eq!(Quota::per_second(100).limit(), 100);
    /// ```
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    /// The window over which [`limit`](Self::limit) requests accrue.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    /// use std::time::Duration;
    ///
    /// assert_eq!(Quota::per_minute(60).period(), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub const fn period(&self) -> Duration {
        self.period
    }

    /// The burst ceiling: the most a key may spend at once before it must wait
    /// for the rate to refill. Defaults to [`limit`](Self::limit).
    ///
    /// Applies to the token and leaky buckets; the window algorithms admit at
    /// most `limit` per `period` regardless of `burst`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    ///
    /// assert_eq!(Quota::per_second(100).burst(), 100);
    /// assert_eq!(Quota::per_second(100).with_burst(250).burst(), 250);
    /// ```
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// Returns a copy with the burst ceiling set to `burst`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Quota;
    ///
    /// // Sustain 1000/min but never let a key spend more than 50 at once.
    /// let quota = Quota::per_minute(1000).with_burst(50);
    /// assert_eq!(quota.limit(), 1000);
    /// assert_eq!(quota.burst(), 50);
    /// ```
    #[must_use]
    pub const fn with_burst(mut self, burst: u32) -> Self {
        self.burst = burst;
        self
    }

    /// Assembles a quota from raw parts without validation, for the infallible
    /// [`Builder`](crate::Builder) path. A zero `limit` admits nothing; a zero
    /// `period` yields a degenerate limiter that never refills.
    pub(crate) const fn from_parts(limit: u32, period: Duration, burst: u32) -> Self {
        Self {
            limit,
            period,
            burst,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::Quota;
    use crate::error::RateLimiterError;
    use core::time::Duration;

    #[test]
    fn test_per_second_sets_one_second_period() {
        let quota = Quota::per_second(10);
        assert_eq!(quota.limit(), 10);
        assert_eq!(quota.period(), Duration::from_secs(1));
    }

    #[test]
    fn test_per_minute_sets_sixty_second_period() {
        assert_eq!(Quota::per_minute(10).period(), Duration::from_secs(60));
    }

    #[test]
    fn test_burst_defaults_to_limit_and_overrides() {
        assert_eq!(Quota::per_second(10).burst(), 10);
        assert_eq!(Quota::per_minute(10).burst(), 10);
        let q = Quota::rate(10, Duration::from_secs(1)).unwrap();
        assert_eq!(q.burst(), 10);
        assert_eq!(q.with_burst(25).burst(), 25);
        // Overriding burst leaves the sustained limit unchanged.
        assert_eq!(q.with_burst(25).limit(), 10);
    }

    #[test]
    fn test_rate_accepts_valid_values() {
        let quota = Quota::rate(5, Duration::from_millis(100)).unwrap();
        assert_eq!(quota.limit(), 5);
        assert_eq!(quota.period(), Duration::from_millis(100));
    }

    #[test]
    fn test_rate_rejects_zero_limit() {
        assert_eq!(
            Quota::rate(0, Duration::from_secs(1)),
            Err(RateLimiterError::ZeroQuota)
        );
    }

    #[test]
    fn test_rate_rejects_zero_period() {
        assert_eq!(
            Quota::rate(10, Duration::ZERO),
            Err(RateLimiterError::ZeroPeriod)
        );
    }

    #[test]
    fn test_per_second_zero_limit_is_allowed() {
        // Infallible constructors accept zero (admits nothing), unlike `rate`.
        assert_eq!(Quota::per_second(0).limit(), 0);
    }
}
