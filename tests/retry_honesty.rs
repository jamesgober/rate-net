//! `retry_after` is never early: a denied request retried exactly
//! `retry_after` later, with no other traffic in between, is admitted.
//!
//! This is the contract behind `Retry-After`: a client that honours the
//! header must not be denied again on its return. Checked for every
//! algorithm across arbitrary quotas, request sizes, and traffic histories.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use proptest::prelude::*;
use rate_net::{Algorithm, RateLimiter};

#[cfg(feature = "algorithms")]
const ALL: [Algorithm; 5] = [
    Algorithm::TokenBucket,
    Algorithm::LeakyBucket,
    Algorithm::FixedWindow,
    Algorithm::SlidingWindowLog,
    Algorithm::SlidingWindowCounter,
];

#[cfg(not(feature = "algorithms"))]
const ALL: [Algorithm; 1] = [Algorithm::TokenBucket];

/// Plays `steps` of `(advance, units)` traffic; after every denial with a
/// finite wait, advances exactly that wait and asserts the same request is
/// now admitted.
fn assert_retry_is_honest(
    algorithm: Algorithm,
    limit: u32,
    period: Duration,
    burst: u32,
    steps: &[(u64, u32)],
) -> Result<(), TestCaseError> {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .quota(limit, period)
        .burst(burst)
        .shards(1)
        .clock(Arc::clone(&clock))
        .build();
    for &(advance_us, units) in steps {
        clock.advance(Duration::from_micros(advance_us));
        let decision = limiter.check_n("k", units);
        if decision.is_impossible() {
            continue;
        }
        if let Some(wait) = decision.retry_after() {
            prop_assert!(wait > Duration::ZERO, "{algorithm:?}: zero retry_after");
            clock.advance(wait);
            let retried = limiter.check_n("k", units);
            prop_assert!(
                retried.is_allow(),
                "{algorithm:?} limit {limit}/{period:?} burst {burst}: {units} unit(s) \
                 denied again after waiting the reported {wait:?}: {retried:?}"
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_retry_after_is_never_early(
        algorithm in prop::sample::select(ALL.to_vec()),
        limit in 1u32..=40,
        period_ms in prop::sample::select(vec![1u64, 7, 100, 1_000, 60_000]),
        burst_extra in 0u32..=20,
        steps in prop::collection::vec((0u64..=400_000, 1u32..=8), 1..60),
    ) {
        let burst = limit + burst_extra;
        assert_retry_is_honest(
            algorithm,
            limit,
            Duration::from_millis(period_ms),
            burst,
            &steps,
        )?;
    }
}

#[test]
fn test_sliding_counter_audit_case_is_honest() {
    // The verifier's case: 10/s, ten spent at t=0, a check at t=0.5s.
    #[cfg(feature = "algorithms")]
    {
        let steps = [(0u64, 10u32), (500_000, 1)];
        let outcome = assert_retry_is_honest(
            Algorithm::SlidingWindowCounter,
            10,
            Duration::from_secs(1),
            10,
            &steps,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
    }
}
