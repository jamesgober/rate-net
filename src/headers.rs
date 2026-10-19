//! HTTP header names for rate-limit responses.
//!
//! rate-net is framework-agnostic: it hands back numbers and these names, and
//! the caller writes them into whatever response type its server uses.
//!
//! - [`RETRY_AFTER`] (RFC 9110 §10.2.3) on a denial, from
//!   [`Decision::retry_after_secs`](crate::Decision::retry_after_secs) — whole
//!   seconds, rounded **up**, so a client that honours it never retries early.
//! - [`RATELIMIT_LIMIT`], [`RATELIMIT_REMAINING`], and [`RATELIMIT_RESET`] — the
//!   three `RateLimit-*` fields of the earlier revisions of the IETF rate-limit
//!   headers draft (`draft-ietf-httpapi-ratelimit-headers`), still the most
//!   widely deployed form, from [`Snapshot::headers`](crate::Snapshot::headers).
//!   They can go on every response, admitted or not.
//! - [`RATELIMIT`] and [`RATELIMIT_POLICY`] — the structured fields of the
//!   current draft: `RateLimit: "name";r=<remaining>;t=<reset>` from
//!   [`Snapshot::ratelimit`](crate::Snapshot::ratelimit), and
//!   `RateLimit-Policy: "name";q=<quota>;w=<window>` from
//!   [`Quota::ratelimit_policy`](crate::Quota::ratelimit_policy). Both format
//!   without allocating.
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

use core::fmt::{self, Write as _};

/// `Retry-After`: seconds until the denied request may be retried.
pub const RETRY_AFTER: &str = "Retry-After";

/// `RateLimit-Limit`: the key's capacity — the most units it can spend at once.
pub const RATELIMIT_LIMIT: &str = "RateLimit-Limit";

/// `RateLimit-Remaining`: the units the key could spend right now.
pub const RATELIMIT_REMAINING: &str = "RateLimit-Remaining";

/// `RateLimit-Reset`: seconds until the key's allowance is whole again.
pub const RATELIMIT_RESET: &str = "RateLimit-Reset";

/// `RateLimit`: the structured field naming a policy and the key's remaining
/// units (`r`) and seconds until they are whole again (`t`). Its value comes
/// from [`Snapshot::ratelimit`](crate::Snapshot::ratelimit).
pub const RATELIMIT: &str = "RateLimit";

/// `RateLimit-Policy`: the structured field naming a policy, its quota (`q`),
/// and its window in seconds (`w`). Its value comes from
/// [`Quota::ratelimit_policy`](crate::Quota::ratelimit_policy).
pub const RATELIMIT_POLICY: &str = "RateLimit-Policy";

/// The largest Structured Field integer (RFC 9651 §3.3.1): fifteen digits.
const MAX_SF_INTEGER: u64 = 999_999_999_999_999;

/// Writes `name` as a Structured Field string: quoted, with `"` and `\`
/// escaped, and any byte outside printable ASCII (which a structured string
/// cannot carry) dropped, so a policy name can never break the field.
fn write_sf_string(f: &mut fmt::Formatter<'_>, name: &str) -> fmt::Result {
    f.write_char('"')?;
    for ch in name.chars() {
        match ch {
            '"' | '\\' => {
                f.write_char('\\')?;
                f.write_char(ch)?;
            }
            ' '..='~' => f.write_char(ch)?,
            _ => {}
        }
    }
    f.write_char('"')
}

/// The value of a [`RATELIMIT`] field for one policy: `"name";r=<remaining>;t=<reset>`.
///
/// Built by [`Snapshot::ratelimit`](crate::Snapshot::ratelimit); formats
/// through [`Display`](fmt::Display) without allocating, so it can be written
/// straight into a header value buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitValue<'a> {
    policy: &'a str,
    remaining: u64,
    reset: u64,
}

impl<'a> RateLimitValue<'a> {
    pub(crate) const fn new(policy: &'a str, remaining: u64, reset: u64) -> Self {
        Self {
            policy,
            remaining,
            reset,
        }
    }
}

impl fmt::Display for RateLimitValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_sf_string(f, self.policy)?;
        write!(
            f,
            ";r={};t={}",
            self.remaining.min(MAX_SF_INTEGER),
            self.reset.min(MAX_SF_INTEGER)
        )
    }
}

/// The value of a [`RATELIMIT_POLICY`] field: `"name";q=<quota>;w=<window>`.
///
/// Built by [`Quota::ratelimit_policy`](crate::Quota::ratelimit_policy);
/// formats through [`Display`](fmt::Display) without allocating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitPolicyValue<'a> {
    policy: &'a str,
    quota: u64,
    window: u64,
}

impl<'a> RateLimitPolicyValue<'a> {
    pub(crate) const fn new(policy: &'a str, quota: u64, window: u64) -> Self {
        Self {
            policy,
            quota,
            window,
        }
    }
}

impl fmt::Display for RateLimitPolicyValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_sf_string(f, self.policy)?;
        write!(
            f,
            ";q={};w={}",
            self.quota.min(MAX_SF_INTEGER),
            self.window.min(MAX_SF_INTEGER)
        )
    }
}

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
    use super::{RateLimitPolicyValue, RateLimitValue, ceil_secs};
    use core::time::Duration;

    #[test]
    fn test_structured_fields_format_and_escape() {
        assert_eq!(
            RateLimitValue::new("default", 7, 3).to_string(),
            r#""default";r=7;t=3"#
        );
        assert_eq!(
            RateLimitPolicyValue::new("burst", 100, 60).to_string(),
            r#""burst";q=100;w=60"#
        );
        // Quotes and backslashes are escaped; non-printable and non-ASCII
        // characters are dropped; integers clamp to fifteen digits.
        assert_eq!(
            RateLimitValue::new("a\"b\\c\n\u{e9}", u64::MAX, u64::MAX).to_string(),
            r#""a\"b\\c";r=999999999999999;t=999999999999999"#
        );
    }

    #[test]
    fn test_ceil_secs_rounds_up_and_saturates() {
        assert_eq!(ceil_secs(Duration::ZERO), 0);
        assert_eq!(ceil_secs(Duration::from_nanos(1)), 1);
        assert_eq!(ceil_secs(Duration::from_millis(1_900)), 2);
        assert_eq!(ceil_secs(Duration::from_secs(3)), 3);
        assert_eq!(ceil_secs(Duration::MAX), u64::MAX);
    }
}
