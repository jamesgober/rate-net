//! The rate-limiting algorithm a limiter applies.

/// Selects the algorithm a limiter uses to decide a request.
///
/// Every algorithm shares the same [`Limiter`](crate::Limiter) surface, so the
/// strategy can change without touching call sites. This enum is the selector a
/// future builder uses to pick between them.
///
/// `#[non_exhaustive]`: algorithms are added over the `0.x` series, so a `match`
/// must include a wildcard arm. As of this release the limiter implements
/// [`TokenBucket`](Self::TokenBucket) — the default; the remaining variants name
/// the surface that lands in later releases.
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
    LeakyBucket,
    /// A counter that resets each window; cheapest, tolerates boundary bursts.
    FixedWindow,
    /// Exact request timestamps within the trailing window; highest accuracy,
    /// higher memory.
    SlidingWindowLog,
    /// A weighted blend of the current and previous window; an accuracy/cost
    /// balance and a common production choice.
    SlidingWindowCounter,
}

#[cfg(test)]
mod tests {
    use super::Algorithm;

    #[test]
    fn test_default_is_token_bucket() {
        assert_eq!(Algorithm::default(), Algorithm::TokenBucket);
    }

    #[test]
    fn test_variants_are_distinct() {
        assert_ne!(Algorithm::TokenBucket, Algorithm::LeakyBucket);
        assert_ne!(Algorithm::FixedWindow, Algorithm::SlidingWindowLog);
    }
}
