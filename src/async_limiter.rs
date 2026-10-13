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

use clock_lib::{Clock, SystemClock};

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
        Self { inner }
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
        loop {
            match self.inner.check_n(key.clone(), n) {
                Decision::Allow => return Ok(()),
                Decision::Deny { retry_after } => {
                    if retry_after == Duration::MAX {
                        return Err(InsufficientCapacity::new(n));
                    }
                    tokio::time::sleep(retry_after).await;
                }
            }
        }
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
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

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
            tokio::time::timeout(Duration::from_secs(2), limiter.try_until_ready("k")).await;
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
