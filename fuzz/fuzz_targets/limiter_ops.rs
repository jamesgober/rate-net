//! Drives a small, heavily evicting limiter with an arbitrary sequence of
//! checks, clock advances, and key-management calls, and checks it against an
//! oracle: for the token bucket, the units admitted for one key in any time
//! interval `[a, b]` never exceed `burst + rate * (b - a)` for that key's own
//! quota — whatever capacity and idle-TTL eviction, debt recording, cuckoo
//! moves, overflow, resets, and removals happened in between (a reset or
//! removal starts a key's accounting over). Keys 200..=254 run under a
//! `quota_for` override far slower than the default, so their recovery
//! outlasts the idle TTL (the RN3-001 case). The oracle is enforced under
//! `Overflow::FailClosed`, and under `Overflow::Forget` while no debt has been
//! dropped. For every algorithm, nothing panics and the store never exceeds
//! its cap.

#![no_main]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arbitrary::Arbitrary;
use clock_lib::ManualClock;
use libfuzzer_sys::fuzz_target;
use rate_net::{Algorithm, Eviction, Overflow, Quota, RateLimiter};

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
    fail_closed: bool,
    ops: Vec<Op>,
}

/// The default quota: 5 per second, burst 4.
const LIMIT: u64 = 5;
const BURST: u64 = 4;
const PERIOD_MS: u64 = 1_000;

/// The override for keys 200..=254: 5 per 10 seconds, burst 5 — a recovery
/// of 10 s, longer than the idle TTL the default quota allows (0.8 s), and
/// short enough for a few clock advances to cross it.
const SLOW_LIMIT: u64 = 5;
const SLOW_BURST: u64 = 5;
const SLOW_PERIOD_MS: u64 = 10_000;

fn is_slow(key: u8) -> bool {
    (200..255).contains(&key)
}

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
    let overflow = if input.fail_closed {
        Overflow::FailClosed
    } else {
        Overflow::Forget
    };
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .algorithm(algorithm)
        .quota(5, Duration::from_secs(1))
        .burst(4)
        .shards(1)
        .eviction(eviction.with_overflow(overflow))
        .quota_for(|key| match key.as_bytes() {
            [255] => Some(Quota::per_second(0)),
            [k] if is_slow(*k) => Quota::rate(5, Duration::from_secs(10)).ok(),
            _ => None,
        })
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
    if overflow == Overflow::Forget && limiter.metrics().debts_forgotten > 0 {
        // Past the exact table, Forget drops debts by design.
        return;
    }
    for (key, events) in &admitted {
        let (limit, burst, period) = if is_slow(*key) {
            (SLOW_LIMIT, SLOW_BURST, SLOW_PERIOD_MS)
        } else {
            (LIMIT, BURST, PERIOD_MS)
        };
        for (i, &(a, _)) in events.iter().enumerate() {
            let mut units = 0;
            for &(b, n) in &events[i..] {
                units += n;
                assert!(
                    units * period <= burst * period + limit * (b - a),
                    "key {key}: {units} units admitted in [{a}ms, {b}ms]"
                );
            }
        }
    }
});
