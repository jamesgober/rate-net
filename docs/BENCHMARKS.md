<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>rate-net</b><br>
    <sub><sup>BENCHMARKS</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="./API.md" title="API Reference"><b>API</b></a>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>
<br>

> Numbers recorded for `v1.1.0`. Treat absolute numbers as machine-specific;
> the relative shape between paths, the before/after, and the head-to-head are
> the signal. Single runs on a developer workstation: expect a few nanoseconds
> of run-to-run noise.

## Method

```bash
cargo bench --bench rate_bench
```

The suite ([`benches/rate_bench.rs`](../benches/rate_bench.rs)) uses limiters
with no effective rate ceiling (default token bucket), so it measures the check
machinery — key construction and hash, shard read lock, index probe, the
per-key accounting, and the clock read — not the denial branch.

- **`check/single_key`** — the steady-state hot path: one existing `&str` key.
- **`check/single_key_coarse_clock`** — the same on a 1 ms
  [`CoarseClock`](./API.md#coarseclock).
- **`check/many_keys`** — 10,000 distinct `u64` keys across 64 shards, cycled.
- **`check/contended_single_key_4t`** — four threads hammering one key; wall
  time divided by the total number of checks (an aggregate throughput figure).
- **`check/eviction_sweep`** — one shard capped at 64 keys, a new key every
  call, so each measurement inserts and evicts.
- **`check/insert_at_default_cap`** — the default limiter (default shard count,
  1,048,576-key cap) filled to its cap, a new key every call: the unique-key
  flood steady state at realistic scale.
- **`check/direct`**, **`check/direct_coarse_clock`** — a
  [`DirectLimiter`](./API.md#directlimiter): no key, no map, no shard lock.
- **`peek/existing_key`** — reading a snapshot for `RateLimit-*` headers.

Recorded on Windows 11 x86_64 (32 logical CPUs), Rust 1.95.0, Criterion `bench`
profile (`opt-level = 3`), against `better-bucket` 1.1.0.

## Results

| Benchmark | v1.0.0 | v1.1.0 |
|-----------|-------:|-------:|
| `check/single_key` | 66.3 ns | 66.2 ns |
| `check/single_key_coarse_clock` | — | **35.0 ns** |
| `check/many_keys` | 86.6 ns | 54.9 ns |
| `check/contended_single_key_4t` | 151.9 ns/op | 49.5 ns/op |
| `check/eviction_sweep` (64-key shard) | 398 ns | 100.7 ns |
| `check/insert_at_default_cap` | ~329 µs | **0.36 µs** |
| `check/direct` | — | 33.4 ns |
| `check/direct_coarse_clock` | — | **8.2 ns** |
| `peek/existing_key` | — | 74.3 ns |

The `v1.0.0` column is the audit's quick-mode measurement of the `1.0.0` code
on the same machine (the insert-at-cap figure is from its probe at the default
cap); the numbers `1.0.0` itself published (~54 ns everywhere, ~217 ns for the
sweep) could not be reproduced and are withdrawn.

What changed:

- **Eviction is O(1).** At capacity, `1.0.0` scanned the whole shard for the
  least-recently-seen key on every insert — ~329 µs at the default cap, roughly
  a thousand times the cost of the insert itself, under the shard write lock.
  CLOCK eviction examines at most 32 slots, so an insert at the default cap
  costs ~0.36 µs, and a small capped shard ~0.1 µs.
- **No shared counter on the check path.** `1.0.0` stamped every check with a
  `fetch_add` on a per-shard sequence counter — a contended read-modify-write
  that every thread touching the shard bounced. The CLOCK reference bit is
  written only when it was clear, so a hot key's check writes nothing beyond the
  bucket's own CAS. Four threads on one key went from 151.9 to 49.5 ns per check.
- **One hash per check, padded shards.** The shard and the slot are found from
  one hash (`1.0.0` hashed twice), and each shard sits on its own cache lines.
  Many-key checks went from 86.6 to 54.9 ns.
- **`CoarseClock`** replaces the ~26 ns OS clock read with one atomic load,
  roughly halving the single-key check and taking the direct check to ~8 ns.
- **`peek`** reads the clock once for both the remaining count and the reset
  time.

`check/single_key` is unchanged: its cost is the `&str` key, the shard read
lock, `better-bucket`'s accounting, and above all the OS clock read (~26 ns on
this machine), none of which this release changes on the default clock.

## Head-to-head vs `governor`

`governor` (0.10.4) is a benchmark-only dependency gated behind
`cfg(comparison)` so it never enters the default tree or CI. Run:

```bash
RUSTFLAGS="--cfg comparison" cargo bench --bench comparison
```

| Path | rate-net | governor |
|------|---------:|---------:|
| keyed, single `u64` key | 51.7 ns | **20.0 ns** |
| keyed, single key, rate-net on `CoarseClock` | 22.6 ns | **19.2 ns** |
| keyed, 10,000 keys | 56.4 ns | **21.2 ns** |
| direct | 32.4 ns | **7.8 ns** |
| direct, rate-net on `CoarseClock` | 8.6 ns | **8.3 ns** |

**On the default clock `governor` is faster, and most of the gap is the
clock.** `governor` reads time through [`quanta`](https://crates.io/crates/quanta),
a TSC-based clock costing a few nanoseconds; rate-net's default `SystemClock`
reads `std::time::Instant::now()` (`QueryPerformanceCounter`, ~26 ns here). On
`CoarseClock` the direct paths are at parity (8.6 vs 8.3 ns) and a keyed single
key is within ~3.5 ns.

The remaining keyed gap is architecture: rate-net finds a key under a per-shard
`RwLock` (a read-lock acquire and release per check) in a store that also
supports bounded memory, CLOCK eviction, idle expiry, five algorithms, `peek`,
and key management; `governor`'s keyed store is a `DashMap` over a GCRA cell
with no automatic eviction. The many-key gap is the same cost.

## Reading the numbers

- **Single-key** on the default clock is dominated by the clock read; on
  `CoarseClock`, by the shard read lock and the bucket's CAS.
- **Many-key** costs about what a single key does: unrelated keys land in
  different shards and take only a read lock.
- **Contended single-key** scales: four threads on one key share the read lock
  and settle on the bucket's atomic. The remaining shared writes are the read
  lock's own reader count (an atomic increment and decrement per check) and the
  bucket's compare-and-swap; the eviction marks are not rewritten.
- **Eviction** is the cold path — the write lock plus a bounded CLOCK step — paid
  only when a new key displaces an old one, and constant whatever the shard
  size.

## What's next

- A lock-free read path for existing keys (an epoch- or seqlock-protected index)
  would remove the read-lock acquire from the keyed check; it is deferred until
  it can be model-checked as thoroughly as the current store.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
