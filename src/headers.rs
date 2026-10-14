//! HTTP header names for rate-limit responses.
//!
//! rate-net is framework-agnostic: it hands back numbers and these names, and
//! the caller writes them into whatever response type its server uses.
//!
//! - [`RETRY_AFTER`] (RFC 9110 §10.2.3) on a denial, from
//!   [`Decision::retry_after_secs`](crate::Decision::retry_after_secs) — whole
//!   seconds, rounded **up**, so a client that honours it never retries early.
//! - [`RATELIMIT_LIMIT`], [`RATELIMIT_REMAINING`], and [`RATELIMIT_RESET`] — the
//!   three `RateLimit-*` fields of the IETF rate-limit headers draft
//!   (`draft-ietf-httpapi-ratelimit-headers`, as widely deployed), from
//!   [`Snapshot::headers`](crate::Snapshot::headers). They can go on every
//!   response, admitted or not.
//!
//! # Examples
//!
//! ```
//! # #[cfg(feature = "std")] {
//! use rate_net::{headers, RateLimiter};
//!
//! let limiter = RateLimiter::per_minute(2);
//! let mut response_headers: Vec<(&str, String)> = Vec::new();
//!
//! let _ = limiter.check("client");
//! let _ = limiter.check("client");
//! let decision = limiter.check("client");
//!
//! if let Some(secs) = decision.retry_after_secs() {
//!     response_headers.push((headers::RETRY_AFTER, secs.to_string()));
//! }
//! for (name, value) in limiter.peek("client").headers() {
//!     response_headers.push((name, value.to_string()));
//! }
//!
//! assert_eq!(response_headers[0].0, "Retry-After");
//! assert_eq!(response_headers[2], ("RateLimit-Remaining", "0".to_string()));
//! # }
//! ```

/// `Retry-After`: seconds until the denied request may be retried.
pub const RETRY_AFTER: &str = "Retry-After";

/// `RateLimit-Limit`: the key's capacity — the most units it can spend at once.
pub const RATELIMIT_LIMIT: &str = "RateLimit-Limit";

/// `RateLimit-Remaining`: the units the key could spend right now.
pub const RATELIMIT_REMAINING: &str = "RateLimit-Remaining";

/// `RateLimit-Reset`: seconds until the key's allowance is whole again.
pub const RATELIMIT_RESET: &str = "RateLimit-Reset";

/// Whole seconds in `wait`, rounded up and saturating.
pub(crate) const fn ceil_secs(wait: core::time::Duration) -> u64 {
    let secs = wait.as_secs();
    if wait.subsec_nanos() > 0 {
        secs.saturating_add(1)
    } else {
        secs
    }
}

#[cfg(test)]
mod tests {
    use super::ceil_secs;
    use core::time::Duration;

    #[test]
    fn test_ceil_secs_rounds_up_and_saturates() {
        assert_eq!(ceil_secs(Duration::ZERO), 0);
        assert_eq!(ceil_secs(Duration::from_nanos(1)), 1);
        assert_eq!(ceil_secs(Duration::from_millis(1_900)), 2);
        assert_eq!(ceil_secs(Duration::from_secs(3)), 3);
        assert_eq!(ceil_secs(Duration::MAX), u64::MAX);
    }
}
