//! No configuration can make the library panic.
//!
//! Every builder knob is driven to its extremes — zero, one, and the type's
//! maximum for the limit, period, burst, shard count, capacity, and idle TTL,
//! under every algorithm — and the resulting limiter is exercised. `try_build`
//! may reject a configuration; `build` must always produce a limiter whose
//! every operation returns normally.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use proptest::prelude::*;
use rate_net::{Algorithm, DirectLimiter, Eviction, Gcra, Quota, RateLimiter};

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

fn limits() -> impl Strategy<Value = u32> {
    prop::sample::select(vec![0, 1, 7, 1_000_000_000, u32::MAX])
}

fn periods() -> impl Strategy<Value = Duration> {
    prop::sample::select(vec![
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::from_millis(1),
        Duration::from_secs(1),
        Duration::from_secs(365 * 86_400),
        Duration::MAX,
    ])
}

fn evictions() -> impl Strategy<Value = Eviction> {
    let caps = prop::sample::select(vec![None, Some(0usize), Some(1), Some(usize::MAX)]);
    let ttls = prop::sample::select(vec![
        None,
        Some(Duration::ZERO),
        Some(Duration::from_nanos(1)),
        Some(Duration::MAX),
    ]);
    (caps, ttls).prop_map(|(cap, ttl)| {
        let base = cap.map_or_else(Eviction::unbounded, Eviction::capacity);
        match ttl {
            Some(ttl) => base.with_idle(ttl),
            None => base,
        }
    })
}

/// Clock advances in whole seconds, up to about three decades.
fn advances() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(
        prop::sample::select(vec![0u64, 1, 60, 86_400, 1_000_000_000]),
        1..4,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    #[test]
    fn prop_no_configuration_panics(
        algorithm in prop::sample::select(ALL.to_vec()),
        limit in limits(),
        period in periods(),
        burst in prop::sample::select(vec![0u32, 1, u32::MAX]),
        shards in prop::sample::select(vec![0usize, 1, 4, usize::MAX]),
        eviction in evictions(),
        steps in advances(),
    ) {
        let builder = || {
            RateLimiter::builder()
                .algorithm(algorithm)
                .quota(limit, period)
                .burst(burst)
                .shards(shards)
                .eviction(eviction)
        };
        // Validation may refuse; it must not panic.
        let _verdict = builder().try_build();
        let _direct = builder().try_build_direct();

        let clock = Arc::new(ManualClock::new());
        let limiter = builder().clock(Arc::clone(&clock)).build();
        let direct = builder().clock(Arc::clone(&clock)).build_direct();
        for seconds in steps {
            clock.advance(Duration::from_secs(seconds));
            for n in [0, 1, u32::MAX] {
                let _ = limiter.check_n("k", n);
                let _ = limiter.check_n(seconds, n);
                let _ = direct.check_n(n);
            }
            let snapshot = limiter.peek("k");
            prop_assert!(snapshot.remaining() <= snapshot.limit());
            let _ = snapshot.headers();
            let _ = limiter.check("k").retry_after_secs();
            let _ = direct.peek();
            let _ = limiter.retain_recent();
        }
        let _ = limiter.reset("k");
        let _ = limiter.remove("k");
        limiter.shrink_to_fit();
        limiter.clear();
        prop_assert!(limiter.shards() >= 1);
    }

    #[test]
    fn prop_gcra_never_panics(
        limit in limits(),
        period in periods(),
        burst in prop::sample::select(vec![0u32, 1, u32::MAX]),
        times in prop::collection::vec(any::<u64>(), 1..8),
    ) {
        let quota = Quota::per_second(limit).with_burst(burst);
        let gcra = Gcra::new(Quota::rate(limit.max(1), period).unwrap_or(quota));
        let _ = Gcra::try_new(quota);
        for nanos in times {
            let now = Duration::from_nanos(nanos);
            let _ = gcra.check_n_at(u32::MAX, now);
            let _ = gcra.check_at(now);
            let _ = gcra.peek_at(now);
        }
        gcra.reset();
    }
}

#[test]
fn test_direct_limiter_extremes_never_panic() {
    for quota in [
        Quota::per_second(0),
        Quota::per_second(u32::MAX).with_burst(u32::MAX),
    ] {
        let limiter = DirectLimiter::with_quota(quota);
        let _ = limiter.check_n(u32::MAX);
        let _ = limiter.peek();
    }
}
