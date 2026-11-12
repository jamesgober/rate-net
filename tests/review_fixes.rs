//! Regression tests for the 2.2.0 review fixes. Each fails against 2.1.0.
//!
//! - Debts recorded under a slow per-key quota must not charge newcomers under
//!   a faster quota through overflow cells only the slow quota wrote, and a
//!   sketch charge must never hold a newcomer back longer than its own
//!   recovery time; `remove` followed by waiting out that time must give a
//!   fresh allowance.
//! - The sliding-window log caps its limit, and so its memory, on every
//!   construction path, not only `try_build`.
//! - The keyed leaky bucket does not admit a second unit 1 ms after the first
//!   at burst 1.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Eviction, Key, Quota, RateLimiter};

/// A limiter whose default quota recovers in one second, with a one-per-hour
/// "slow" class, one shard and a tiny cap, flooded with slow keys that each
/// spend their unit: far more one-hour debts than the exact table holds, so
/// they fill the shared overflow sketch.
fn flooded() -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_second(100)
        .quota_for(|key: &Key| {
            if key.as_bytes().starts_with(b"slow") {
                Quota::rate(1, Duration::from_secs(3_600)).ok()
            } else {
                None
            }
        })
        .shards(1)
        .eviction(Eviction::capacity(64))
        .clock(Arc::clone(&clock))
        .build();
    for i in 0..4_000u32 {
        let _ = limiter.check(format!("slow{i}"));
    }
    (clock, limiter)
}

#[test]
fn slow_quota_debts_do_not_charge_newcomers_under_another_quota() {
    // 2.1.0 denied every one of these users for an hour. Overflow cells that
    // only the slow class wrote hold none of their debt, so they are not
    // charged at all. (Each user recovers before the next one arrives, so no
    // user debt ever spills into the sketch.)
    let (clock, limiter) = flooded();
    for j in 0..1_000u32 {
        let decision = limiter.check(format!("user{j}"));
        assert!(decision.is_allow(), "user{j}: {decision:?}");
        clock.advance(Duration::from_millis(20));
    }
}

#[test]
fn a_sketch_charge_never_outlasts_the_newcomers_own_recovery() {
    // Users arriving faster than they recover spill their own small debts
    // into cells the slow class wrote, so those cells charge everyone. Such a
    // charge is cut to the newcomer's own one-second refill, never the slow
    // quota's hour.
    let (clock, limiter) = flooded();
    for j in 0..1_000u32 {
        if let Some(wait) = limiter.check(format!("user{j}")).retry_after() {
            assert!(
                wait <= Duration::from_secs(1),
                "user{j} held back {wait:?}, longer than its own 1 s recovery"
            );
        }
    }
    // `remove`, then waiting out the key's own recovery time, gives it a
    // fresh allowance whatever the sketch holds.
    let key = "user500";
    let _ = limiter.remove(key);
    let _ = limiter.check(key);
    clock.advance(Duration::from_secs(1));
    assert!(limiter.check(key).is_allow());
}

#[test]
#[cfg(feature = "algorithms")]
fn sliding_log_limit_is_capped_on_the_infallible_path() {
    use rate_net::{Algorithm, MAX_SLIDING_LOG_LIMIT};

    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::per_second(10_000_000)
        .with_algorithm(Algorithm::SlidingWindowLog)
        .with_clock(Arc::clone(&clock));
    let mut admitted = 0u32;
    for _ in 0..(MAX_SLIDING_LOG_LIMIT + 10_000) {
        clock.advance(Duration::from_nanos(1));
        if limiter.check(1u64).is_allow() {
            admitted += 1;
        }
    }
    // All within one second: the log holds at most the capped limit.
    assert_eq!(admitted, MAX_SLIDING_LOG_LIMIT);
    assert_eq!(limiter.peek(1u64).limit(), MAX_SLIDING_LOG_LIMIT);

    let built = RateLimiter::builder()
        .per_second(10_000_000)
        .algorithm(Algorithm::SlidingWindowLog)
        .clock(Arc::clone(&clock))
        .build();
    assert!(built.check(1u64).is_allow());
    assert_eq!(built.peek(1u64).limit(), MAX_SLIDING_LOG_LIMIT);
}

#[test]
#[cfg(feature = "algorithms")]
fn keyed_leaky_bucket_keeps_its_spacing_across_an_interval_boundary() {
    use rate_net::Algorithm;

    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_second(1)
        .burst(1)
        .algorithm(Algorithm::LeakyBucket)
        .clock(Arc::clone(&clock))
        .build();
    assert!(limiter.check("k").is_allow());
    clock.advance(Duration::from_millis(1_999));
    assert!(limiter.check("k").is_allow());
    clock.advance(Duration::from_millis(1));
    assert!(
        limiter.check("k").is_deny(),
        "two units 1 ms apart at burst 1"
    );
    clock.advance(Duration::from_millis(999));
    assert!(limiter.check("k").is_allow());
}
