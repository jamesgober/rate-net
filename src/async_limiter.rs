//! Optional async-friendly wrapper.
//!
//! The core is sync and runtime-free: [`check`](crate::RateLimiter::check) never
//! blocks, so most async callers just call it and shed load on a denial. This
//! layer adds the one thing that genuinely needs a runtime — *awaiting* until a
//! key is allowed — behind the `async` feature, so the core never depends on a
//! runtime.
//!
//! [`AsyncLimiter::until_ready`] retries on each denial, sleeping for the
//! reported `retry_after` via [`tokio::time::sleep`], until the request is
//! admitted. It needs a clock that actually advances (the default
//! [`SystemClock`](clock_lib::SystemClock)); under a frozen `ManualClock` the
//! allowance never refills, so it would wait forever.

use core::time::Duration;

use clock_lib::{Clock, SystemClock};

use crate::decision::Decision;
use crate::key::Key;
use crate::limiter::RateLimiter;

/// An async-friendly wrapper around a [`RateLimiter`].
///
/// Holds a limiter and adds [`until_ready`](Self::until_ready) /
/// [`until_ready_n`](Self::until_ready_n), which await until the key is admitted
/// rather than returning a denial. The synchronous [`check`](Self::check) /
/// [`check_n`](Self::check_n) pass straight through.
///
/// # Examples
///
/// ```
/// use rate_net::{AsyncLimiter, RateLimiter};
///
/// # async fn demo() {
/// let limiter = AsyncLimiter::new(RateLimiter::per_second(100));
///
/// // Non-blocking: returns immediately, allow or deny.
/// let _ = limiter.check("user:42");
///
/// // Awaiting: returns once the key is within its limit.
/// limiter.until_ready("user:42").await;
/// # }
/// ```
pub struct AsyncLimiter<C: Clock + Clone = SystemClock> {
    inner: RateLimiter<C>,
}

impl<C: Clock + Clone> AsyncLimiter<C> {
    /// Wraps a limiter for async use.
    #[must_use]
    pub fn new(inner: RateLimiter<C>) -> Self {
        Self { inner }
    }

    /// Borrows the wrapped limiter.
    #[must_use]
    pub fn inner(&self) -> &RateLimiter<C> {
        &self.inner
    }

    /// Unwraps back into the limiter.
    #[must_use]
    pub fn into_inner(self) -> RateLimiter<C> {
        self.inner
    }

    /// Checks a single unit against `key` without awaiting — a straight
    /// pass-through to [`RateLimiter::check`].
    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.inner.check(key)
    }

    /// Checks `n` units against `key` without awaiting — a straight pass-through
    /// to [`RateLimiter::check_n`].
    pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.inner.check_n(key, n)
    }

    /// Awaits until a single unit is admitted for `key`.
    ///
    /// Equivalent to `until_ready_n(key, 1)`.
    pub async fn until_ready(&self, key: impl Into<Key>) {
        self.until_ready_n(key, 1).await;
    }

    /// Awaits until `n` units are admitted for `key`, sleeping for the reported
    /// `retry_after` after each denial.
    ///
    /// Returns immediately if `n` exceeds what the limit can ever grant (a
    /// `retry_after` of [`Duration::MAX`]): no amount of waiting would help, so
    /// it gives up rather than sleep forever.
    pub async fn until_ready_n(&self, key: impl Into<Key>, n: u32) {
        let key = key.into();
        loop {
            match self.inner.check_n(key.clone(), n) {
                Decision::Allow => return,
                Decision::Deny { retry_after } => {
                    if retry_after == Duration::MAX {
                        return; // can never succeed; do not wait forever
                    }
                    tokio::time::sleep(retry_after).await;
                }
            }
        }
    }
}

impl<C: Clock + Clone> From<RateLimiter<C>> for AsyncLimiter<C> {
    fn from(inner: RateLimiter<C>) -> Self {
        Self::new(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncLimiter;
    use crate::limiter::RateLimiter;

    #[test]
    fn test_check_passthrough_is_sync() {
        let limiter = AsyncLimiter::new(RateLimiter::per_second(1));
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_deny());
    }

    #[tokio::test]
    async fn test_until_ready_admits_after_waiting() {
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
            tokio::time::timeout(std::time::Duration::from_secs(2), limiter.until_ready("k")).await;
        assert!(completed.is_ok(), "until_ready did not complete within 2s");
    }

    #[tokio::test]
    async fn test_until_ready_n_gives_up_when_impossible() {
        let limiter = AsyncLimiter::new(RateLimiter::per_second(5));
        // 6 > capacity of 5 → can never succeed; must return without hanging.
        limiter.until_ready_n("k", 6).await;
    }

    #[test]
    fn test_from_and_into_inner_round_trip() {
        let limiter: AsyncLimiter = RateLimiter::per_second(10).into();
        assert_eq!(limiter.inner().quota().limit(), 10);
        let back = limiter.into_inner();
        assert_eq!(back.quota().limit(), 10);
    }
}
