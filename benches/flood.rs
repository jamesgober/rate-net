//! Tail latency of legitimate traffic under a unique-key flood.
//!
//! A limiter is filled to its cap, then one thread checks a fixed set of
//! legitimate, already-tracked keys in a loop, timing every check, while
//! `FLOOD_THREADS` other threads insert a never-ending stream of unique keys —
//! the attack that makes every insert evict. The report is the legitimate
//! checks' latency distribution (p50, p99, p99.9, max) with and without the
//! flood, so the question "does a flood stall real users?" has a number.
//!
//! In `1.0.0` each flood insert scanned its whole shard under the shard's write
//! lock, and a legitimate check landing in that shard waited for the scan.
//! Eviction now does bounded work per insert, so the distribution should barely
//! move.
//!
//! A custom harness (not Criterion): the measurement is a distribution under a
//! concurrent load, not a mean. Each timed check includes one `Instant::now()`
//! pair (tens of nanoseconds), identically in both scenarios.
//!
//! Run with `cargo bench --bench flood`. `FLOOD_SECS` (default 3) sets each
//! scenario's duration.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rate_net::{DEFAULT_MAX_KEYS, RateLimiter};

/// Threads inserting unique keys during the flood scenario.
const FLOOD_THREADS: u64 = 3;

/// Legitimate keys the measuring thread cycles through.
const LEGIT_KEYS: u64 = 1_000;

/// Keys used by the measuring thread start here, far from the flood's range.
const LEGIT_BASE: u64 = 1 << 62;

fn main() {
    let secs = std::env::var("FLOOD_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    let duration = Duration::from_secs(secs);

    // Printing the report is the purpose of this harness.
    #[allow(clippy::print_stdout)]
    {
        println!(
            "rate-net {} — legitimate check latency, default limiter at its cap",
            rate_net::VERSION
        );
        println!(
            "{:<22} {:>10} {:>10} {:>10} {:>10} {:>12} {:>14}",
            "scenario", "p50", "p99", "p99.9", "max", "legit checks", "flood inserts"
        );
        for flood in [false, true] {
            let report = run(flood, duration);
            println!(
                "{:<22} {:>10} {:>10} {:>10} {:>10} {:>12} {:>14}",
                if flood {
                    "3-thread unique flood"
                } else {
                    "no flood"
                },
                fmt(report.p50),
                fmt(report.p99),
                fmt(report.p999),
                fmt(report.max),
                report.checks,
                report.inserts,
            );
        }
    }
}

struct Report {
    p50: Duration,
    p99: Duration,
    p999: Duration,
    max: Duration,
    checks: usize,
    inserts: u64,
}

fn run(flood: bool, duration: Duration) -> Report {
    // A rate no legitimate key will exhaust, so every timed check is admitted.
    let limiter = Arc::new(RateLimiter::per_second(u32::MAX));
    // Fill to the cap (twice over, so the store is in its evicting steady
    // state), then make the legitimate keys tracked and recently used.
    for key in 0..(DEFAULT_MAX_KEYS as u64 * 2) {
        let _ = limiter.check(key);
    }
    for key in 0..LEGIT_KEYS {
        let _ = limiter.check(LEGIT_BASE + key);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let inserted = Arc::new(AtomicU64::new(0));
    let flooders: Vec<_> = (0..if flood { FLOOD_THREADS } else { 0 })
        .map(|thread_index| {
            let limiter = Arc::clone(&limiter);
            let stop = Arc::clone(&stop);
            let inserted = Arc::clone(&inserted);
            thread::spawn(move || {
                // Disjoint, never-repeating key ranges per thread.
                let mut key = (thread_index + 1) << 40;
                let mut count = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _ = limiter.check(black_box(key));
                    key += 1;
                    count += 1;
                }
                let _ = inserted.fetch_add(count, Ordering::Relaxed);
            })
        })
        .collect();

    let mut samples = Vec::with_capacity(8_000_000);
    let end = Instant::now() + duration;
    let mut key = 0;
    while Instant::now() < end {
        let start = Instant::now();
        let decision = limiter.check(black_box(LEGIT_BASE + key));
        samples.push(start.elapsed());
        let _ = black_box(decision);
        key = (key + 1) % LEGIT_KEYS;
    }
    stop.store(true, Ordering::Relaxed);
    for flooder in flooders {
        let _ = flooder.join();
    }

    samples.sort_unstable();
    let at = |q: f64| {
        // Index of quantile `q`; the sample count is far below 2^52.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let index = ((samples.len() as f64 - 1.0) * q).round() as usize;
        samples.get(index).copied().unwrap_or_default()
    };
    Report {
        p50: at(0.50),
        p99: at(0.99),
        p999: at(0.999),
        max: samples.last().copied().unwrap_or_default(),
        checks: samples.len(),
        inserts: inserted.load(Ordering::Relaxed),
    }
}

fn fmt(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 10_000 {
        format!("{nanos} ns")
    } else if nanos < 10_000_000 {
        format!("{:.1} µs", duration.as_secs_f64() * 1e6)
    } else {
        format!("{:.2} ms", duration.as_secs_f64() * 1e3)
    }
}
