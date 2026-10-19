//! Saturation guardrails: the gauge and counters an operator alerts on before
//! and when the store runs out of room for evicted keys' debts. The
//! rate-limited `tracing` warnings are in `saturation_warnings.rs`.

#![cfg(feature = "metrics")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Eviction, Overflow, RateLimiter};

fn limiter(overflow: Overflow) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_minute(1)
        .shards(1)
        .eviction(Eviction::capacity(64).with_overflow(overflow))
        .clock(Arc::clone(&clock))
        .build();
    (clock, limiter)
}

#[test]
fn test_recovery_load_rises_before_anything_is_refused() {
    let (clock, limiter) = limiter(Overflow::FailClosed);
    let empty = limiter.recovery_load();
    assert_eq!(empty.debts_held, 0);
    assert_eq!(empty.debt_capacity, 128); // two records per key of capacity
    assert!(empty.overflow_cells > 0);

    // Every key drains its quota and is evicted owing it: the gauge climbs
    // while nothing has spilled or been refused yet.
    for key in 0..150u64 {
        let _ = limiter.check(key);
    }
    let filling = limiter.recovery_load();
    assert!(
        filling.debts_held > 64 && filling.debts_held <= 128,
        "{filling:?}"
    );
    assert!(filling.utilization() > 0.5 && filling.fullest_shard > 0.5);
    let metrics = limiter.metrics();
    assert_eq!((metrics.debts_overflowed, metrics.keys_refused), (0, 0));

    // Far past the table: debts spill, then newcomers are refused.
    for key in 150..5_000u64 {
        let _ = limiter.check(key);
    }
    let full = limiter.recovery_load();
    assert!(full.utilization() > 0.9, "{full:?}");
    assert!(full.overflow_cells_live > 0);
    let metrics = limiter.metrics();
    assert!(metrics.debts_overflowed > 0 && metrics.keys_refused > 0);

    // A minute later every debt has expired: the gauge is back to empty.
    clock.advance(Duration::from_secs(61));
    let drained = limiter.recovery_load();
    assert_eq!((drained.debts_held, drained.overflow_cells_live), (0, 0));
}

#[test]
fn test_recovery_load_under_forget_has_no_sketch() {
    let (_clock, limiter) = limiter(Overflow::Forget);
    for key in 0..5_000u64 {
        let _ = limiter.check(key);
    }
    let load = limiter.recovery_load();
    assert_eq!((load.overflow_cells, load.overflow_cells_live), (0, 0));
    assert!(load.utilization() > 0.9);
    let metrics = limiter.metrics();
    assert!(metrics.debts_forgotten > 0);
    assert_eq!((metrics.debts_overflowed, metrics.keys_refused), (0, 0));
}

#[test]
fn test_an_uncapped_store_reports_no_capacity() {
    let limiter = RateLimiter::per_minute(1).with_eviction(Eviction::unbounded());
    let _ = limiter.check("a");
    let load = limiter.recovery_load();
    assert_eq!(load, rate_net::RecoveryLoad::default());
    assert!(load.utilization().abs() < f64::EPSILON);
}
