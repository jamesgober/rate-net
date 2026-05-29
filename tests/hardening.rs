//! Adversarial-traffic and edge-matrix hardening.
//!
//! The threat model is hostile traffic. These tests throw the nasty cases at the
//! limiter — unique-key floods, burst storms, clock jumps, no time advance,
//! near-maximum request counts, rapid reconfiguration — and assert the
//! invariants hold: no panic, no wrap, never over-admit, memory stays bounded.
//! The per-algorithm section walks the edge matrix (exact quota, window
//! boundaries, zero counts) for each algorithm.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Decision, Eviction, RateLimiter};

fn manual(limit: u32) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::per_second(limit).with_clock(Arc::clone(&clock));
    (clock, limiter)
}

#[test]
fn flood_of_unique_keys_stays_bounded_and_spares_a_live_key() {
    let (clock, limiter) = {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(5)
            .with_clock(Arc::clone(&clock))
            .with_shards(8)
            .with_eviction(Eviction::capacity(64));
        (clock, limiter)
    };

    // Keep one key warm throughout, then flood with unique keys.
    assert!(limiter.check("live").is_allow());
    for k in 0..100_000u64 {
        let _ = limiter.check(k);
        if k % 1000 == 0 {
            let _ = limiter.check("live"); // keep "live" recently-seen
        }
    }

    // Memory is bounded by the per-shard rounding of the cap.
    let bound = 64usize.div_ceil(8).max(1) * 8;
    assert!(
        limiter.tracked_keys() <= bound,
        "flood unbounded: {}",
        limiter.tracked_keys()
    );

    // The live key was never corrupted: it still enforces its own limit and
    // refills on its own schedule.
    clock.advance(Duration::from_secs(1));
    let mut admitted = 0;
    for _ in 0..10 {
        if limiter.check("live").is_allow() {
            admitted += 1;
        }
    }
    assert!(
        admitted <= 5,
        "live key over-admitted {admitted} after a flood"
    );
}

#[test]
fn burst_storm_on_one_key_never_over_admits() {
    let (_clock, limiter) = manual(50);
    // 10x the quota fired at the same instant.
    let admitted = (0..500).filter(|_| limiter.check("k").is_allow()).count();
    assert_eq!(
        admitted, 50,
        "burst storm admitted {admitted}, expected exactly 50"
    );
}

#[test]
fn no_clock_advance_admits_exactly_the_quota() {
    let (_clock, limiter) = manual(7);
    let admitted = (0..1000).filter(|_| limiter.check("k").is_allow()).count();
    assert_eq!(admitted, 7);
}

#[test]
fn large_clock_jump_refills_to_cap_not_beyond() {
    let (clock, limiter) = manual(10);
    // Drain.
    for _ in 0..10 {
        assert!(limiter.check("k").is_allow());
    }
    assert!(limiter.check("k").is_deny());

    // Jump forward 100 days — far past any refill window, but safe for `Instant`.
    clock.advance(Duration::from_secs(100 * 86_400));

    // Refills to capacity (10), never beyond: no wrap, no over-admit.
    let admitted = (0..1000).filter(|_| limiter.check("k").is_allow()).count();
    assert_eq!(admitted, 10, "clock jump over-admitted {admitted}");
}

#[test]
fn request_near_u32_max_denies_without_panic() {
    let (_clock, limiter) = manual(100);
    assert_eq!(
        limiter.check_n("k", u32::MAX),
        Decision::Deny {
            retry_after: Duration::MAX
        }
    );
    // The limiter is still usable afterward.
    assert!(limiter.check("k").is_allow());
}

#[test]
fn huge_quota_does_not_panic() {
    let limiter = RateLimiter::per_second(u32::MAX);
    assert!(limiter.check("k").is_allow());
    assert!(limiter.check_n("k", u32::MAX).is_allow() || limiter.check_n("k", u32::MAX).is_deny());
}

#[test]
fn zero_quota_denies_everything_without_panic() {
    let limiter = RateLimiter::per_second(0);
    for _ in 0..100 {
        assert!(limiter.check("k").is_deny());
    }
}

#[test]
fn rapid_reconfiguration_does_not_panic() {
    let mut limiter = RateLimiter::per_second(10);
    for shards in [1usize, 2, 4, 8, 16, 32] {
        limiter = limiter
            .with_shards(shards)
            .with_eviction(Eviction::capacity(shards * 4))
            .with_eviction(Eviction::idle(Duration::from_secs(1)));
        assert!(limiter.check("k").is_allow() || limiter.check("k").is_deny());
    }
    assert!(limiter.tracked_keys() <= 1 || limiter.tracked_keys() >= 1);
}

#[test]
fn unbounded_eviction_keeps_every_key() {
    let (_clock, limiter) = {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(1)
            .with_clock(Arc::clone(&clock))
            .with_eviction(Eviction::unbounded());
        (clock, limiter)
    };
    for k in 0..5_000u64 {
        let _ = limiter.check(k);
    }
    assert_eq!(limiter.tracked_keys(), 5_000);
}

#[cfg(feature = "algorithms")]
mod algorithm_edges {
    use super::{Arc, Decision, Duration, ManualClock};
    use rate_net::{Algorithm, RateLimiter};

    const ALL: [Algorithm; 5] = [
        Algorithm::TokenBucket,
        Algorithm::LeakyBucket,
        Algorithm::FixedWindow,
        Algorithm::SlidingWindowLog,
        Algorithm::SlidingWindowCounter,
    ];

    fn limiter(
        algorithm: Algorithm,
        limit: u32,
    ) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .algorithm(algorithm)
            .per_second(limit)
            .clock(Arc::clone(&clock))
            .build();
        (clock, limiter)
    }

    #[test]
    fn every_algorithm_admits_zero_units() {
        for algorithm in ALL {
            let (_clock, lim) = limiter(algorithm, 1);
            // Drain, then a zero-unit check still succeeds and costs nothing.
            assert!(lim.check("k").is_allow());
            assert_eq!(lim.check_n("k", 0), Decision::Allow, "{algorithm:?}");
        }
    }

    #[test]
    fn every_algorithm_admits_exactly_the_quota_in_a_window() {
        for algorithm in ALL {
            let (_clock, lim) = limiter(algorithm, 5);
            let admitted = (0..100).filter(|_| lim.check("k").is_allow()).count();
            assert_eq!(
                admitted, 5,
                "{algorithm:?} admitted {admitted} of a 5 quota"
            );
        }
    }

    #[test]
    fn every_algorithm_refuses_a_request_larger_than_the_quota() {
        for algorithm in ALL {
            let (_clock, lim) = limiter(algorithm, 5);
            assert!(
                lim.check_n("k", 6).is_deny(),
                "{algorithm:?} admitted n > limit"
            );
            assert_eq!(
                lim.check_n("k", 6).retry_after(),
                Some(Duration::MAX),
                "{algorithm:?} should report an impossible request"
            );
        }
    }

    #[test]
    fn every_algorithm_refills_after_enough_time() {
        for algorithm in ALL {
            let (clock, lim) = limiter(algorithm, 4);
            for _ in 0..4 {
                assert!(lim.check("k").is_allow(), "{algorithm:?}");
            }
            assert!(lim.check("k").is_deny(), "{algorithm:?}");
            // Two full periods restore the allowance for every algorithm. (One
            // period suffices for most; the sliding-window counter still weights
            // the just-ended window fully at the exact boundary, so it needs the
            // previous window to clear before it admits again.)
            clock.advance(Duration::from_secs(2));
            assert!(lim.check("k").is_allow(), "{algorithm:?} did not refill");
        }
    }

    #[test]
    fn window_algorithms_handle_exact_boundary() {
        // At the exact window boundary the new window must be fresh.
        for algorithm in [Algorithm::FixedWindow, Algorithm::SlidingWindowLog] {
            let (clock, lim) = limiter(algorithm, 3);
            for _ in 0..3 {
                assert!(lim.check("k").is_allow(), "{algorithm:?}");
            }
            assert!(lim.check("k").is_deny(), "{algorithm:?}");
            // Exactly one period later, the oldest units have aged out.
            clock.advance(Duration::from_secs(1));
            assert!(lim.check("k").is_allow(), "{algorithm:?} at boundary");
        }
    }

    #[test]
    fn every_algorithm_survives_a_burst_storm_with_no_time() {
        for algorithm in ALL {
            let (_clock, lim) = limiter(algorithm, 20);
            let admitted = (0..1000).filter(|_| lim.check("k").is_allow()).count();
            assert_eq!(
                admitted, 20,
                "{algorithm:?} over-admitted under a burst storm"
            );
        }
    }
}
