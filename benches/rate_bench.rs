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
//!   worst case for per-key contention.
//! - `check/eviction_sweep` — first-seeing a new key while a small shard is at
//!   capacity, so every call inserts and evicts.
//! - `check/insert_at_default_cap` — the same at the realistic scale: the
//!   default 1,048,576-key cap over the default shard count, full. Before the
//!   CLOCK eviction this cost ~329 µs per insert.
//! - `check/direct` — a [`DirectLimiter`]: no key, no map, no lock.
//! - `peek/existing_key` — reading an allowance for `RateLimit-*` headers.
//!
//! Run with `cargo bench --bench rate_bench`.

use std::hint::black_box;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rate_net::{CoarseClock, DEFAULT_MAX_KEYS, DirectLimiter, Eviction, RateLimiter};

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
    const THREADS: u64 = 4;
    let limiter = Arc::new(permissive());
    let _ = limiter.check("hot"); // warm

    let _ = c.bench_function("check/contended_single_key_4t", |b| {
        b.iter_custom(|iters| {
            let per_thread = iters / THREADS;
            let start = Instant::now();
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let limiter = Arc::clone(&limiter);
                    thread::spawn(move || {
                        for _ in 0..per_thread {
                            let _ = limiter.check(black_box("hot"));
                        }
                    })
                })
                .collect();
            for handle in handles {
                let _ = handle.join();
            }
            start.elapsed()
        });
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

fn bench_peek(c: &mut Criterion) {
    let limiter = permissive();
    let _ = limiter.check("user:42");

    let _ = c.bench_function("peek/existing_key", |b| {
        b.iter(|| limiter.peek(black_box("user:42")));
    });
}

criterion_group!(
    benches,
    bench_single_key,
    bench_single_key_coarse_clock,
    bench_many_keys,
    bench_contended_single_key,
    bench_eviction_sweep,
    bench_insert_at_default_cap,
    bench_direct,
    bench_peek
);
criterion_main!(benches);
