//! Construction-time configuration errors.
//!
//! The check path is infallible — it returns a [`Decision`](crate::Decision),
//! never a `Result`. The only fallible operation is describing a limit: a
//! [`Quota`](crate::Quota) built from values that cannot describe a working
//! limiter is rejected up front rather than producing a limiter that silently
//! misbehaves.
//!
//! [`RateLimiterError`] implements [`error_forge::ForgeError`], so it slots into
//! the portfolio error stack (kinds, captions, the central error hook) the same
//! way every other domain error does.

use core::fmt;

use error_forge::ForgeError;

/// A limit configuration rejected at construction time.
///
/// Returned by [`Quota::rate`](crate::Quota::rate) when the supplied values
/// cannot describe a working rate limit. Each variant names exactly which
/// constraint was violated so the caller can correct the specific field.
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
}

impl fmt::Display for RateLimiterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ZeroQuota => "quota limit must be greater than zero",
            Self::ZeroPeriod => "quota period must be greater than zero",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RateLimiterError {}

impl ForgeError for RateLimiterError {
    fn kind(&self) -> &'static str {
        match self {
            Self::ZeroQuota => "ZeroQuota",
            Self::ZeroPeriod => "ZeroPeriod",
        }
    }

    fn caption(&self) -> &'static str {
        "Invalid rate-limit configuration"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::RateLimiterError;
    use error_forge::ForgeError;

    #[test]
    fn test_display_names_the_violated_constraint() {
        assert!(RateLimiterError::ZeroQuota.to_string().contains("limit"));
        assert!(RateLimiterError::ZeroPeriod.to_string().contains("period"));
    }

    #[test]
    fn test_forge_kind_matches_variant() {
        assert_eq!(RateLimiterError::ZeroQuota.kind(), "ZeroQuota");
        assert_eq!(RateLimiterError::ZeroPeriod.kind(), "ZeroPeriod");
    }

    #[test]
    fn test_config_errors_are_not_retryable() {
        // A bad configuration will not fix itself on retry.
        assert!(!RateLimiterError::ZeroQuota.is_retryable());
    }
}
