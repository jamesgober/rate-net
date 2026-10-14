//! Drives a small, heavily evicting limiter with an arbitrary sequence of
//! checks, clock advances, and key-management calls, and checks it against an
//! oracle: for the token bucket, the units admitted for one key in any time
//! interval `[a, b]` never exceed `burst + rate * (b - a)` — whatever eviction,
//! debt recording, merging, resets, and removals happened in between (a reset
//! or removal starts a key's accounting over). For every algorithm, nothing
//! panics and the store never exceeds its cap.

#![no_main]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arbitrary::Arbitrary;
use clock_lib::ManualClock;
use libfuzzer_sys::fuzz_target;
use rate_net::{Algorithm, Eviction, Quota, RateLimiter};

#[derive(Arbitrary, Debug)]
enum Op {
    Check { key: u8, n: u8 },
    Advance { ms: u16 },
    Peek { key: u8 },
    Reset { key: u8 },
    Remove { key: u8 },
    RetainRecent,
}

#[derive(Arbitrary, Debug)]
struct Input {
    algorithm: u8,
    cap: u8,
    idle_ms: Option<u16>,
    ops: Vec<Op>,
}

/// 5 per second, burst 4.
const LIMIT: u64 = 5;
const BURST: u64 = 4;

fuzz_target!(|input: Input| {
    let algorithm = match input.algorithm % 5 {
        0 => Algorithm::TokenBucket,
        1 => Algorithm::LeakyBucket,
        2 => Algorithm::FixedWindow,
        3 => Algorithm::SlidingWindowLog,
        _ => Algorithm::SlidingWindowCounter,
    };
    let cap = usize::from(input.cap % 16) + 1;
    let eviction = match input.idle_ms {
        Some(ms) => Eviction::new(cap, Duration::from_millis(u64::from(ms))),
        None => Eviction::capacity(cap),
    };
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .quota(5, Duration::from_secs(1))
        .burst(4)
        .shards(1)
        .eviction(eviction)
        .quota_for(|key| (key.as_bytes() == [255]).then(|| Quota::per_second(0)))
        .clock(Arc::clone(&clock))
        .build();

    let mut now_ms = 0u64;
    // Per key: the admissions since its accounting last started over.
    let mut admitted: HashMap<u8, Vec<(u64, u64)>> = HashMap::new();
    for op in input.ops.into_iter().take(2_000) {
        match op {
            Op::Check { key, n } => {
                let n = n % 6;
                let decision = limiter.check_n([key], u32::from(n));
                if key == 255 && n > 0 {
                    assert!(decision.is_impossible());
                }
                if decision.is_allow() {
                    admitted
                        .entry(key)
                        .or_default()
                        .push((now_ms, u64::from(n)));
                }
            }
            Op::Advance { ms } => {
                let ms = u64::from(ms % 2_000);
                clock.advance(Duration::from_millis(ms));
                now_ms += ms;
            }
            Op::Peek { key } => {
                let snapshot = limiter.peek([key]);
                assert!(snapshot.remaining() <= snapshot.limit());
            }
            Op::Reset { key } => {
                let _ = limiter.reset([key]);
                let _ = admitted.remove(&key);
            }
            Op::Remove { key } => {
                let _ = limiter.remove([key]);
                let _ = admitted.remove(&key);
            }
            Op::RetainRecent => {
                // Forgets only recovered keys: no decision changes.
                let _ = limiter.retain_recent();
            }
        }
        assert!(limiter.tracked_keys() <= cap);
    }

    if algorithm != Algorithm::TokenBucket {
        return;
    }
    for (key, events) in &admitted {
        for (i, &(a, _)) in events.iter().enumerate() {
            let mut units = 0;
            for &(b, n) in &events[i..] {
                units += n;
                assert!(
                    units * 1_000 <= BURST * 1_000 + LIMIT * (b - a),
                    "key {key}: {units} units admitted in [{a}ms, {b}ms]"
                );
            }
        }
    }
});
