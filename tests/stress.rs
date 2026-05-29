//! Multi-threaded stress: under real contention across many keys, a key is
//! never over-admitted and never under-admitted.
//!
//! With the clock frozen (a `ManualClock` that never advances) no refill
//! happens, so each key has exactly its quota of tokens for the whole run. Many
//! threads then hammer every key far more times than its quota. The invariant:
//! the total admitted for each key equals its quota exactly — never more (no
//! over-admit) and never fewer (no lost decrement). Unrelated keys live in
//! different shards and make progress concurrently; the run completing is itself
//! evidence the shards do not serialise.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

use clock_lib::ManualClock;
use rate_net::{Eviction, RateLimiter};

#[test]
fn stress_many_keys_never_over_or_under_admit() {
    const KEYS: u64 = 64;
    const LIMIT: u32 = 100;
    const THREADS: usize = 8;
    const PASSES: u32 = 40; // THREADS * PASSES = 320 attempts per key >> LIMIT

    let clock = Arc::new(ManualClock::new());
    let limiter = Arc::new(
        RateLimiter::per_second(LIMIT)
            .with_clock(Arc::clone(&clock))
            // Keep every key live for the whole run; this test is about
            // concurrency, not eviction.
            .with_eviction(Eviction::unbounded()),
    );

    let admitted: Arc<Vec<AtomicU32>> = Arc::new((0..KEYS).map(|_| AtomicU32::new(0)).collect());

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let limiter = Arc::clone(&limiter);
        let admitted = Arc::clone(&admitted);
        handles.push(thread::spawn(move || {
            for _ in 0..PASSES {
                for key in 0..KEYS {
                    if limiter.check(key).is_allow() {
                        let _ = admitted[key as usize].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }

    for key in 0..KEYS {
        let count = admitted[key as usize].load(Ordering::Relaxed);
        assert_eq!(
            count, LIMIT,
            "key {key} admitted {count}, expected exactly {LIMIT} (frozen clock)"
        );
    }
}

/// Every algorithm — token bucket (lock-free), leaky bucket (lock-free CAS),
/// fixed window (lock-free packed atomic), and the sliding-window log/counter
/// (per-key `Mutex`) — must hold its per-key limit under real concurrency on a
/// single hot key. With the clock frozen, exactly the quota is admitted: never
/// more (no over-admit, no torn updates) and never fewer (no lost decrements,
/// no deadlock).
#[cfg(feature = "algorithms")]
#[test]
fn stress_every_algorithm_admits_exactly_the_quota_under_concurrency() {
    use rate_net::Algorithm;

    const LIMIT: u32 = 200;
    const THREADS: usize = 8;
    const PASSES: u32 = 50; // THREADS * PASSES = 400 attempts > LIMIT

    for algorithm in [
        Algorithm::TokenBucket,
        Algorithm::LeakyBucket,
        Algorithm::FixedWindow,
        Algorithm::SlidingWindowLog,
        Algorithm::SlidingWindowCounter,
    ] {
        let clock = Arc::new(ManualClock::new());
        let limiter = Arc::new(
            RateLimiter::builder()
                .algorithm(algorithm)
                .per_second(LIMIT)
                .clock(Arc::clone(&clock))
                .build(),
        );
        let admitted = Arc::new(AtomicU32::new(0));

        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let limiter = Arc::clone(&limiter);
            let admitted = Arc::clone(&admitted);
            handles.push(thread::spawn(move || {
                for _ in 0..PASSES {
                    if limiter.check("hot").is_allow() {
                        let _ = admitted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }

        let total = admitted.load(Ordering::Relaxed);
        assert_eq!(
            total, LIMIT,
            "{algorithm:?} admitted {total} under contention, expected exactly {LIMIT}"
        );
    }
}

/// The public surface promises the limiter is shared across threads. Lock these
/// guarantees in at the *type* level so any regression surfaces at compile time
/// rather than as a runtime test failure.
#[test]
fn public_types_are_send_sync_and_static() {
    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    assert_send_sync_static::<rate_net::RateLimiter>();
    assert_send_sync_static::<rate_net::Decision>();
    assert_send_sync_static::<rate_net::Quota>();
    assert_send_sync_static::<rate_net::Eviction>();
    assert_send_sync_static::<rate_net::Algorithm>();
    assert_send_sync_static::<rate_net::RateLimiterError>();
    assert_send_sync_static::<rate_net::Key>();

    #[cfg(feature = "async")]
    assert_send_sync_static::<rate_net::AsyncLimiter>();
}
