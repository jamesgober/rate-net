//! The rate limiter and the trait every algorithm shares.

use std::fmt;

use better_bucket::Bucket;
use clock_lib::{Clock, SystemClock};
use dashmap::DashMap;

use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::key::Key;
use crate::quota::Quota;

/// The shared rate-limiting surface, independent of the algorithm behind it.
///
/// Every limiter — whatever algorithm it uses — answers the same question:
/// *is this key allowed right now?* `Limiter` is that contract, so generic code
/// can hold any limiter and call [`check`](Self::check) without naming the
/// concrete type or its clock. [`RateLimiter`] is the implementation this crate
/// ships.
///
/// Implementors only need to provide [`check_n`](Self::check_n);
/// [`check`](Self::check) defaults to one unit.
///
/// # Examples
///
/// ```
/// use rate_net::{Limiter, RateLimiter, Decision};
///
/// // Generic over any limiter implementation.
/// fn admit_one<L: Limiter>(limiter: &L, key: &str) -> bool {
///     limiter.check(key).is_allow()
/// }
///
/// let limiter = RateLimiter::per_second(2);
/// assert!(admit_one(&limiter, "user:1"));
/// ```
pub trait Limiter {
    /// Checks `n` units against `key`, returning the [`Decision`].
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision;

    /// Checks a single unit against `key`. Equivalent to `check_n(key, 1)`.
    fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_n(key, 1)
    }
}

/// A keyed rate limiter.
///
/// Tracks an independent allowance for every key it sees and answers
/// [`check`](Self::check) in the time it takes to hash the key and run its
/// bucket. State for unrelated keys lives in different shards of a concurrent
/// map, so they never contend; the limiter is `Send + Sync` and is meant to be
/// shared (behind an [`Arc`](std::sync::Arc), or as a `static`) across all the
/// threads serving requests.
///
/// The default algorithm is the token bucket, whose accounting is delegated to
/// [`better-bucket`](https://crates.io/crates/better-bucket): each key bursts up
/// to its [`Quota`] immediately, then sustains the quota rate as the allowance
/// refills. Time comes from an injectable [`Clock`] — [`SystemClock`] in
/// production, or a `ManualClock` in tests via [`with_clock`](Self::with_clock).
///
/// # Examples
///
/// ```
/// use rate_net::{RateLimiter, Decision};
///
/// // 100 requests per second, per key.
/// let limiter = RateLimiter::per_second(100);
///
/// match limiter.check("user:42") {
///     Decision::Allow => { /* serve the request */ }
///     Decision::Deny { retry_after } => {
///         // 429, Retry-After: retry_after
///         let _ = retry_after;
///     }
///     _ => {}
/// }
/// ```
pub struct RateLimiter<C: Clock + Clone = SystemClock> {
    quota: Quota,
    clock: C,
    keys: DashMap<Key, Bucket<C>>,
}

impl RateLimiter<SystemClock> {
    /// Creates a limiter allowing `limit` requests per second, per key, driven
    /// by the OS monotonic clock.
    ///
    /// The headline Tier-1 constructor. A `limit` of `0` yields a limiter that
    /// denies every request.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// assert_eq!(limiter.quota().limit(), 10);
    /// ```
    #[must_use]
    pub fn per_second(limit: u32) -> Self {
        Self::with_quota(Quota::per_second(limit))
    }

    /// Creates a limiter allowing `limit` requests per minute, per key.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::per_minute(600);
    /// assert_eq!(limiter.quota().period(), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn per_minute(limit: u32) -> Self {
        Self::with_quota(Quota::per_minute(limit))
    }

    /// Creates a limiter from an explicit [`Quota`], driven by the OS monotonic
    /// clock.
    ///
    /// Use this with [`Quota::rate`] when the window is neither a second nor a
    /// minute.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Quota};
    /// use std::time::Duration;
    ///
    /// let quota = Quota::rate(5, Duration::from_millis(100))?;
    /// let limiter = RateLimiter::with_quota(quota);
    /// assert_eq!(limiter.quota().limit(), 5);
    /// # Ok::<(), rate_net::RateLimiterError>(())
    /// ```
    #[must_use]
    pub fn with_quota(quota: Quota) -> Self {
        Self {
            quota,
            clock: SystemClock::new(),
            keys: DashMap::new(),
        }
    }
}

impl<C: Clock + Clone> RateLimiter<C> {
    /// Replaces the limiter's time source, discarding any per-key state.
    ///
    /// This is the clock-injection seam, intended for use immediately after
    /// construction. Injecting a `ManualClock` makes refill behaviour
    /// deterministic so window and rollover tests run with no `sleep`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Decision};
    /// use clock_lib::ManualClock;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let clock = Arc::new(ManualClock::new());
    /// let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));
    ///
    /// // Drain the key's allowance.
    /// for _ in 0..5 {
    ///     assert!(limiter.check("k").is_allow());
    /// }
    /// assert!(limiter.check("k").is_deny());
    ///
    /// // Advance one second — no real sleep — and the allowance is back.
    /// clock.advance(Duration::from_secs(1));
    /// assert!(limiter.check("k").is_allow());
    /// ```
    #[must_use]
    pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> RateLimiter<C2> {
        RateLimiter {
            quota: self.quota,
            clock,
            keys: DashMap::new(),
        }
    }

    /// Checks a single unit against `key`.
    ///
    /// Returns [`Decision::Allow`] if the key is within its limit (the unit is
    /// counted), or [`Decision::Deny`] with the wait until it would be admitted.
    /// The key can be anything that converts into a [`Key`] — a string, an IP
    /// address, a user id.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Decision};
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// assert_eq!(limiter.check("user:42"), Decision::Allow);
    /// assert!(limiter.check("user:42").is_deny()); // limit reached
    /// ```
    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_inner(key.into(), 1)
    }

    /// Checks `n` units against `key` in one operation.
    ///
    /// Useful when a single request costs more than one unit (a batch, a
    /// weighted endpoint). Either all `n` units are admitted or none are.
    /// Requesting `0` always succeeds; requesting more than the quota can never
    /// succeed, and the denial's `retry_after` is [`Duration::MAX`].
    ///
    /// [`Duration::MAX`]: std::time::Duration::MAX
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Decision};
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// assert_eq!(limiter.check_n("tenant:acme", 4), Decision::Allow);
    /// assert_eq!(limiter.check_n("tenant:acme", 6), Decision::Allow);
    /// assert!(limiter.check_n("tenant:acme", 1).is_deny()); // 10 spent
    /// ```
    pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_inner(key.into(), n)
    }

    /// The quota every key is limited to.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// assert_eq!(RateLimiter::per_second(50).quota().limit(), 50);
    /// ```
    #[must_use]
    pub fn quota(&self) -> Quota {
        self.quota
    }

    /// The algorithm this limiter applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Algorithm};
    ///
    /// assert_eq!(RateLimiter::per_second(1).algorithm(), Algorithm::TokenBucket);
    /// ```
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        Algorithm::TokenBucket
    }

    /// The number of keys with live state right now.
    ///
    /// A momentary snapshot, advisory under concurrent access. Until eviction
    /// lands it only grows; it is exposed mainly for tests and diagnostics.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// assert_eq!(limiter.tracked_keys(), 0);
    /// let _ = limiter.check("a");
    /// assert_eq!(limiter.tracked_keys(), 1);
    /// ```
    #[must_use]
    pub fn tracked_keys(&self) -> usize {
        self.keys.len()
    }

    /// The shared check path: locate (or create) the key's bucket and acquire.
    fn check_inner(&self, key: Key, n: u32) -> Decision {
        let outcome = self
            .keys
            .entry(key)
            .or_insert_with(|| self.new_bucket())
            .acquire(n);
        outcome.into()
    }

    /// Builds a fresh per-key bucket for the configured quota, anchored at the
    /// injected clock's current reading.
    fn new_bucket(&self) -> Bucket<C> {
        Bucket::per_duration(self.quota.limit(), self.quota.period()).with_clock(self.clock.clone())
    }
}

impl<C: Clock + Clone> Limiter for RateLimiter<C> {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_inner(key.into(), n)
    }
}

impl<C: Clock + Clone> fmt::Debug for RateLimiter<C> {
    /// Formats the limiter without exposing any key. Keys can be caller
    /// identities or other sensitive values, so only the configuration and the
    /// live key count are shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimiter")
            .field("algorithm", &self.algorithm())
            .field("quota", &self.quota)
            .field("tracked_keys", &self.keys.len())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use clock_lib::ManualClock;

    use super::{Limiter, RateLimiter};
    use crate::algorithm::Algorithm;
    use crate::decision::Decision;
    use crate::quota::Quota;

    fn manual() -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));
        (clock, limiter)
    }

    #[test]
    fn test_fresh_key_is_admitted() {
        let limiter = RateLimiter::per_second(1);
        assert_eq!(limiter.check("user:1"), Decision::Allow);
    }

    #[test]
    fn test_quota_is_exhausted_then_refills_on_advance() {
        let (clock, limiter) = manual();

        for _ in 0..5 {
            assert_eq!(limiter.check("k"), Decision::Allow);
        }
        // Sixth request in the same instant is denied.
        let decision = limiter.check("k");
        assert!(decision.is_deny());
        assert!(decision.retry_after().is_some());

        // A full second restores the whole allowance.
        clock.advance(Duration::from_secs(1));
        assert_eq!(limiter.check("k"), Decision::Allow);
    }

    #[test]
    fn test_keys_are_independent() {
        let (_clock, limiter) = manual();

        // Drain one key entirely.
        for _ in 0..5 {
            assert!(limiter.check("a").is_allow());
        }
        assert!(limiter.check("a").is_deny());

        // A different key is untouched.
        assert!(limiter.check("b").is_allow());
    }

    #[test]
    fn test_check_n_takes_multiple_units_atomically() {
        let (_clock, limiter) = manual();
        assert_eq!(limiter.check_n("batch", 3), Decision::Allow);
        assert_eq!(limiter.check_n("batch", 2), Decision::Allow);
        assert!(limiter.check_n("batch", 1).is_deny());
    }

    #[test]
    fn test_check_n_zero_always_admits() {
        let (_clock, limiter) = manual();
        // Drain the key first.
        for _ in 0..5 {
            assert!(limiter.check("k").is_allow());
        }
        // A zero-unit check costs nothing and is always admitted.
        assert_eq!(limiter.check_n("k", 0), Decision::Allow);
    }

    #[test]
    fn test_request_larger_than_quota_can_never_succeed() {
        let (clock, limiter) = manual();
        let decision = limiter.check_n("k", 6); // quota is 5
        assert_eq!(
            decision,
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
        // Even after time passes, an over-capacity request still cannot succeed.
        clock.advance(Duration::from_secs(10));
        assert_eq!(limiter.check_n("k", 6).retry_after(), Some(Duration::MAX));
    }

    #[test]
    fn test_zero_limit_denies_everything() {
        let limiter = RateLimiter::with_quota(Quota::per_second(0));
        assert!(limiter.check("k").is_deny());
    }

    #[test]
    fn test_partial_refill_admits_proportionally() {
        // 10 per second → one token every 100ms.
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(10).with_clock(Arc::clone(&clock));
        for _ in 0..10 {
            assert!(limiter.check("k").is_allow());
        }
        assert!(limiter.check("k").is_deny());

        // 300ms restores ~3 tokens.
        clock.advance(Duration::from_millis(300));
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_deny());
    }

    #[test]
    fn test_tracked_keys_counts_distinct_keys() {
        let (_clock, limiter) = manual();
        assert_eq!(limiter.tracked_keys(), 0);
        let _ = limiter.check("a");
        let _ = limiter.check("b");
        let _ = limiter.check("a");
        assert_eq!(limiter.tracked_keys(), 2);
    }

    #[test]
    fn test_introspection_reports_token_bucket() {
        let limiter = RateLimiter::per_second(1);
        assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
    }

    #[test]
    fn test_limiter_trait_object_safe_via_generic() {
        fn count_admitted<L: Limiter>(limiter: &L, key: &str, attempts: u32) -> u32 {
            (0..attempts)
                .filter(|_| limiter.check(key).is_allow())
                .count() as u32
        }
        let limiter = RateLimiter::per_second(3);
        assert_eq!(count_admitted(&limiter, "k", 10), 3);
    }

    #[test]
    fn test_debug_does_not_leak_keys() {
        let (_clock, limiter) = manual();
        let _ = limiter.check("secret-token-do-not-print");
        let rendered = format!("{limiter:?}");
        assert!(!rendered.contains("secret-token"));
        assert!(rendered.contains("RateLimiter"));
        assert!(rendered.contains("tracked_keys"));
    }
}
