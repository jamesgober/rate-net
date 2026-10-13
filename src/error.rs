//! Construction-time configuration errors, and the async wait's give-up error.
//!
//! The check path is infallible — it returns a [`Decision`](crate::Decision),
//! never a `Result`. The fallible operations are describing a limit and
//! validating a configuration: a [`Quota`](crate::Quota) or a builder
//! configuration that cannot describe a working limiter is rejected up front
//! rather than producing a limiter that silently misbehaves.
//!
//! [`RateLimiterError`] and [`InsufficientCapacity`] implement
//! `error_forge::ForgeError` under the `std` feature, so they slot into the
//! portfolio error stack (kinds, captions, the central error hook) the same way
//! every other domain error does. Both implement [`core::error::Error`] in every
//! build, `no_std` included.

use core::fmt;

/// A limit configuration rejected at construction time.
///
/// Returned by [`Quota::rate`](crate::Quota::rate) and by the validating
/// constructors — `Builder::try_build` and
/// [`Gcra::try_new`](crate::Gcra::try_new) — when the supplied values cannot
/// describe a working rate limit. Each variant names exactly which constraint
/// was violated so the caller can correct the specific field.
///
/// The enum is `#[non_exhaustive]`: future releases may add validation rules
/// (and therefore variants) without it being a breaking change, so a `match` on
/// it must include a wildcard arm.
///
/// # Examples
///
/// ```
/// use rate_net::{Quota, RateLimiterError};
/// use std::time::Duration;
///
/// let err = Quota::rate(0, Duration::from_secs(1)).unwrap_err();
/// assert_eq!(err, RateLimiterError::ZeroQuota);
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimiterError {
    /// The quota limit was zero. A limit that admits nothing per period can
    /// never allow a request; supply a limit of at least `1`.
    ZeroQuota,
    /// The quota period was zero. The limit accrues *per period*, so a
    /// zero-length period is undefined (it implies an infinite rate); supply a
    /// non-zero [`Duration`](core::time::Duration).
    ZeroPeriod,
    /// The burst was zero. The token and leaky buckets could then never admit a
    /// single unit; leave the burst at its default (the limit) or supply at
    /// least `1`.
    ZeroBurst,
    /// The rate is finer than the algorithm can account for: the leaky bucket
    /// spaces units at least one nanosecond apart, so it cannot sustain more
    /// than one unit per nanosecond (`limit` greater than the period in
    /// nanoseconds). Lengthen the period or lower the limit.
    RateTooHigh,
    /// The limit exceeds what the sliding-window log will hold per key. The log
    /// keeps a record per admitted unit (coalesced by timestamp), so its memory
    /// grows with the limit; above
    /// [`MAX_SLIDING_LOG_LIMIT`](crate::MAX_SLIDING_LOG_LIMIT) use the
    /// sliding-window counter, which is O(1) per key.
    LimitTooLarge,
    /// The idle TTL is shorter than the time a key needs to recover its full
    /// allowance. Evicting a key earlier than that hands it a fresh quota
    /// early — an over-admit. Raise the TTL to at least the quota's recovery
    /// time (the period, or `burst / limit` periods for the buckets, or two
    /// periods for the sliding-window counter).
    IdleTtlTooShort,
}

impl RateLimiterError {
    /// A stable, machine-readable name for the variant.
    #[cfg(feature = "std")]
    const fn name(self) -> &'static str {
        match self {
            Self::ZeroQuota => "ZeroQuota",
            Self::ZeroPeriod => "ZeroPeriod",
            Self::ZeroBurst => "ZeroBurst",
            Self::RateTooHigh => "RateTooHigh",
            Self::LimitTooLarge => "LimitTooLarge",
            Self::IdleTtlTooShort => "IdleTtlTooShort",
        }
    }
}

impl fmt::Display for RateLimiterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ZeroQuota => "quota limit must be greater than zero",
            Self::ZeroPeriod => "quota period must be greater than zero",
            Self::ZeroBurst => "quota burst must be greater than zero",
            Self::RateTooHigh => "quota rate exceeds one unit per nanosecond",
            Self::LimitTooLarge => "quota limit exceeds the sliding-window log's per-key ceiling",
            Self::IdleTtlTooShort => "idle TTL is shorter than the quota's recovery time",
        };
        f.write_str(message)
    }
}

impl core::error::Error for RateLimiterError {}

#[cfg(feature = "std")]
impl error_forge::ForgeError for RateLimiterError {
    fn kind(&self) -> &'static str {
        self.name()
    }

    fn caption(&self) -> &'static str {
        "Invalid rate-limit configuration"
    }
}

/// A request that can never be admitted, however long the caller waits.
///
/// Returned by the async wrapper's `try_until_ready` / `try_until_ready_n` when
/// the request asks for more units than the limit can ever grant at once (more
/// than the burst for the buckets, or the limit for the window algorithms), or
/// when the limiter admits nothing at all. Waiting would never help, so the wait
/// gives up and says so instead of returning as if admitted.
///
/// # Examples
///
/// ```
/// use rate_net::InsufficientCapacity;
///
/// let err = InsufficientCapacity::new(10);
/// assert_eq!(err.requested(), 10);
/// assert!(err.to_string().contains("10"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsufficientCapacity {
    requested: u32,
}

impl InsufficientCapacity {
    /// Records that a request for `requested` units can never be admitted.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::InsufficientCapacity;
    ///
    /// assert_eq!(InsufficientCapacity::new(3).requested(), 3);
    /// ```
    #[must_use]
    pub const fn new(requested: u32) -> Self {
        Self { requested }
    }

    /// The number of units the refused request asked for.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::InsufficientCapacity;
    ///
    /// assert_eq!(InsufficientCapacity::new(7).requested(), 7);
    /// ```
    #[must_use]
    pub const fn requested(&self) -> u32 {
        self.requested
    }
}

impl fmt::Display for InsufficientCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a request for {} unit(s) exceeds what the limit can ever grant",
            self.requested
        )
    }
}

impl core::error::Error for InsufficientCapacity {}

#[cfg(feature = "std")]
impl error_forge::ForgeError for InsufficientCapacity {
    fn kind(&self) -> &'static str {
        "InsufficientCapacity"
    }

    fn caption(&self) -> &'static str {
        "Request exceeds rate-limit capacity"
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{InsufficientCapacity, RateLimiterError};
    use error_forge::ForgeError;

    const ALL: [RateLimiterError; 6] = [
        RateLimiterError::ZeroQuota,
        RateLimiterError::ZeroPeriod,
        RateLimiterError::ZeroBurst,
        RateLimiterError::RateTooHigh,
        RateLimiterError::LimitTooLarge,
        RateLimiterError::IdleTtlTooShort,
    ];

    #[test]
    fn test_display_names_the_violated_constraint() {
        assert!(RateLimiterError::ZeroQuota.to_string().contains("limit"));
        assert!(RateLimiterError::ZeroPeriod.to_string().contains("period"));
        assert!(RateLimiterError::ZeroBurst.to_string().contains("burst"));
        assert!(RateLimiterError::RateTooHigh.to_string().contains("rate"));
        assert!(
            RateLimiterError::LimitTooLarge
                .to_string()
                .contains("limit")
        );
        assert!(
            RateLimiterError::IdleTtlTooShort
                .to_string()
                .contains("TTL")
        );
    }

    #[test]
    fn test_forge_kind_matches_variant() {
        for err in ALL {
            assert_eq!(err.kind(), format!("{err:?}"));
            assert_eq!(err.caption(), "Invalid rate-limit configuration");
        }
    }

    #[test]
    fn test_config_errors_are_not_retryable() {
        // A bad configuration will not fix itself on retry.
        for err in ALL {
            assert!(!err.is_retryable());
        }
    }

    #[test]
    fn test_insufficient_capacity_reports_request_and_kind() {
        let err = InsufficientCapacity::new(42);
        assert_eq!(err.requested(), 42);
        assert!(err.to_string().contains("42"));
        assert_eq!(err.kind(), "InsufficientCapacity");
        assert!(!err.caption().is_empty());
        assert!(!err.is_retryable());
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert!(boxed.to_string().contains("exceeds"));
    }
}
