//! The `metrics` feature's counters track every decision and every key
//! lifecycle event.

#![cfg(feature = "metrics")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{DirectLimiter, Eviction, Metrics, Overflow, RateLimiter};

#[test]
fn test_decisions_and_inserts_are_counted() {
    let limiter = RateLimiter::per_second(3);
    for user in 0..4u64 {
        for _ in 0..5 {
            let _ = limiter.check(user);
        }
    }
    let metrics = limiter.metrics();
    assert_eq!(metrics.allowed, 12);
    assert_eq!(metrics.denied, 8);
    assert_eq!(metrics.keys_inserted, 4);
    assert_eq!(metrics.keys_evicted, 0);
}

#[test]
fn test_capacity_evictions_are_counted() {
    let limiter = RateLimiter::per_second(1)
        .with_shards(1)
        .with_eviction(Eviction::capacity(10).with_overflow(Overflow::FailClosed));
    for user in 0..100u64 {
        let _ = limiter.check(user);
    }
    let metrics = limiter.metrics();
    // Every key is drained (one unit a second) and evicted owing it: its debt
    // is remembered, exactly or in the overflow; once a 10-key shard is
    // saturated with keys that cannot be evicted without losing a debt, a
    // newcomer is refused instead of stored.
    assert_eq!(metrics.keys_inserted + metrics.keys_refused, 100);
    assert_eq!(metrics.keys_evicted, metrics.keys_inserted - 10);
    assert_eq!(metrics.keys_remembered, metrics.keys_evicted);
    assert!(metrics.debts_overflowed <= metrics.keys_remembered);
    assert_eq!(limiter.tracked_keys(), 10);

    // Keys that spend nothing recover at once and are evicted losslessly.
    let lossless = RateLimiter::per_second(1)
        .with_shards(1)
        .with_eviction(Eviction::capacity(10));
    for user in 0..100u64 {
        let _ = lossless.check_n(user, 0);
    }
    let metrics = lossless.metrics();
    assert_eq!((metrics.keys_inserted, metrics.keys_evicted), (100, 90));
    assert_eq!((metrics.keys_remembered, metrics.keys_refused), (0, 0));

    // The default `Overflow::Forget`: every evicted debt is either kept
    // exactly or dropped (counted), nothing spills or is refused.
    let forgetting = RateLimiter::per_second(1)
        .with_shards(1)
        .with_eviction(Eviction::capacity(10));
    for user in 0..1_000u64 {
        let _ = forgetting.check(user);
    }
    let metrics = forgetting.metrics();
    assert_eq!((metrics.keys_inserted, metrics.keys_evicted), (1_000, 990));
    assert_eq!(metrics.keys_remembered + metrics.debts_forgotten, 990);
    assert!(metrics.debts_forgotten > 0);
    assert_eq!((metrics.debts_overflowed, metrics.keys_refused), (0, 0));
}

#[test]
fn test_idle_expiry_and_removals_are_counted() {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_second(1)
        .shards(1)
        .eviction(Eviction::new(1_000, Duration::from_secs(1)))
        .clock(Arc::clone(&clock))
        .build();
    for user in 0..4u64 {
        let _ = limiter.check(user);
    }
    clock.advance(Duration::from_secs(5));
    let _ = limiter.check("newcomer"); // sweeps the four idle keys
    assert_eq!(limiter.metrics().keys_expired, 4);

    assert!(limiter.remove("newcomer"));
    for user in 0..3u64 {
        let _ = limiter.check(user);
    }
    limiter.clear();
    let metrics = limiter.metrics();
    assert_eq!(metrics.keys_removed, 4);
    assert_eq!(metrics.keys_inserted, 8);
}

#[test]
fn test_direct_limiter_counts_decisions() {
    let limiter = DirectLimiter::per_second(2);
    for _ in 0..5 {
        let _ = limiter.check();
    }
    let metrics = limiter.metrics();
    assert_eq!((metrics.allowed, metrics.denied), (2, 3));
    assert_eq!(metrics.keys_inserted, 0);
    assert_ne!(metrics, Metrics::default());
}
