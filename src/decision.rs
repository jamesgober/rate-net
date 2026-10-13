//! The outcome of a rate-limit check.

use core::time::Duration;

/// The result of checking a key against its limit.
///
/// Returned by [`RateLimiter::check`](crate::RateLimiter::check) and
/// [`check_n`](crate::RateLimiter::check_n). A check is infallible — there is no
/// error case on the request path, only an allow/deny outcome — so this is a
/// plain enum rather than a `Result`. When a request is denied, the decision
/// carries how long the caller should wait before enough capacity will have
/// accrued, which is exactly what an HTTP `Retry-After` header needs.
///
/// `#[non_exhaustive]` so future variants can be added without breaking callers;
/// match with a wildcard arm, or use the [`is_allow`](Self::is_allow) /
/// [`retry_after`](Self::retry_after) helpers.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "std")] {
/// use rate_net::{RateLimiter, Decision};
///
/// let limiter = RateLimiter::per_second(1);
/// match limiter.check("user:42") {
///     Decision::Allow => { /* serve the request */ }
///     Decision::Deny { retry_after } => {
///         // return 429 with `Retry-After: {retry_after}`
///         let _ = retry_after;
///     }
///     _ => {}
/// }
/// # }
/// ```
#[must_use]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request is within the limit and has been counted against the key.
    Allow,
    /// The request would exceed the key's limit and was refused.
    Deny {
        /// The minimum wait until the same request would be admitted. A value of
        /// [`Duration::MAX`] means it can never succeed — the request asked for
        /// more than the limit's burst capacity.
        retry_after: Duration,
    },
}

impl Decision {
    /// Returns `true` if the request was admitted.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Decision;
    ///
    /// assert!(Decision::Allow.is_allow());
    /// ```
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Returns `true` if the request was refused.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Decision;
    /// use std::time::Duration;
    ///
    /// let denied = Decision::Deny { retry_after: Duration::from_millis(250) };
    /// assert!(denied.is_deny());
    /// ```
    #[must_use]
    pub const fn is_deny(&self) -> bool {
        !self.is_allow()
    }

    /// Returns the wait until the request would be admitted, or `None` if it was
    /// allowed.
    ///
    /// Use it to populate an HTTP `Retry-After` header on a `429` response, or
    /// to drive a client-side backoff.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Decision;
    /// use std::time::Duration;
    ///
    /// let denied = Decision::Deny { retry_after: Duration::from_millis(250) };
    /// assert_eq!(denied.retry_after(), Some(Duration::from_millis(250)));
    /// assert_eq!(Decision::Allow.retry_after(), None);
    /// ```
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Deny { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    /// Returns `true` if the request was refused permanently: it asked for more
    /// than the limit can ever grant at once, or the limiter admits nothing.
    /// Its `retry_after` is [`Duration::MAX`] and no wait will help.
    ///
    /// An HTTP service should answer such a request with a client error (`400`
    /// or `413`) rather than `429`, since retrying is pointless.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Decision;
    /// use std::time::Duration;
    ///
    /// assert!(Decision::Deny { retry_after: Duration::MAX }.is_impossible());
    /// assert!(!Decision::Deny { retry_after: Duration::from_secs(1) }.is_impossible());
    /// assert!(!Decision::Allow.is_impossible());
    /// ```
    #[must_use]
    pub fn is_impossible(&self) -> bool {
        matches!(self, Self::Deny { retry_after } if *retry_after == Duration::MAX)
    }

    /// The HTTP `Retry-After` value for a denial: whole seconds, **rounded
    /// up**, and at least `1`.
    ///
    /// Rounding up is what makes the header honest: a 1.9 s wait truncated to
    /// `1` sends clients back early to be denied again. Returns `None` when
    /// there is nothing to wait for — the request was allowed — or nothing
    /// worth waiting for — it is [impossible](Self::is_impossible).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Decision;
    /// use std::time::Duration;
    ///
    /// let denied = Decision::Deny { retry_after: Duration::from_millis(1_900) };
    /// assert_eq!(denied.retry_after_secs(), Some(2));
    ///
    /// let soon = Decision::Deny { retry_after: Duration::from_millis(5) };
    /// assert_eq!(soon.retry_after_secs(), Some(1));
    ///
    /// assert_eq!(Decision::Allow.retry_after_secs(), None);
    /// assert_eq!(Decision::Deny { retry_after: Duration::MAX }.retry_after_secs(), None);
    /// ```
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Deny { retry_after } if *retry_after != Duration::MAX => {
                Some(crate::headers::ceil_secs(*retry_after).max(1))
            }
            _ => None,
        }
    }
}

#[cfg(feature = "std")]
impl From<better_bucket::Decision> for Decision {
    /// Lifts a [`better_bucket::Decision`] into the rate-net decision. The token
    /// bucket's `Allowed`/`Denied { retry_after }` maps directly; any future
    /// variant added upstream is treated, conservatively, as a denial that can
    /// never succeed.
    fn from(decision: better_bucket::Decision) -> Self {
        match decision {
            better_bucket::Decision::Allowed => Self::Allow,
            better_bucket::Decision::Denied { retry_after } => Self::Deny { retry_after },
            _ => Self::Deny {
                retry_after: Duration::MAX,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Decision;
    use core::time::Duration;

    #[test]
    fn test_allow_predicates() {
        let allow = Decision::Allow;
        assert!(allow.is_allow());
        assert!(!allow.is_deny());
        assert_eq!(allow.retry_after(), None);
    }

    #[test]
    fn test_retry_after_secs_rounds_up_and_skips_impossible() {
        let cases = [
            (Duration::from_nanos(1), Some(1)),
            (Duration::from_millis(999), Some(1)),
            (Duration::from_secs(1), Some(1)),
            (Duration::from_millis(1_001), Some(2)),
            (Duration::ZERO, Some(1)),
            (Duration::MAX, None),
        ];
        for (wait, expected) in cases {
            let deny = Decision::Deny { retry_after: wait };
            assert_eq!(deny.retry_after_secs(), expected, "{wait:?}");
            assert_eq!(deny.is_impossible(), wait == Duration::MAX);
        }
        assert_eq!(Decision::Allow.retry_after_secs(), None);
    }

    #[test]
    fn test_deny_predicates() {
        let deny = Decision::Deny {
            retry_after: Duration::from_secs(2),
        };
        assert!(deny.is_deny());
        assert!(!deny.is_allow());
        assert_eq!(deny.retry_after(), Some(Duration::from_secs(2)));
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_from_better_bucket_allowed_maps_to_allow() {
        assert_eq!(
            Decision::from(better_bucket::Decision::Allowed),
            Decision::Allow
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_from_better_bucket_denied_carries_retry_after() {
        let upstream = better_bucket::Decision::Denied {
            retry_after: Duration::from_millis(500),
        };
        assert_eq!(
            Decision::from(upstream),
            Decision::Deny {
                retry_after: Duration::from_millis(500)
            }
        );
    }
}
