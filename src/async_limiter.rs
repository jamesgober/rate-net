//! Optional async-friendly wrapper.
//!
//! The core is sync and runtime-free: [`check`](crate::RateLimiter::check) never
//! sleeps or waits on a timer, so most async callers just call it and shed load
//! on a denial. This layer adds the one thing that genuinely needs a runtime —
//! *awaiting* until a key is allowed — behind the `async` feature, so the core
//! never depends on a runtime.
//!
//! [`AsyncLimiter::try_until_ready`] retries on each denial, sleeping for the
//! reported `retry_after` via [`tokio::time::sleep`], until the request is
//! admitted — or returns [`InsufficientCapacity`] when no amount of waiting
//! could admit it. It needs a clock that actually advances (the default
//! [`SystemClock`](clock_lib::SystemClock)); under a frozen `ManualClock` the
//! allowance never refills, so it would wait forever.

use core::fmt;
use core::time::Duration;

use ahash::RandomState;
use clock_lib::{Clock, SystemClock};
use tokio::time::Instant;

use crate::decision::Decision;
use crate::error::InsufficientCapacity;
use crate::key::Key;
use crate::limiter::RateLimiter;

/// An async-friendly wrapper around a [`RateLimiter`].
///
/// Holds a limiter and adds [`try_until_ready`](Self::try_until_ready) /
/// [`try_until_ready_n`](Self::try_until_ready_n), which await until the key is
/// admitted rather than returning a denial. The synchronous
/// [`check`](Self::check) / [`check_n`](Self::check_n) pass straight through.
///
/// # Cancellation
///
/// Every wait is cancel-safe: the future only sleeps between checks, and a unit
/// is counted only by the check that admits it, so dropping the future (a
/// `select!` branch losing, a timeout firing) consumes nothing.
///
/// # Deadlines and jitter
///
/// [`until_ready_within`](Self::until_ready_within) bounds the wait: it sleeps
/// only while the next possible admission falls inside the budget, and
/// otherwise returns the denial at once, so a caller never sleeps for nothing.
/// [`with_jitter`](Self::with_jitter) adds a random delay to every sleep, so
/// many tasks denied at the same instant do not all retry at the same instant
/// (a thundering herd that would mostly be denied again).
///
/// # Panics
///
/// The awaiting methods sleep with [`tokio::time::sleep`], so — like it — they
/// panic if polled outside a Tokio runtime with the time driver enabled. The
/// synchronous [`check`](Self::check) / [`check_n`](Self::check_n) work
/// anywhere.
///
/// # Examples
///
/// ```
/// use rate_net::{AsyncLimiter, RateLimiter};
///
/// # async fn demo() -> Result<(), rate_net::InsufficientCapacity> {
/// let limiter = AsyncLimiter::new(RateLimiter::per_second(100));
///
/// // Non-blocking: returns immediately, allow or deny.
/// let _ = limiter.check("user:42");
///
/// // Awaiting: returns once the key is within its limit, or an error if the
/// // request could never fit.
/// limiter.try_until_ready("user:42").await?;
/// # Ok(())
/// # }
/// ```
pub struct AsyncLimiter<C: Clock + Clone = SystemClock> {
    inner: RateLimiter<C>,
    jitter: Duration,
}

impl<C: Clock + Clone> AsyncLimiter<C> {
    /// Wraps a limiter for async use.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(10));
    /// assert_eq!(limiter.inner().quota().limit(), 10);
    /// ```
    #[must_use]
    pub fn new(inner: RateLimiter<C>) -> Self {
        Self {
            inner,
            jitter: Duration::ZERO,
        }
    }

    /// Adds a random delay of up to `max` to every sleep between attempts.
    ///
    /// When many tasks are denied together (a burst against one key, a shared
    /// downstream quota), each would otherwise wake exactly at its
    /// `retry_after` and contend for the same refill. Uniform jitter in
    /// `[0, max]` spreads the retries out. It never makes a wait shorter, and a
    /// bounded wait ([`until_ready_within`](Self::until_ready_within)) still
    /// never sleeps past its deadline. The default is no jitter.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    /// use std::time::Duration;
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(10))
    ///     .with_jitter(Duration::from_millis(20));
    /// assert_eq!(limiter.jitter(), Duration::from_millis(20));
    /// ```
    #[must_use]
    pub const fn with_jitter(mut self, max: Duration) -> Self {
        self.jitter = max;
        self
    }

    /// The most jitter added to each sleep (zero unless set with
    /// [`with_jitter`](Self::with_jitter)).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    /// use std::time::Duration;
    ///
    /// assert_eq!(AsyncLimiter::new(RateLimiter::per_second(1)).jitter(), Duration::ZERO);
    /// ```
    #[must_use]
    pub const fn jitter(&self) -> Duration {
        self.jitter
    }

    /// Borrows the wrapped limiter — for `peek`, `remove`, and the rest of the
    /// synchronous surface.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(10));
    /// assert_eq!(limiter.inner().tracked_keys(), 0);
    /// ```
    #[must_use]
    pub fn inner(&self) -> &RateLimiter<C> {
        &self.inner
    }

    /// Unwraps back into the limiter.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(10)).into_inner();
    /// assert_eq!(limiter.quota().limit(), 10);
    /// ```
    #[must_use]
    pub fn into_inner(self) -> RateLimiter<C> {
        self.inner
    }

    /// Checks a single unit against `key` without awaiting — a straight
    /// pass-through to [`RateLimiter::check`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(1));
    /// assert!(limiter.check("k").is_allow());
    /// assert!(limiter.check("k").is_deny());
    /// ```
    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.inner.check(key)
    }

    /// Checks `n` units against `key` without awaiting — a straight pass-through
    /// to [`RateLimiter::check_n`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(5));
    /// assert!(limiter.check_n("k", 5).is_allow());
    /// assert!(limiter.check_n("k", 1).is_deny());
    /// ```
    pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.inner.check_n(key, n)
    }

    /// Awaits until a single unit is admitted for `key`, or reports that it
    /// never can be.
    ///
    /// Equivalent to `try_until_ready_n(key, 1)`.
    ///
    /// # Errors
    ///
    /// [`InsufficientCapacity`] if the limiter can never admit even one unit
    /// (a zero limit or zero burst).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// # async fn demo() {
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(100));
    /// assert!(limiter.try_until_ready("user:42").await.is_ok());
    ///
    /// // A limiter that admits nothing reports it instead of returning as if
    /// // the caller had been admitted.
    /// let closed = AsyncLimiter::new(RateLimiter::per_second(0));
    /// assert!(closed.try_until_ready("user:42").await.is_err());
    /// # }
    /// ```
    pub async fn try_until_ready(&self, key: impl Into<Key>) -> Result<(), InsufficientCapacity> {
        self.try_until_ready_n(key, 1).await
    }

    /// Awaits until `n` units are admitted for `key`, sleeping for the reported
    /// `retry_after` after each denial, or reports that they never can be.
    ///
    /// Returns `Ok(())` only once the units have actually been counted against
    /// the key. Cancel-safe: dropping the future consumes nothing.
    ///
    /// # Errors
    ///
    /// [`InsufficientCapacity`] if `n` exceeds what the limit can ever grant at
    /// once (more than the burst for the buckets, more than the limit for the
    /// window algorithms), or the limiter admits nothing at all. Waiting would
    /// never help, so it gives up immediately.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    ///
    /// # async fn demo() {
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(5));
    /// assert!(limiter.try_until_ready_n("batch", 5).await.is_ok());
    ///
    /// // Six units can never fit a five-unit burst.
    /// let err = limiter.try_until_ready_n("batch", 6).await.unwrap_err();
    /// assert_eq!(err.requested(), 6);
    /// # }
    /// ```
    pub async fn try_until_ready_n(
        &self,
        key: impl Into<Key>,
        n: u32,
    ) -> Result<(), InsufficientCapacity> {
        let key = key.into();
        let decision = wait(
            || self.inner.check_ref(key.as_key_ref(), n),
            None,
            self.jitter,
        )
        .await;
        if decision.is_allow() {
            Ok(())
        } else {
            Err(InsufficientCapacity::new(n))
        }
    }

    /// Awaits until a single unit is admitted for `key`, but never past
    /// `max_wait`. Equivalent to `until_ready_n_within(key, 1, max_wait)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    /// use std::time::Duration;
    ///
    /// # async fn demo() {
    /// let limiter = AsyncLimiter::new(RateLimiter::per_minute(1));
    /// assert!(limiter.until_ready_within("k", Duration::ZERO).await.is_allow());
    ///
    /// // The next unit is a minute away: a 50ms budget is denied at once,
    /// // without sleeping, with the real wait in `retry_after`.
    /// let denied = limiter.until_ready_within("k", Duration::from_millis(50)).await;
    /// assert!(denied.retry_after().unwrap() > Duration::from_secs(50));
    /// # }
    /// ```
    pub async fn until_ready_within(&self, key: impl Into<Key>, max_wait: Duration) -> Decision {
        self.until_ready_n_within(key, 1, max_wait).await
    }

    /// Awaits until `n` units are admitted for `key`, but never past
    /// `max_wait`: the bounded form of
    /// [`try_until_ready_n`](Self::try_until_ready_n), for a caller with a
    /// latency budget (a request deadline, a job's time slot).
    ///
    /// It sleeps only while the next admission the limiter reports falls
    /// within the remaining budget. Returns [`Decision::Allow`] once the units
    /// are counted; otherwise the last [`Decision::Deny`] — returned as soon as
    /// the wait is known not to fit, not after sleeping out the budget — whose
    /// `retry_after` is the wait the caller would still need
    /// ([`Duration::MAX`] if the request can never fit). Cancel-safe.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{AsyncLimiter, RateLimiter};
    /// use std::time::Duration;
    ///
    /// # async fn demo() {
    /// let limiter = AsyncLimiter::new(RateLimiter::per_second(100));
    /// assert!(limiter.check_n("k", 100).is_allow());
    /// // One unit refills in 10ms, well within the budget.
    /// let admitted = limiter
    ///     .until_ready_n_within("k", 1, Duration::from_secs(1))
    ///     .await;
    /// assert!(admitted.is_allow());
    ///
    /// // More than the burst can never fit, budget or not.
    /// let never = limiter
    ///     .until_ready_n_within("k", 101, Duration::from_secs(60))
    ///     .await;
    /// assert!(never.is_impossible());
    /// # }
    /// ```
    pub async fn until_ready_n_within(
        &self,
        key: impl Into<Key>,
        n: u32,
        max_wait: Duration,
    ) -> Decision {
        let key = key.into();
        wait(
            || self.inner.check_ref(key.as_key_ref(), n),
            Some(max_wait),
            self.jitter,
        )
        .await
    }

    /// Awaits until a single unit is admitted for `key`.
    ///
    /// Equivalent to `until_ready_n(key, 1)`.
    #[deprecated(
        since = "1.1.0",
        note = "returns as if admitted when the request can never fit; use `try_until_ready`, which reports that as an error"
    )]
    pub async fn until_ready(&self, key: impl Into<Key>) {
        let _outcome = self.try_until_ready_n(key, 1).await;
    }

    /// Awaits until `n` units are admitted for `key`, sleeping for the reported
    /// `retry_after` after each denial.
    ///
    /// Returns immediately — *without* admitting anything — if `n` exceeds
    /// what the limit can ever grant: the caller cannot tell that apart from a
    /// real admission. Use [`try_until_ready_n`](Self::try_until_ready_n),
    /// which returns [`InsufficientCapacity`] in that case.
    #[deprecated(
        since = "1.1.0",
        note = "returns as if admitted when the request can never fit; use `try_until_ready_n`, which reports that as an error"
    )]
    pub async fn until_ready_n(&self, key: impl Into<Key>, n: u32) {
        let _outcome = self.try_until_ready_n(key, n).await;
    }
}

impl<C: Clock + Clone> From<RateLimiter<C>> for AsyncLimiter<C> {
    fn from(inner: RateLimiter<C>) -> Self {
        Self::new(inner)
    }
}

impl<C: Clock + Clone> fmt::Debug for AsyncLimiter<C> {
    /// Formats the wrapped limiter's configuration; never any key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncLimiter")
            .field("inner", &self.inner)
            .field("jitter", &self.jitter)
            .finish()
    }
}

/// The shared wait loop: check, and on a denial sleep for the reported
/// `retry_after` (plus up to `jitter`) and check again, until admitted, until
/// the request is reported impossible, or — with a `max_wait` — until the next
/// admission would fall past the deadline. Returns the last decision.
pub(crate) async fn wait(
    mut check: impl FnMut() -> Decision,
    max_wait: Option<Duration>,
    jitter: Duration,
) -> Decision {
    // An unrepresentably distant deadline is no deadline.
    let deadline = max_wait.and_then(|budget| Instant::now().checked_add(budget));
    let bounded = max_wait.is_some();
    loop {
        let decision = check();
        let Decision::Deny { retry_after } = decision else {
            return decision;
        };
        if retry_after == Duration::MAX {
            return decision;
        }
        let mut sleep_for = retry_after.saturating_add(random_jitter(jitter));
        if bounded {
            let left = deadline.map_or(Duration::MAX, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            if retry_after > left {
                return decision;
            }
            sleep_for = sleep_for.min(left);
        }
        tokio::time::sleep(sleep_for).await;
    }
}

/// A uniformly random delay in `[0, max]`. Seeded from a fresh randomly keyed
/// hasher per call; not cryptographic, and needs not be.
fn random_jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let span = u64::try_from(max.as_nanos())
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let draw = RandomState::new().hash_one(0u8);
    // `span == 0` only if `max` saturated to `u64::MAX` nanoseconds and wrapped
    // to zero above, which `saturating_add` rules out.
    Duration::from_nanos(draw.checked_rem(span).unwrap_or(draw))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn test_random_jitter_stays_within_its_bound() {
        assert_eq!(super::random_jitter(Duration::ZERO), Duration::ZERO);
        let max = Duration::from_micros(50);
        let draws: Vec<Duration> = (0..1_000).map(|_| super::random_jitter(max)).collect();
        assert!(draws.iter().all(|d| *d <= max));
        // Not constant: a thousand draws from 50,001 values are not all equal.
        assert!(draws.iter().any(|d| *d != draws[0]));
        assert!(super::random_jitter(Duration::MAX) <= Duration::MAX);
    }

    #[tokio::test]
    async fn test_until_ready_within_denies_at_once_when_the_wait_does_not_fit() {
        let limiter = AsyncLimiter::new(RateLimiter::per_minute(1));
        assert!(limiter.check("k").is_allow());
        let start = Instant::now();
        let decision = limiter
            .until_ready_within("k", Duration::from_millis(200))
            .await;
        assert!(start.elapsed() < Duration::from_millis(150), "it slept");
        assert!(decision.retry_after().unwrap() > Duration::from_secs(50));
        // Nothing was counted by the attempt.
        assert_eq!(limiter.inner().peek("k").remaining(), 0);
    }

    #[tokio::test]
    async fn test_until_ready_within_admits_inside_the_budget_even_with_jitter() {
        let limiter =
            AsyncLimiter::new(RateLimiter::per_second(200)).with_jitter(Duration::from_millis(2));
        assert!(limiter.check_n("k", 200).is_allow());
        let decision = limiter
            .until_ready_n_within("k", 1, Duration::from_secs(2))
            .await;
        assert!(decision.is_allow());
        assert!(
            limiter
                .until_ready_n_within("k", 201, Duration::from_secs(2))
                .await
                .is_impossible()
        );
        assert!(
            limiter
                .until_ready_within("k", Duration::MAX)
                .await
                .is_allow()
        );
        assert!(format!("{limiter:?}").contains("jitter"));
    }

    #[tokio::test]
    async fn test_direct_limiter_bounded_waits_and_jitter() {
        let limiter = crate::DirectLimiter::per_second(100).with_jitter(Duration::from_millis(1));
        assert_eq!(limiter.jitter(), Duration::from_millis(1));
        assert!(limiter.check_n(100).is_allow());
        assert!(
            limiter
                .until_ready_within(Duration::from_secs(1))
                .await
                .is_allow()
        );
        assert!(limiter.check_n(100).is_deny());
        assert!(
            limiter
                .until_ready_n_within(101, Duration::from_secs(1))
                .await
                .is_impossible()
        );
        let slow = crate::DirectLimiter::per_minute(1).with_jitter(Duration::from_millis(1));
        assert!(slow.check().is_allow());
        assert!(
            slow.until_ready_within(Duration::from_millis(5))
                .await
                .is_deny()
        );
        // The jitter survives a rebuild.
        let rebuilt = slow.with_algorithm(crate::Algorithm::TokenBucket);
        assert_eq!(rebuilt.jitter(), Duration::from_millis(1));
        let reclocked = rebuilt.with_clock(clock_lib::SystemClock::new());
        assert_eq!(reclocked.jitter(), Duration::from_millis(1));
    }

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::AsyncLimiter;
    use crate::error::InsufficientCapacity;
    use crate::limiter::RateLimiter;

    #[test]
    fn test_check_passthrough_is_sync() {
        let limiter = AsyncLimiter::new(RateLimiter::per_second(1));
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_deny());
        assert!(limiter.check_n("k", 0).is_allow());
    }

    #[tokio::test]
    async fn test_try_until_ready_admits_after_waiting() {
        // A fast rate keeps the wait to a few milliseconds of real time.
        let limiter = AsyncLimiter::new(RateLimiter::per_second(200));
        // Drain the key, so the next admit must wait for a refill.
        for _ in 0..200 {
            assert!(limiter.check("k").is_allow());
        }
        assert!(limiter.check("k").is_deny());
        // Awaiting must complete once a token refills (~5ms for 200/s). The
        // timeout guards against a hang rather than asserting exact timing,
        // which the OS scheduler makes nondeterministic.
        let completed =
            tokio::time::timeout(Duration::from_secs(10), limiter.try_until_ready("k")).await;
        assert_eq!(completed.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn test_try_until_ready_n_reports_impossible_requests() {
        let limiter = AsyncLimiter::new(RateLimiter::per_second(5));
        // 6 > capacity of 5 → can never succeed; must return an error, not hang.
        assert_eq!(
            limiter.try_until_ready_n("k", 6).await,
            Err(InsufficientCapacity::new(6))
        );
        // A limiter that admits nothing reports it for a single unit too.
        let closed = AsyncLimiter::new(RateLimiter::per_second(0));
        assert_eq!(
            closed.try_until_ready("k").await,
            Err(InsufficientCapacity::new(1))
        );
        // Nothing was counted: the key still has its full allowance.
        assert!(limiter.check_n("k", 5).is_allow());
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn test_deprecated_until_ready_still_terminates() {
        let limiter = AsyncLimiter::new(RateLimiter::per_second(5));
        limiter.until_ready_n("k", 6).await;
        limiter.until_ready("k").await;
        assert_eq!(limiter.inner().tracked_keys(), 1);
    }

    #[tokio::test]
    async fn test_cancelled_wait_consumes_nothing() {
        // One unit per minute: the second unit is a minute away, so the wait is
        // cancelled by the timeout long before it could be admitted.
        let limiter = AsyncLimiter::new(RateLimiter::per_minute(1));
        assert!(limiter.check("k").is_allow());
        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), limiter.try_until_ready("k")).await;
        assert!(cancelled.is_err(), "the wait should have been cancelled");
        // The key is exactly as drained as before: still denied, roughly a
        // minute out.
        let retry = limiter.check("k").retry_after().unwrap();
        assert!(retry > Duration::from_secs(50), "retry {retry:?}");
    }

    #[tokio::test]
    async fn test_concurrent_waiters_are_each_admitted_without_over_admitting() {
        const WAITERS: u32 = 10;
        let limiter = Arc::new(AsyncLimiter::new(RateLimiter::per_second(50)));
        for _ in 0..50 {
            assert!(limiter.check("k").is_allow());
        }
        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..WAITERS {
            let limiter = Arc::clone(&limiter);
            handles.push(tokio::spawn(
                async move { limiter.try_until_ready("k").await },
            ));
        }
        for handle in handles {
            let joined = tokio::time::timeout(Duration::from_secs(5), handle).await;
            assert_eq!(joined.unwrap().unwrap(), Ok(()));
        }
        // Ten more units at 50/s need at least ~180ms of refill (10 x 20ms,
        // less scheduling slack): the waiters waited rather than over-admitting.
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn test_from_into_inner_and_debug() {
        let limiter: AsyncLimiter = RateLimiter::per_second(10).into();
        assert_eq!(limiter.inner().quota().limit(), 10);
        let _ = limiter.check("secret-key");
        let rendered = format!("{limiter:?}");
        assert!(rendered.contains("AsyncLimiter"));
        assert!(!rendered.contains("secret-key"));
        let back = limiter.into_inner();
        assert_eq!(back.quota().limit(), 10);
    }
}
