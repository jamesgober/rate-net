//! The rate-limiting algorithm a limiter applies.

use core::time::Duration;

use crate::error::RateLimiterError;
use crate::quota::Quota;

/// The largest `limit` the validating builder accepts for the sliding-window
/// log (`Algorithm::SlidingWindowLog`, under the `algorithms` feature).
///
/// The log keeps one 16-byte record per distinct admit time in the trailing
/// window, so a key's worst case is `16 * limit` bytes — 1 MiB at this ceiling.
/// Above it, the sliding-window counter gives a close approximation in O(1)
/// memory per key. The infallible `build` does not enforce the ceiling; only
/// `try_build` rejects a larger limit, with
/// [`RateLimiterError::LimitTooLarge`].
///
/// # Examples
///
/// ```
/// assert_eq!(rate_net::MAX_SLIDING_LOG_LIMIT, 65_536);
/// ```
pub const MAX_SLIDING_LOG_LIMIT: u32 = 1 << 16;

/// Selects the algorithm a limiter uses to decide a request.
///
/// Every algorithm shares the same [`Limiter`](crate::Limiter) surface, so the
/// strategy can change without touching call sites. This enum is the selector a
/// future builder uses to pick between them.
///
/// `#[non_exhaustive]`: algorithms are added over the `0.x` series, so a `match`
/// must include a wildcard arm. [`TokenBucket`](Self::TokenBucket) — the default
/// — is always available; the leaky bucket and the window algorithms are
/// compiled in under the `algorithms` feature, so their variants only exist when
/// it is enabled.
///
/// # Examples
///
/// ```
/// use rate_net::Algorithm;
///
/// // The default is the token bucket — smooth refill with burst headroom.
/// assert_eq!(Algorithm::default(), Algorithm::TokenBucket);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Algorithm {
    /// Smooth refill with burst headroom up to the configured capacity. The
    /// general-purpose default; delegates its accounting to `better-bucket`.
    #[default]
    TokenBucket,
    /// Constant-drain shaping that smooths bursts to a steady output rate.
    /// Requires the `algorithms` feature.
    #[cfg(feature = "algorithms")]
    LeakyBucket,
    /// A counter that resets each window; cheapest, tolerates boundary bursts.
    /// Requires the `algorithms` feature.
    #[cfg(feature = "algorithms")]
    FixedWindow,
    /// Exact request timestamps within the trailing window; highest accuracy,
    /// higher memory. Requires the `algorithms` feature.
    #[cfg(feature = "algorithms")]
    SlidingWindowLog,
    /// A weighted blend of the current and previous window; an accuracy/cost
    /// balance and a common production choice. Requires the `algorithms` feature.
    #[cfg(feature = "algorithms")]
    SlidingWindowCounter,
}

impl Algorithm {
    /// How long an idle key takes to become indistinguishable from a fresh one
    /// under `quota`: the token and leaky buckets refill their whole burst, a
    /// fixed window or log ages out one period, and the sliding-window counter
    /// lets its previous window fade over a second period.
    ///
    /// Evicting an idle key earlier than this would hand it a fresh allowance
    /// early, so it is the floor for any idle TTL. A quota that admits nothing
    /// has no state worth keeping, so its recovery time is zero.
    pub(crate) fn recovery_time(self, quota: &Quota) -> Duration {
        if quota.limit() == 0 || quota.period().is_zero() {
            return Duration::ZERO;
        }
        match self {
            Self::TokenBucket => refill_time(quota),
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket => refill_time(quota),
            #[cfg(feature = "algorithms")]
            Self::FixedWindow | Self::SlidingWindowLog => quota.period(),
            #[cfg(feature = "algorithms")]
            Self::SlidingWindowCounter => quota.period().saturating_mul(2),
        }
    }

    /// Checks that `quota` describes a working limit for this algorithm.
    ///
    /// The infallible constructors accept anything and fail closed; this is the
    /// strict path behind the validating builder.
    pub(crate) fn validate(self, quota: &Quota) -> Result<(), RateLimiterError> {
        if quota.limit() == 0 {
            return Err(RateLimiterError::ZeroQuota);
        }
        if quota.period().is_zero() {
            return Err(RateLimiterError::ZeroPeriod);
        }
        match self {
            Self::TokenBucket => non_zero_burst(quota),
            #[cfg(feature = "algorithms")]
            Self::LeakyBucket => {
                non_zero_burst(quota)?;
                // One emission interval must be at least a nanosecond.
                if u128::from(quota.limit()) > quota.period().as_nanos() {
                    return Err(RateLimiterError::RateTooHigh);
                }
                Ok(())
            }
            #[cfg(feature = "algorithms")]
            Self::SlidingWindowLog => {
                if quota.limit() > MAX_SLIDING_LOG_LIMIT {
                    return Err(RateLimiterError::LimitTooLarge);
                }
                Ok(())
            }
            #[cfg(feature = "algorithms")]
            Self::FixedWindow | Self::SlidingWindowCounter => Ok(()),
        }
    }
}

/// The bucket algorithms admit nothing with a zero burst.
fn non_zero_burst(quota: &Quota) -> Result<(), RateLimiterError> {
    if quota.burst() == 0 {
        Err(RateLimiterError::ZeroBurst)
    } else {
        Ok(())
    }
}

/// Time for a bucket to refill its whole burst: `burst * period / limit`,
/// rounded up to the nanosecond.
fn refill_time(quota: &Quota) -> Duration {
    let scaled = u128::from(quota.burst()) * quota.period().as_nanos();
    let nanos = scaled.div_ceil(u128::from(quota.limit()));
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::Algorithm;
    use crate::error::RateLimiterError;
    use crate::quota::Quota;

    #[test]
    fn test_default_is_token_bucket() {
        assert_eq!(Algorithm::default(), Algorithm::TokenBucket);
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_variants_are_distinct() {
        assert_ne!(Algorithm::TokenBucket, Algorithm::LeakyBucket);
        assert_ne!(Algorithm::FixedWindow, Algorithm::SlidingWindowLog);
    }

    #[test]
    fn test_token_bucket_recovery_is_burst_refill_time() {
        let quota = Quota::per_second(10);
        assert_eq!(
            Algorithm::TokenBucket.recovery_time(&quota),
            Duration::from_secs(1)
        );
        // A burst of 30 at 10/s takes three seconds to refill.
        assert_eq!(
            Algorithm::TokenBucket.recovery_time(&quota.with_burst(30)),
            Duration::from_secs(3)
        );
        // Rounded up, never down.
        assert_eq!(
            Algorithm::TokenBucket.recovery_time(&Quota::per_second(3).with_burst(1)),
            Duration::from_nanos(333_333_334)
        );
    }

    #[test]
    fn test_degenerate_quota_has_no_recovery_time() {
        assert_eq!(
            Algorithm::TokenBucket.recovery_time(&Quota::per_second(0)),
            Duration::ZERO
        );
        let zero_period = Quota::from_parts(5, Duration::ZERO, 5);
        assert_eq!(
            Algorithm::TokenBucket.recovery_time(&zero_period),
            Duration::ZERO
        );
    }

    #[test]
    fn test_validate_rejects_each_degenerate_field() {
        let tb = Algorithm::TokenBucket;
        assert_eq!(
            tb.validate(&Quota::per_second(0)),
            Err(RateLimiterError::ZeroQuota)
        );
        assert_eq!(
            tb.validate(&Quota::from_parts(5, Duration::ZERO, 5)),
            Err(RateLimiterError::ZeroPeriod)
        );
        assert_eq!(
            tb.validate(&Quota::per_second(5).with_burst(0)),
            Err(RateLimiterError::ZeroBurst)
        );
        assert_eq!(tb.validate(&Quota::per_second(5)), Ok(()));
        // No representational ceiling on the token bucket's burst.
        assert_eq!(
            tb.validate(&Quota::per_second(10).with_burst(u32::MAX)),
            Ok(())
        );
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_window_algorithm_recovery_times() {
        let quota = Quota::per_second(10);
        let second = Duration::from_secs(1);
        assert_eq!(Algorithm::FixedWindow.recovery_time(&quota), second);
        assert_eq!(Algorithm::SlidingWindowLog.recovery_time(&quota), second);
        assert_eq!(
            Algorithm::SlidingWindowCounter.recovery_time(&quota),
            second * 2
        );
        assert_eq!(
            Algorithm::LeakyBucket.recovery_time(&quota.with_burst(20)),
            second * 2
        );
    }

    #[cfg(feature = "algorithms")]
    #[test]
    fn test_validate_applies_algorithm_ceilings() {
        // The leaky bucket cannot space units closer than a nanosecond.
        let too_fast = Quota::per_second(1_000_000_001);
        assert_eq!(
            Algorithm::LeakyBucket.validate(&too_fast),
            Err(RateLimiterError::RateTooHigh)
        );
        assert_eq!(
            Algorithm::LeakyBucket.validate(&Quota::per_second(1_000_000_000)),
            Ok(())
        );
        assert_eq!(
            Algorithm::LeakyBucket.validate(&Quota::per_second(5).with_burst(0)),
            Err(RateLimiterError::ZeroBurst)
        );
        // The sliding log caps its per-key memory.
        let log = Algorithm::SlidingWindowLog;
        assert_eq!(
            log.validate(&Quota::per_second(super::MAX_SLIDING_LOG_LIMIT)),
            Ok(())
        );
        assert_eq!(
            log.validate(&Quota::per_second(super::MAX_SLIDING_LOG_LIMIT + 1)),
            Err(RateLimiterError::LimitTooLarge)
        );
        // Window algorithms ignore the burst, so zero is not an error there.
        for algorithm in [Algorithm::FixedWindow, Algorithm::SlidingWindowCounter, log] {
            assert_eq!(
                algorithm.validate(&Quota::per_second(5).with_burst(0)),
                Ok(())
            );
        }
    }
}
