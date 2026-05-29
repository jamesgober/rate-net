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

> Numbers from the `v0.6.0` optimization pass. Treat absolute numbers as
> machine-specific; the relative shape between paths, the trend across versions,
> and the head-to-head are the signal.

## Method

```bash
cargo bench --bench rate_bench
```

The suite ([`benches/rate_bench.rs`](../benches/rate_bench.rs)) uses a limiter
with no effective rate ceiling (default token bucket), so it measures the check
machinery — key hash, shard read lock, map lookup, the per-key accounting — not
the denial branch.

- **`check/single_key`** — the steady-state hot path: one existing key.
- **`check/many_keys`** — 10 000 distinct keys across 64 shards, cycled.
- **`check/contended_single_key_4t`** — four threads hammering one key.
- **`check/eviction_sweep`** — a single shard capped at 64 keys, a new key every
  call, so each measurement inserts and evicts.

Recorded on Windows x86_64, Rust stable 1.95.x, Criterion `bench` profile
(`opt-level = 3`).

## Results — `v0.6.0` vs the `v0.5.0` baseline

| Benchmark | v0.5.0 | v0.6.0 | Change |
|-----------|-------:|-------:|-------:|
| `check/single_key` | ~77 ns | **~54 ns** | −30% |
| `check/many_keys` | ~99 ns | **~54 ns** | −45% |
| `check/contended_single_key_4t` | ~76 ns/op | **~54 ns/op** | −29% |
| `check/eviction_sweep` | ~287 ns | **~217 ns** | −24% |

What changed in `0.6.0`:

- **`ahash` instead of SipHash** for shard selection and the shard map — fast,
  and still collision-attack resistant thanks to a random per-store seed.
- **No redundant clock read.** For the default path (token bucket, capacity-only
  eviction) the limiter no longer reads the clock at all: `better-bucket` already
  reads it for refill, and least-recently-seen eviction orders by a cheap
  per-shard logical counter instead of wall time. The clock is only read when a
  window algorithm or an idle TTL actually needs real time.
- **`#[inline]`** on the check path so it inlines across the crate boundary.

## Head-to-head vs `governor`

`governor` is a benchmark-only dependency gated behind `cfg(comparison)` so it
never enters the default tree or CI. Run:

```bash
RUSTFLAGS="--cfg comparison" cargo bench --bench comparison
```

| Path | rate-net | governor |
|------|---------:|---------:|
| single key | ~44 ns | **~17 ns** |
| many keys | ~54 ns | **~18 ns** |

**`governor` is currently faster, and the gap is almost entirely the clock.** By
default `governor` reads time through [`quanta`](https://crates.io/crates/quanta),
a TSC-based clock that costs ~5 ns. rate-net reads time through `better-bucket` →
[`clock-lib`](https://crates.io/crates/clock-lib) → `std::time::Instant::now()`,
which on Windows is `QueryPerformanceCounter` at ~20 ns. That ~15 ns difference,
read on every check, accounts for the bulk of the gap; the rest is rate-net's
extra layering (the sharded read-lock store over `better-bucket`) versus
`governor`'s tightly-integrated GCRA-on-`quanta`.

rate-net's *own* per-key overhead — hash, shard read lock, dispatch — is
competitive once the clock is held equal. It cannot beat `governor` end-to-end
while its clock is ~4× slower, and that clock lives in `clock-lib` /
`better-bucket` (sibling crates), not here. Closing the gap needs a fast
TSC/`quanta`-style monotonic source in `clock-lib`; that is tracked as a separate
sibling enhancement. This is recorded honestly rather than papered over.

## Reading the numbers

- **Single-key** is now dominated by the one unavoidable clock read (in
  `better-bucket`) plus the shard read lock; the hash and dispatch are a small
  fraction.
- **Many-key** matches single-key: unrelated keys land in different shards and
  take only a read lock, so they do not serialise.
- **Contended single-key** stays close to the uncontended cost — concurrent
  checks of one key share a read lock and settle on the per-key atomic.
- **Eviction sweep** is the cold path (write lock + scan of the small capped
  shard), an order of magnitude more than a steady check, and paid only when a
  new key displaces an old one.

## What's next

- A fast monotonic clock in `clock-lib` (raised separately) would cut the single
  unavoidable clock read and is the path to beating `governor`.
- A possible further micro-optimization: fuse the two key hashes (shard
  selection and the map probe) into one via a precomputed-hash raw entry. It
  saves only a few nanoseconds and is dwarfed by the clock, so it is deferred.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
