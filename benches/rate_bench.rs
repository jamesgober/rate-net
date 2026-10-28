//! Criterion benchmark suite for `rate-net`.
//!
//! The paths that matter for a keyed limiter:
//!
//! - `check/single_key` — the steady-state hot path: one existing key, a shard
//!   read lock plus the bucket's atomic accounting.
//! - `check/single_key_coarse_clock` — the same, reading a [`CoarseClock`]
//!   instead of the OS monotonic clock.
//! - `check/many_keys` — distinct keys spread across shards, the realistic
//!   server workload.
//! - `check/contended_single_key_4t` — four threads hammering one key, the
//!   worst case for per-key contention. Reported as **per-check latency** —
//!   each thread's own time per check, averaged over the threads — not as
//!   aggregate throughput.
//! - `check/long_key`, `check/long_key_bytes` — a 36-byte UUID key through
//!   `check` (which builds an owned key, allocating for anything over 22
//!   bytes) and through `check_bytes` (which borrows it).
//! - `check/eviction_sweep` — first-seeing a new key while a small shard is at
//!   capacity, so every call inserts and evicts.
//! - `check/insert_at_default_cap` — the same at the realistic scale: the
//!   default 1,048,576-key cap over the default shard count, full. Before the
//!   CLOCK eviction this cost ~329 µs per insert.
//! - `check/direct` — a [`DirectLimiter`]: no key, no map, no shard lock.
//! - `check/direct_coarse_clock` — the same on a [`CoarseClock`].
//! - `peek/existing_key` — reading an allowance for `RateLimit-*` headers.
//! - `check/blocked_key` — a key whose `quota_for` override admits nothing (a
//!   blocked tenant): denied without ever taking the shard write lock.
//! - `gcra/check_at`: the clock-free `Gcra`, with caller-supplied time: the
//!   leaky-bucket arithmetic and one compare-and-swap, nothing else.
//! - `gcra_params/check_at`: the same arithmetic through a shared
//!   `GcraParams` and a caller-owned word, as a keyed table uses it.
//!
//! Run with `cargo bench --bench rate_bench`.

use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rate_net::{
    CoarseClock, DEFAULT_MAX_KEYS, DirectLimiter, Eviction, Gcra, GcraParams, Quota, RateLimiter,
};

/// A limiter with effectively no rate ceiling, so the benchmark measures the
/// check machinery rather than the deny branch.
fn permissive() -> RateLimiter {
    RateLimiter::per_second(u32::MAX)
}

fn bench_single_key(c: &mut Criterion) {
    let limiter = permissive();
    let _ = limiter.check("user:42"); // warm: insert the key once

    let _ = c.bench_function("check/single_key", |b| {
        b.iter(|| limiter.check(black_box("user:42")));
    });
}

fn bench_single_key_coarse_clock(c: &mut Criterion) {
    let clock = CoarseClock::spawn(Duration::from_millis(1)).expect("spawn the ticker");
    let limiter = permissive().with_clock(clock);
    let _ = limiter.check("user:42");

    let _ = c.bench_function("check/single_key_coarse_clock", |b| {
        b.iter(|| limiter.check(black_box("user:42")));
    });
}

fn bench_many_keys(c: &mut Criterion) {
    let limiter = permissive()
        .with_shards(64)
        .with_eviction(Eviction::unbounded());
    let keys: Vec<u64> = (0..10_000).collect();
    for &key in &keys {
        let _ = limiter.check(key); // warm every key
    }

    let _ = c.bench_function("check/many_keys", |b| {
        let mut i = 0usize;
        b.iter(|| {
            let key = keys[i % keys.len()];
            i = i.wrapping_add(1);
            limiter.check(black_box(key))
        });
    });
}

fn bench_contended_single_key(c: &mut Criterion) {
    const THREADS: u32 = 4;
    let limiter = Arc::new(permissive());
    let _ = limiter.check("hot"); // warm

    let _ = c.bench_function("check/contended_single_key_4t", |b| {
        b.iter_custom(|iters| {
            // Every thread runs `iters` checks, timing only its own loop (after
            // a barrier, so spawning is not measured). Criterion divides the
            // returned time by `iters`, so returning the threads' mean loop
            // time reports the latency of one check under 4-way contention.
            let barrier = Arc::new(Barrier::new(THREADS as usize));
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let limiter = Arc::clone(&limiter);
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        let _leader = barrier.wait();
                        let start = Instant::now();
                        for _ in 0..iters {
                            let _ = limiter.check(black_box("hot"));
                        }
                        start.elapsed()
                    })
                })
                .collect();
            let total: Duration = handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_default())
                .sum();
            total / THREADS
        });
    });
}

fn bench_long_key(c: &mut Criterion) {
    let limiter = permissive();
    let key = "9f6c1d2e-8b7a-4c3d-9e1f-0a2b3c4d5e6f";
    let _ = limiter.check(key);

    let _ = c.bench_function("check/long_key", |b| {
        b.iter(|| limiter.check(black_box(key)));
    });
    let _ = c.bench_function("check/long_key_bytes", |b| {
        b.iter(|| limiter.check_bytes(black_box(key)));
    });
}

fn bench_eviction_sweep(c: &mut Criterion) {
    // One shard capped at 64 keys: every new key inserts and evicts.
    let limiter = permissive()
        .with_shards(1)
        .with_eviction(Eviction::capacity(64));

    let _ = c.bench_function("check/eviction_sweep", |b| {
        let mut key = 0u64;
        b.iter(|| {
            key = key.wrapping_add(1);
            limiter.check(black_box(key))
        });
    });
}

fn bench_insert_at_default_cap(c: &mut Criterion) {
    // The default limiter, filled to its default cap: every further new key
    // evicts one. This is the unique-key-flood steady state.
    let limiter = permissive();
    let mut key = 0u64;
    while key < DEFAULT_MAX_KEYS as u64 * 2 {
        let _ = limiter.check(key);
        key += 1;
    }

    let _ = c.bench_function("check/insert_at_default_cap", |b| {
        b.iter(|| {
            key = key.wrapping_add(1);
            limiter.check(black_box(key))
        });
    });
}

fn bench_direct(c: &mut Criterion) {
    let limiter = DirectLimiter::per_second(u32::MAX);

    let _ = c.bench_function("check/direct", |b| {
        b.iter(|| black_box(&limiter).check());
    });
}

fn bench_direct_coarse_clock(c: &mut Criterion) {
    let clock = CoarseClock::spawn(Duration::from_millis(1)).expect("spawn the ticker");
    let limiter = DirectLimiter::per_second(u32::MAX).with_clock(clock);

    let _ = c.bench_function("check/direct_coarse_clock", |b| {
        b.iter(|| black_box(&limiter).check());
    });
}

fn bench_peek(c: &mut Criterion) {
    let limiter = permissive();
    let _ = limiter.check("user:42");

    let _ = c.bench_function("peek/existing_key", |b| {
        b.iter(|| limiter.peek(black_box("user:42")));
    });
}

fn bench_blocked_key(c: &mut Criterion) {
    let limiter = RateLimiter::builder()
        .per_second(u32::MAX)
        .quota_for(|key| (key.as_bytes() == b"blocked").then(|| Quota::per_second(0)))
        .build();

    let _ = c.bench_function("check/blocked_key", |b| {
        b.iter(|| limiter.check(black_box("blocked")));
    });
}

/// One unit per nanosecond, with time advanced a nanosecond per check, so
/// every check is admitted: the benchmark measures the admit path.
fn gcra_quota() -> Quota {
    Quota::per_second(1_000_000_000).with_burst(1)
}

fn bench_gcra(c: &mut Criterion) {
    let gcra = Gcra::new(gcra_quota());
    let now = Cell::new(0u64);

    let _ = c.bench_function("gcra/check_at", |b| {
        b.iter(|| {
            now.set(now.get() + 1);
            black_box(&gcra).check_at(Duration::from_nanos(now.get()))
        });
    });
}

fn bench_gcra_params(c: &mut Criterion) {
    let params = GcraParams::new(gcra_quota());
    let tat = AtomicU64::new(0);
    let now = Cell::new(0u64);

    let _ = c.bench_function("gcra_params/check_at", |b| {
        b.iter(|| {
            now.set(now.get() + 1);
            black_box(&params).check_at(black_box(&tat), Duration::from_nanos(now.get()))
        });
    });
}

criterion_group!(
    benches,
    bench_single_key,
    bench_single_key_coarse_clock,
    bench_many_keys,
    bench_contended_single_key,
    bench_long_key,
    bench_eviction_sweep,
    bench_insert_at_default_cap,
    bench_direct,
    bench_direct_coarse_clock,
    bench_peek,
    bench_blocked_key,
    bench_gcra,
    bench_gcra_params
);
criterion_main!(benches);
