//! The rate-limiting algorithm a limiter applies.

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

#[cfg(test)]
mod tests {
    use super::Algorithm;

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
}
