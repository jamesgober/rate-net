//! A read-only view of one key's allowance.

use core::time::Duration;

/// How much of its allowance a key has left, read without consuming any.
///
/// Returned by `RateLimiter::peek`. It answers what rate-limit response
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
    pub(crate) fn new(limit: u32, remaining: u32, reset_after: Duration) -> Self {
        Self {
            limit,
            remaining: remaining.min(limit),
            reset_after,
        }
    }

    /// A key with its whole allowance: `remaining == limit`, nothing to wait
    /// for. What an unseen key reports.
    pub(crate) fn full(limit: u32) -> Self {
        Self::new(limit, limit, Duration::ZERO)
    }

    /// A limiter that admits nothing: no capacity, and never any.
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
