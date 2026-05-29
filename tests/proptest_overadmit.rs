//! Property-based proof of the core safety invariant: a key is never
//! over-admitted.
//!
//! Across any interleaving of checks and (whole-second) time advances, the
//! number of requests a single key has been admitted never exceeds what the
//! token bucket can have made available — the initial burst plus one refill per
//! elapsed second. This is the per-algorithm over-admit invariant for the token
//! bucket; the concurrent interleaving variant is model-checked with `loom`.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use proptest::prelude::*;
use rate_net::RateLimiter;

proptest! {
    /// For `per_second(limit)`, a key admitted across whole-second advances
    /// never exceeds `limit * (elapsed_seconds + 1)`: the full initial burst
    /// plus one refill per elapsed second, which caps cumulative admission.
    #[test]
    fn token_bucket_never_over_admits_single_key(
        limit in 1u32..=64,
        steps in prop::collection::vec((0u32..=100, 0u64..=3), 0..40),
    ) {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(limit).with_clock(Arc::clone(&clock));

        let mut admitted: u64 = 0;
        let mut elapsed_secs: u64 = 0;

        for (attempts, advance_secs) in steps {
            clock.advance(Duration::from_secs(advance_secs));
            elapsed_secs += advance_secs;

            for _ in 0..attempts {
                if limiter.check("k").is_allow() {
                    admitted += 1;
                }
            }

            let ceiling = u64::from(limit) * (elapsed_secs + 1);
            prop_assert!(
                admitted <= ceiling,
                "over-admit: {admitted} admitted, ceiling {ceiling} \
                 (limit={limit}, elapsed={elapsed_secs}s)"
            );
        }
    }

    /// Distinct keys are accounted independently: draining one key never spends
    /// another key's allowance. With `k` distinct keys and a fresh limiter, each
    /// key must admit its full initial burst of `limit`.
    #[test]
    fn distinct_keys_each_get_their_own_allowance(
        limit in 1u32..=32,
        key_count in 1usize..=16,
    ) {
        let limiter = RateLimiter::per_second(limit);

        for k in 0..key_count {
            let key = format!("key:{k}");
            let mut admitted = 0u32;
            for _ in 0..(limit + 5) {
                if limiter.check(key.as_str()).is_allow() {
                    admitted += 1;
                }
            }
            prop_assert_eq!(
                admitted, limit,
                "key {} admitted {} of an expected {}", k, admitted, limit
            );
        }
    }
}
