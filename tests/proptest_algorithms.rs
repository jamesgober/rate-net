//! Per-algorithm over-admit proofs.
//!
//! Each algorithm has its own safety bound; across any interleaving of checks
//! and time advances a single key never exceeds it. Driven through the full
//! `RateLimiter` (builder + sharded store) on a deterministic `ManualClock`:
//!
//! - **Fixed window** — at most `limit` per aligned window (exact).
//! - **Sliding-window log** — at most `limit` in any rolling window (exact).
//! - **Sliding-window counter** — within `2 * limit` in any rolling window
//!   (the approximation's worst case).
//! - **Leaky bucket (GCRA)** — within `burst + limit` (here `2 * limit`) in any
//!   rolling window.

#![cfg(all(feature = "std", feature = "algorithms"))]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use proptest::prelude::*;
use rate_net::{Algorithm, RateLimiter};

const PERIOD_MS: u64 = 1000;

/// Runs a sequence of `(advance_ms, attempts)` steps against a single key and
/// returns the elapsed-millisecond timestamps at which a unit was admitted.
fn run(algorithm: Algorithm, limit: u32, steps: &[(u64, u32)]) -> Vec<u64> {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .quota(limit, Duration::from_millis(PERIOD_MS))
        .clock(Arc::clone(&clock))
        .build();

    let mut admits = Vec::new();
    let mut now = 0u64;
    for &(advance, attempts) in steps {
        clock.advance(Duration::from_millis(advance));
        now += advance;
        for _ in 0..attempts {
            if limiter.check("k").is_allow() {
                admits.push(now);
            }
        }
    }
    admits
}

/// The number of admits in the half-open rolling window `(t - PERIOD, t]`.
fn trailing(admits: &[u64], t: u64) -> usize {
    let lo = t.saturating_sub(PERIOD_MS);
    admits.iter().filter(|&&s| s > lo && s <= t).count()
}

proptest! {
    #[test]
    fn fixed_window_never_exceeds_limit_per_aligned_window(
        limit in 1u32..=50,
        steps in prop::collection::vec((0u64..=400, 0u32..=25), 0..40),
    ) {
        let admits = run(Algorithm::FixedWindow, limit, &steps);
        let mut per_window: HashMap<u64, usize> = HashMap::new();
        for t in admits {
            *per_window.entry(t / PERIOD_MS).or_default() += 1;
        }
        for (window, count) in per_window {
            prop_assert!(
                count <= limit as usize,
                "fixed window {window} admitted {count} > {limit}"
            );
        }
    }

    #[test]
    fn sliding_log_never_exceeds_limit_in_any_window(
        limit in 1u32..=50,
        steps in prop::collection::vec((0u64..=400, 0u32..=25), 0..40),
    ) {
        let admits = run(Algorithm::SlidingWindowLog, limit, &steps);
        for &t in &admits {
            let count = trailing(&admits, t);
            prop_assert!(
                count <= limit as usize,
                "sliding-log window at {t} held {count} > {limit}"
            );
        }
    }

    #[test]
    fn sliding_counter_stays_within_double_limit(
        limit in 1u32..=50,
        steps in prop::collection::vec((0u64..=400, 0u32..=25), 0..40),
    ) {
        let admits = run(Algorithm::SlidingWindowCounter, limit, &steps);
        let bound = 2 * limit as usize + 1;
        for &t in &admits {
            let count = trailing(&admits, t);
            prop_assert!(
                count <= bound,
                "sliding-counter window at {t} held {count} > {bound}"
            );
        }
    }

    #[test]
    fn leaky_bucket_stays_within_burst_plus_limit(
        limit in 1u32..=50,
        steps in prop::collection::vec((0u64..=400, 0u32..=25), 0..40),
    ) {
        // burst defaults to limit, so the GCRA window bound is limit + burst.
        let admits = run(Algorithm::LeakyBucket, limit, &steps);
        let bound = 2 * limit as usize + 1;
        for &t in &admits {
            let count = trailing(&admits, t);
            prop_assert!(
                count <= bound,
                "leaky window at {t} held {count} > {bound}"
            );
        }
    }
}
