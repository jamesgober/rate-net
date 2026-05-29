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

> Baseline numbers for the four paths in the Criterion suite, recorded at the
> `v0.5.0` feature freeze. These are the **pre-optimization** baseline: `v0.6.0`
> tightens the per-check cost (single keyed hash, shard-lookup fusion) and adds
> the head-to-head comparison against `governor`. Treat the absolute numbers as a
> floor to improve on, and the relative shape between paths as the signal.

## Method

```bash
cargo bench --bench rate_bench
```

The suite ([`benches/rate_bench.rs`](../benches/rate_bench.rs)) uses a limiter
with no effective rate ceiling, so it measures the check machinery — key hash,
shard read lock, map lookup, and the algorithm's atomic accounting — rather than
the denial branch. Default algorithm (token bucket).

- **`check/single_key`** — the steady-state hot path: one existing key.
- **`check/many_keys`** — 10 000 distinct keys across 64 shards, cycled.
- **`check/contended_single_key_4t`** — four threads hammering one key; the
  reported figure is wall time per operation under contention.
- **`check/eviction_sweep`** — a single shard capped at 64 keys, a brand-new key
  every call, so each measurement inserts and evicts.

## Results

Recorded on Windows x86_64, Rust stable 1.95.x, Criterion `bench` profile
(`opt-level = 3`). Numbers vary by CPU and environment; compare trends, not
absolutes across machines.

| Benchmark | Median time |
|-----------|------------:|
| `check/single_key` | ~77 ns |
| `check/many_keys` | ~99 ns |
| `check/contended_single_key_4t` | ~76 ns/op |
| `check/eviction_sweep` | ~287 ns |

## Reading the numbers

- **Single-key** is dominated by hashing the key (twice today — once to pick the
  shard, once inside the shard map) and the read-lock acquire; the bucket's CAS
  is a small fraction. Fusing those two hashes and using a faster keyed hasher is
  the headline `0.6` optimization.
- **Many-key** costs a little more than single-key — more cache misses across
  10 000 entries — but does not collapse, because unrelated keys land in
  different shards and take only a read lock.
- **Contended single-key** stays close to the uncontended single-key cost:
  concurrent checks of one key share a read lock and the bucket settles
  contention with a CAS, rather than serialising on a write lock.
- **Eviction sweep** is the cold path — a first-seen key under capacity pressure
  takes the shard write lock and scans the (small, capped) shard for the
  least-recently-seen victim. It is an order of magnitude more than a steady
  check, as expected, and is paid only when a new key displaces an old one.

## What's next (`0.6.0`)

- Reduce `check/single_key` toward single-digit nanoseconds: one keyed hash for
  both shard selection and the map probe, and a faster hasher than SipHash.
- Document many-key throughput scaling with shard count up to the core count.
- A head-to-head comparison against [`governor`](https://crates.io/crates/governor),
  with method and numbers, recorded here.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
