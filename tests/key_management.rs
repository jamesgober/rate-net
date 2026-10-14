//! Introspection and key management across every algorithm: `peek` reads an
//! allowance without consuming it, and `reset` / `remove` / `clear` /
//! `retain_recent` manage tracked keys without ever loosening a limit.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Algorithm, Eviction, RateLimiter};

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

fn limiter(algorithm: Algorithm, limit: u32) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .per_second(limit)
        .eviction(Eviction::unbounded())
        .clock(Arc::clone(&clock))
        .try_build()
        .expect("a valid configuration");
    (clock, limiter)
}

#[test]
fn test_peek_tracks_spending_without_consuming() {
    for algorithm in ALL {
        let (_clock, lim) = limiter(algorithm, 5);
        let fresh = lim.peek("k");
        assert_eq!((fresh.limit(), fresh.remaining()), (5, 5), "{algorithm:?}");
        assert_eq!(fresh.reset_after(), Duration::ZERO, "{algorithm:?}");
        assert_eq!(lim.tracked_keys(), 0, "{algorithm:?}: peek created state");

        assert!(lim.check_n("k", 2).is_allow(), "{algorithm:?}");
        for _ in 0..10 {
            // Peeking repeatedly never spends anything.
            assert_eq!(lim.peek("k").remaining(), 3, "{algorithm:?}");
        }
        assert!(
            lim.peek("k").reset_after() > Duration::ZERO,
            "{algorithm:?}"
        );
        assert!(lim.check_n("k", 3).is_allow(), "{algorithm:?}");
        let spent = lim.peek("k");
        assert!(spent.is_exhausted(), "{algorithm:?}");
        assert!(lim.check("k").is_deny(), "{algorithm:?}");
    }
}

#[test]
fn test_peek_agrees_with_check_after_time_passes() {
    for algorithm in ALL {
        let (clock, lim) = limiter(algorithm, 4);
        assert!(lim.check_n("k", 4).is_allow(), "{algorithm:?}");
        // Two full periods restore every algorithm (the sliding counter needs
        // the previous window to fade out too).
        clock.advance(Duration::from_secs(2));
        let snapshot = lim.peek("k");
        assert_eq!(snapshot.remaining(), 4, "{algorithm:?}");
        assert!(
            lim.check_n("k", snapshot.remaining()).is_allow(),
            "{algorithm:?}"
        );
    }
}

#[test]
fn test_reset_restores_and_remove_forgets() {
    for algorithm in ALL {
        let (_clock, lim) = limiter(algorithm, 3);
        assert!(lim.check_n("k", 3).is_allow(), "{algorithm:?}");
        assert!(lim.check("k").is_deny(), "{algorithm:?}");
        assert!(lim.reset("k"), "{algorithm:?}");
        assert_eq!(lim.peek("k").remaining(), 3, "{algorithm:?}");
        assert!(lim.check_n("k", 3).is_allow(), "{algorithm:?}");
        assert_eq!(lim.tracked_keys(), 1, "{algorithm:?}");

        assert!(lim.remove("k"), "{algorithm:?}");
        assert_eq!(lim.tracked_keys(), 0, "{algorithm:?}");
        assert!(!lim.remove("k"), "{algorithm:?}");
        assert!(!lim.reset("k"), "{algorithm:?}");
    }
}

#[test]
fn test_retain_recent_never_changes_a_decision() {
    for algorithm in ALL {
        let (clock, lim) = limiter(algorithm, 5);
        for user in 0..50u64 {
            // Even users spend everything, odd users nothing.
            let n = if user % 2 == 0 { 5 } else { 0 };
            assert!(lim.check_n(user, n).is_allow(), "{algorithm:?}");
        }
        assert_eq!(lim.retain_recent(), 25, "{algorithm:?}");
        // The drained keys are still tracked and still drained.
        for user in (0..50u64).step_by(2) {
            assert!(lim.check(user).is_deny(), "{algorithm:?} user {user}");
        }
        clock.advance(Duration::from_secs(2));
        assert_eq!(lim.retain_recent(), 25, "{algorithm:?}");
        assert_eq!(lim.tracked_keys(), 0, "{algorithm:?}");
        lim.shrink_to_fit();
    }
}

#[test]
fn test_clear_forgets_every_key() {
    for algorithm in ALL {
        let (_clock, lim) = limiter(algorithm, 1);
        for user in 0..200u64 {
            assert!(lim.check(user).is_allow(), "{algorithm:?}");
        }
        lim.clear();
        assert_eq!(lim.tracked_keys(), 0, "{algorithm:?}");
        assert!(lim.check(7u64).is_allow(), "{algorithm:?}");
    }
}

#[test]
fn test_closed_limiter_reports_no_capacity() {
    let lim = RateLimiter::per_second(0);
    let snapshot = lim.peek("k");
    assert_eq!(snapshot.limit(), 0);
    assert!(snapshot.is_exhausted());
    assert_eq!(snapshot.reset_after(), Duration::MAX);
}
