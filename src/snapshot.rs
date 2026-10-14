//! A read-only view of one key's allowance.

use core::time::Duration;

/// How much of its allowance a key has left, read without consuming any.
///
/// Returned by `RateLimiter::peek`, `DirectLimiter::peek`, and
/// [`Gcra::peek_at`](crate::Gcra::peek_at). It answers what rate-limit response
/// headers need — the capacity, what is left of it, and when it is whole again
/// — for allowed responses as well as denied ones, without touching the key's
/// state: peeking never counts a unit, never creates state for an unseen key,
/// and never refreshes a key's eviction recency.
///
/// Like every read of shared state, it is a momentary snapshot: under
/// concurrent checks it can be stale the instant it returns. Treat it as
/// advisory and keep [`Decision`](crate::Decision) as the authority on whether
/// a request is admitted.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "std")] {
/// use rate_net::RateLimiter;
///
/// let limiter = RateLimiter::per_second(10);
/// assert!(limiter.check_n("user:42", 3).is_allow());
///
/// let snapshot = limiter.peek("user:42");
/// assert_eq!(snapshot.limit(), 10);
/// assert_eq!(snapshot.remaining(), 7);
/// assert!(!snapshot.is_exhausted());
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Snapshot {
    limit: u32,
    remaining: u32,
    reset_after: Duration,
}

impl Snapshot {
    /// Assembles a snapshot, clamping `remaining` to `limit`.
    // Every producer of snapshots needs 64-bit atomics; on a target without
    // them the type still exists (it is plain data) but nothing builds one.
    #[cfg_attr(not(target_has_atomic = "64"), allow(dead_code))]
    pub(crate) fn new(limit: u32, remaining: u32, reset_after: Duration) -> Self {
        Self {
            limit,
            remaining: remaining.min(limit),
            reset_after,
        }
    }

    /// A key with its whole allowance: `remaining == limit`, nothing to wait
    /// for. What an unseen key reports.
    #[cfg(any(feature = "std", test))]
    pub(crate) fn full(limit: u32) -> Self {
        Self::new(limit, limit, Duration::ZERO)
    }

    /// A limiter that admits nothing: no capacity, and never any.
    #[cfg_attr(not(target_has_atomic = "64"), allow(dead_code))]
    pub(crate) fn closed() -> Self {
        Self::new(0, 0, Duration::MAX)
    }

    /// The most units the key can hold at once: the burst for the token and
    /// leaky buckets, the limit for the window algorithms. `0` for a limiter
    /// that admits nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::{Quota, RateLimiter};
    ///
    /// let limiter = RateLimiter::with_quota(Quota::per_second(10).with_burst(25));
    /// assert_eq!(limiter.peek("k").limit(), 25);
    /// # }
    /// ```
    #[must_use]
    pub const fn limit(&self) -> u32 {
        self.limit
    }

    /// The units that would be admitted right now, never more than
    /// [`limit`](Self::limit).
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(5);
    /// let _ = limiter.check_n("k", 2);
    /// assert_eq!(limiter.peek("k").remaining(), 3);
    /// # }
    /// ```
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        self.remaining
    }

    /// The wait until the key has its whole allowance back
    /// (`remaining == limit`). Zero for a full key; [`Duration::MAX`] for a
    /// limiter that admits nothing.
    ///
    /// For the window algorithms this is when the current window's (or the
    /// trailing window's) admissions have all aged out; for the buckets, when
    /// the burst has fully refilled.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::per_second(5);
    /// assert_eq!(limiter.peek("unseen").reset_after(), Duration::ZERO);
    /// let _ = limiter.check("k");
    /// assert!(limiter.peek("k").reset_after() > Duration::ZERO);
    /// # }
    /// ```
    #[must_use]
    pub const fn reset_after(&self) -> Duration {
        self.reset_after
    }

    /// Whether the key has nothing left right now (`remaining == 0`).
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// assert!(!limiter.peek("k").is_exhausted());
    /// let _ = limiter.check("k");
    /// assert!(limiter.peek("k").is_exhausted());
    /// # }
    /// ```
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }

    /// [`reset_after`](Self::reset_after) in whole seconds, rounded up — the
    /// `RateLimit-Reset` value.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_minute(60);
    /// let _ = limiter.check("k"); // one token back in 1 s
    /// assert_eq!(limiter.peek("k").reset_after_secs(), 1);
    /// # }
    /// ```
    #[must_use]
    pub const fn reset_after_secs(&self) -> u64 {
        crate::headers::ceil_secs(self.reset_after)
    }

    /// The `RateLimit-Reset` value: the rounded-up reset time, or `0` for a
    /// limiter that admits nothing (see [`headers`](Self::headers)).
    const fn reset_header_secs(&self) -> u64 {
        if self.limit == 0 {
            0
        } else {
            self.reset_after_secs()
        }
    }

    /// The three `RateLimit-*` response headers as `(name, value)` pairs, ready
    /// for any HTTP framework: `RateLimit-Limit` ([`limit`](Self::limit)),
    /// `RateLimit-Remaining` ([`remaining`](Self::remaining)), and
    /// `RateLimit-Reset` ([`reset_after_secs`](Self::reset_after_secs)).
    ///
    /// A limiter that admits nothing (`limit() == 0`) has no reset to report:
    /// its `reset_after` is [`Duration::MAX`], which as a header would be a
    /// 20-digit number that 32-bit parsers reject. It reports
    /// `RateLimit-Reset: 0` instead, beside `RateLimit-Limit: 0`, which is what
    /// tells a client nothing will be admitted.
    ///
    /// See [`headers`](crate::headers) for the header names and how they pair
    /// with `Retry-After`.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// let _ = limiter.check_n("k", 3);
    /// assert_eq!(
    ///     limiter.peek("k").headers(),
    ///     [
    ///         ("RateLimit-Limit", 10),
    ///         ("RateLimit-Remaining", 7),
    ///         ("RateLimit-Reset", 1),
    ///     ],
    /// );
    /// # }
    /// ```
    #[must_use]
    pub const fn headers(&self) -> [(&'static str, u64); 3] {
        self.legacy_headers()
    }

    /// The value of the structured `RateLimit` field for the policy named
    /// `policy`: `"policy";r=<remaining>;t=<reset>`, where `t` is
    /// [`reset_after_secs`](Self::reset_after_secs) (or `0` for a limiter that
    /// admits nothing, as in [`headers`](Self::headers)).
    ///
    /// This is the current IETF draft's single structured field (send it under
    /// [`headers::RATELIMIT`](crate::headers::RATELIMIT)), beside a
    /// `RateLimit-Policy` field from
    /// [`Quota::ratelimit_policy`](crate::Quota::ratelimit_policy) with the same
    /// name. The value formats with `Display` and never allocates.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "std")] {
    /// use rate_net::{headers, RateLimiter};
    ///
    /// let limiter = RateLimiter::per_minute(60);
    /// let _ = limiter.check_n("k", 2);
    /// let snapshot = limiter.peek("k");
    /// assert_eq!(headers::RATELIMIT, "RateLimit");
    /// assert_eq!(snapshot.ratelimit("default").to_string(), r#""default";r=58;t=2"#);
    /// # }
    /// ```
    #[must_use]
    pub const fn ratelimit<'a>(&self, policy: &'a str) -> crate::headers::RateLimitValue<'a> {
        crate::headers::RateLimitValue::new(policy, self.remaining as u64, self.reset_header_secs())
    }

    const fn legacy_headers(&self) -> [(&'static str, u64); 3] {
        [
            (crate::headers::RATELIMIT_LIMIT, self.limit as u64),
            (crate::headers::RATELIMIT_REMAINING, self.remaining as u64),
            (crate::headers::RATELIMIT_RESET, self.reset_header_secs()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::Snapshot;
    use core::time::Duration;

    #[test]
    fn test_remaining_is_clamped_to_limit() {
        let snapshot = Snapshot::new(5, 9, Duration::ZERO);
        assert_eq!(snapshot.remaining(), 5);
        assert_eq!(snapshot.limit(), 5);
    }

    #[test]
    fn test_headers_report_limit_remaining_and_rounded_reset() {
        let snapshot = Snapshot::new(10, 4, Duration::from_millis(2_500));
        assert_eq!(snapshot.reset_after_secs(), 3);
        assert_eq!(
            snapshot.headers(),
            [
                ("RateLimit-Limit", 10),
                ("RateLimit-Remaining", 4),
                ("RateLimit-Reset", 3),
            ]
        );
        assert_eq!(Snapshot::full(5).reset_after_secs(), 0);
    }

    #[test]
    fn test_closed_snapshot_headers_report_no_reset() {
        let closed = Snapshot::closed();
        assert_eq!(closed.reset_after_secs(), u64::MAX);
        assert_eq!(
            closed.headers(),
            [
                ("RateLimit-Limit", 0),
                ("RateLimit-Remaining", 0),
                ("RateLimit-Reset", 0),
            ]
        );
    }

    #[test]
    fn test_full_and_closed() {
        let full = Snapshot::full(8);
        assert_eq!(
            (full.limit(), full.remaining(), full.reset_after()),
            (8, 8, Duration::ZERO)
        );
        assert!(!full.is_exhausted());
        let closed = Snapshot::closed();
        assert_eq!(closed.limit(), 0);
        assert!(closed.is_exhausted());
        assert_eq!(closed.reset_after(), Duration::MAX);
    }
}
