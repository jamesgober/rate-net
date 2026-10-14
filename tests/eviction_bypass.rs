//! Eviction must never reset a limit (RN-005).
//!
//! A key that has spent its allowance and is then evicted — because a flood of
//! other keys filled the store — must not come back with a fresh allowance.
//! In `1.1.0` it did: capacity eviction dropped the key's state, and its next
//! check started full. That let anyone reset a target's limit (login attempts
//! per username, say) by flooding the limiter with throwaway keys.
//!
//! Every test here fails against `1.1.0` and passes now. They drive the public
//! API only, with a frozen `ManualClock`, so nothing refills by itself.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Algorithm, Eviction, Quota, RateLimiter};

fn limiter(algorithm: Algorithm, cap: usize) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .quota(5, Duration::from_secs(60))
        .shards(1)
        .eviction(Eviction::capacity(cap))
        .clock(Arc::clone(&clock))
        .build();
    (clock, limiter)
}

fn algorithms() -> Vec<Algorithm> {
    #[cfg(feature = "algorithms")]
    {
        vec![
            Algorithm::TokenBucket,
            Algorithm::LeakyBucket,
            Algorithm::FixedWindow,
            Algorithm::SlidingWindowLog,
            Algorithm::SlidingWindowCounter,
        ]
    }
    #[cfg(not(feature = "algorithms"))]
    {
        vec![Algorithm::TokenBucket]
    }
}

#[test]
fn test_drained_key_evicted_by_a_flood_stays_drained() {
    for algorithm in algorithms() {
        let (_clock, limiter) = limiter(algorithm, 1_000);
        for _ in 0..5 {
            assert!(limiter.check("victim").is_allow());
        }
        assert!(limiter.check("victim").is_deny());

        // An attacker floods the store with throwaway keys, never touching the
        // victim, until it has been evicted.
        for key in 0..10_000u64 {
            let _ = limiter.check(key);
        }
        assert!(limiter.tracked_keys() <= 1_000);

        assert!(
            limiter.check("victim").is_deny(),
            "{algorithm:?}: the flood reset the victim's limit"
        );
        // And the headers say so too, before the key is checked again.
        assert_eq!(limiter.peek("victim").remaining(), 0, "{algorithm:?}");
    }
}

#[test]
fn test_partly_spent_key_resumes_with_no_more_than_it_had() {
    for algorithm in algorithms() {
        // One evicted key in a one-key store: its debt is recorded exactly.
        let (_clock, limiter) = limiter(algorithm, 1);
        assert!(limiter.check_n("victim", 3).is_allow());
        assert!(limiter.check("other").is_allow()); // evicts "victim"
        // Two units were left; three would mean the eviction gave some back.
        assert!(limiter.check_n("victim", 3).is_deny(), "{algorithm:?}");
        assert!(limiter.check_n("victim", 2).is_allow(), "{algorithm:?}");
    }
}

#[test]
fn test_overloaded_store_fails_closed_never_open() {
    for algorithm in algorithms() {
        // Thousands of unrecovered evictions overload a 64-key store's
        // recovery table: debts spill into the shared overflow sketch, and
        // keys may come back *more* restricted than they left — never less.
        let (_clock, limiter) = limiter(algorithm, 64);
        assert!(limiter.check_n("victim", 3).is_allow());
        for key in 0..5_000u64 {
            let _ = limiter.check(key);
        }
        assert!(limiter.check_n("victim", 3).is_deny(), "{algorithm:?}");
    }
}

#[test]
fn test_evicted_key_recovers_on_schedule() {
    // Remembering a debt must not hold a key back past its recovery: a
    // token-bucket key drained, evicted, and back after a full refill is whole.
    let (clock, limiter) = limiter(Algorithm::TokenBucket, 64);
    assert!(limiter.check_n("victim", 5).is_allow());
    for key in 0..5_000u64 {
        let _ = limiter.check(key);
    }
    clock.advance(Duration::from_secs(60));
    assert!(limiter.check_n("victim", 5).is_allow());
}

#[test]
fn test_idle_expiry_with_a_short_ttl_never_resets_a_limit() {
    // per_minute(5) with a one-second idle TTL: the store raises the TTL to the
    // recovery time, and a key expired anyway (a per-key override with a
    // longer recovery) has its debt remembered.
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .with_quota_for_test()
        .shards(1)
        .eviction(Eviction::new(1_000, Duration::from_secs(1)))
        .clock(Arc::clone(&clock))
        .build();
    for _ in 0..5 {
        assert!(limiter.check("slow:victim").is_allow());
    }
    clock.advance(Duration::from_secs(5));
    // New keys drive the incremental idle sweep past the victim.
    for key in 0..100u64 {
        let _ = limiter.check(key);
    }
    assert!(limiter.check("slow:victim").is_deny());
}

/// A builder with a 5/s default quota and a 5/hour override for `slow:` keys,
/// whose recovery outlasts the idle TTL the default quota allows.
trait QuotaForTest {
    fn with_quota_for_test(self) -> Self;
}

impl<C: clock_lib::Clock + Clone> QuotaForTest for rate_net::Builder<C> {
    fn with_quota_for_test(self) -> Self {
        self.per_second(5).quota_for(|key| {
            key.as_bytes()
                .starts_with(b"slow:")
                .then(|| Quota::rate(5, Duration::from_secs(3_600)).unwrap_or(Quota::per_minute(5)))
        })
    }
}
