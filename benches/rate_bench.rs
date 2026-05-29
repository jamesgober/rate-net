//! Criterion benchmark harness for `rate-net`.
//!
//! `0.1.0` is the scaffold: there is no `check` path to measure yet, so this
//! suite establishes a baseline for the one step the limiter performs on every
//! request before it ever touches a bucket — hashing the key and folding the
//! digest down to a shard index. Measuring it in isolation gives an honest
//! floor for the per-request work `rate-net` owns (the bucket accounting itself
//! belongs to `better-bucket`). The real single-key, many-key (shard scaling),
//! contended-single-key, and eviction-sweep benchmarks land alongside the
//! sharded core in `0.3.0`, with the full comparative suite in `0.5.0`.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

/// Mirror of the future shard-locate step: hash a key and mask the digest down
/// to a shard index. `shard_mask` is `shard_count - 1` for a power-of-two shard
/// count, so the mask selects the low bits of the hash without a division.
#[inline]
fn locate_shard(hasher: &RandomState, key: &str, shard_mask: u64) -> u64 {
    hasher.hash_one(key) & shard_mask
}

fn bench_locate_shard(c: &mut Criterion) {
    let hasher = RandomState::new();
    let shard_count: u64 = 64;
    let shard_mask = shard_count - 1;

    let _ = c.bench_function("locate_shard/single_key", |b| {
        b.iter(|| {
            locate_shard(
                black_box(&hasher),
                black_box("user:42"),
                black_box(shard_mask),
            )
        });
    });
}

criterion_group!(benches, bench_locate_shard);
criterion_main!(benches);
