//! Real-clock contention: many threads hammer one key while real time moves.
//!
//! Every other concurrency test freezes the clock, and a frozen clock can never
//! expose a stale-timestamp race: when time does not move, no thread can carry
//! an older `now` than the state it lands on. These tests use the OS clock, so
//! threads routinely read the time, get preempted, and arrive after another
//! thread has already committed a later timestamp — exactly the interleaving
//! that made the token bucket admit 20x to 600x its quota (RN-COR-01) and let
//! the fixed window roll back to an older window (RN-COR-03).
//!
//! The invariant is an upper bound on admissions over the run, derived from the
//! algorithm's own contract and the measured elapsed time — never an exact
//! count, which real scheduling makes nondeterministic. Each run is short and
//! every algorithm gets its own test so they run in parallel.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rate_net::{Algorithm, RateLimiter};

/// Window length for every run: short, so a run spans several windows.
const PERIOD: Duration = Duration::from_millis(100);
/// Units admitted per period (1000 per second).
const LIMIT: u32 = 100;
/// How long each configuration is hammered.
const RUN: Duration = Duration::from_millis(300);
/// Thread counts exercised for each algorithm.
const THREADS: [usize; 3] = [2, 4, 8];

/// Hammers one key from `threads` threads for [`RUN`], returning the units
/// admitted and the time elapsed since just before the limiter was built.
fn hammer(algorithm: Algorithm, threads: usize) -> (u64, Duration) {
    let start = Instant::now();
    let limiter = Arc::new(
        RateLimiter::builder()
            .algorithm(algorithm)
            .quota(LIMIT, PERIOD)
            .try_build()
            .expect("a valid quota"),
    );
    let admitted = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let limiter = Arc::clone(&limiter);
            let admitted = Arc::clone(&admitted);
            thread::spawn(move || {
                let mut local = 0u64;
                while start.elapsed() < RUN {
                    if limiter.check("hot").is_allow() {
                        local += 1;
                    }
                }
                let _total = admitted.fetch_add(local, Ordering::Relaxed);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }
    (admitted.load(Ordering::Relaxed), start.elapsed())
}

/// Whole periods elapsed (rounded down).
#[cfg(feature = "algorithms")]
fn periods(elapsed: Duration) -> u64 {
    (elapsed.as_nanos() / PERIOD.as_nanos()) as u64
}

/// The bucket bound: the initial burst plus the rate times the elapsed time,
/// plus one unit of rounding slack.
fn bucket_ceiling(elapsed: Duration) -> u64 {
    let refilled = u128::from(LIMIT) * elapsed.as_nanos() / PERIOD.as_nanos();
    u64::from(LIMIT) + refilled as u64 + 1
}

fn assert_bounded(algorithm: Algorithm, ceiling: impl Fn(Duration) -> u64) {
    for threads in THREADS {
        let (admitted, elapsed) = hammer(algorithm, threads);
        let bound = ceiling(elapsed);
        assert!(
            admitted <= bound,
            "{algorithm:?} with {threads} threads admitted {admitted} in {elapsed:?}; \
             the quota allows at most {bound}"
        );
        // The run did real work: the key was admitted well past one burst.
        assert!(
            admitted >= u64::from(LIMIT),
            "{algorithm:?} with {threads} threads admitted only {admitted}"
        );
    }
}

#[test]
fn test_token_bucket_real_clock_contention_never_over_admits() {
    assert_bounded(Algorithm::TokenBucket, bucket_ceiling);
}

#[cfg(feature = "algorithms")]
#[test]
fn test_leaky_bucket_real_clock_contention_never_over_admits() {
    assert_bounded(Algorithm::LeakyBucket, bucket_ceiling);
}

#[cfg(feature = "algorithms")]
#[test]
fn test_fixed_window_real_clock_contention_never_over_admits() {
    // At most `limit` in each aligned window the run touches.
    assert_bounded(Algorithm::FixedWindow, |elapsed| {
        u64::from(LIMIT) * (periods(elapsed) + 2)
    });
}

#[cfg(feature = "algorithms")]
#[test]
fn test_sliding_log_real_clock_contention_never_over_admits() {
    // At most `limit` in any rolling window, so at most `limit` per
    // consecutive period of the run.
    assert_bounded(Algorithm::SlidingWindowLog, |elapsed| {
        u64::from(LIMIT) * (periods(elapsed) + 1)
    });
}

#[cfg(feature = "algorithms")]
#[test]
fn test_sliding_counter_real_clock_contention_never_over_admits() {
    // The approximation's worst case is `2 * limit` in a rolling window.
    assert_bounded(Algorithm::SlidingWindowCounter, |elapsed| {
        2 * u64::from(LIMIT) * (periods(elapsed) + 1)
    });
}
