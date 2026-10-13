//! The `metrics` feature's counters track every decision and every key
//! lifecycle event.

#![cfg(feature = "metrics")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{DirectLimiter, Eviction, Metrics, RateLimiter};

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
        .with_eviction(Eviction::capacity(10));
    for user in 0..100u64 {
        let _ = limiter.check(user);
    }
    let metrics = limiter.metrics();
    assert_eq!(metrics.keys_inserted, 100);
    assert_eq!(metrics.keys_evicted, 90);
    assert_eq!(limiter.tracked_keys(), 10);
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
