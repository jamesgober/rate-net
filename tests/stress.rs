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
