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

> Numbers recorded for `v2.0.0`, with the at-capacity insert re-measured for
> `v2.1.0`. Treat absolute numbers as machine-specific;
> the relative shape between paths, the before/after, and the head-to-head are
> the signal.

## Method

```bash
cargo bench --bench rate_bench   # per-check latency (Criterion)
cargo bench --bench flood        # legitimate-key tail latency under a flood
```

The Criterion suite ([`benches/rate_bench.rs`](../benches/rate_bench.rs))
uses limiters with no effective rate ceiling (default token bucket), so it
measures the check machinery — key construction and hash, shard read lock,
index probe, the per-key accounting, and the clock read — not the denial
branch. Every figure is the time of **one check** as its caller sees it.

- **`check/single_key`** — the steady-state hot path: one existing `&str` key.
- **`check/single_key_coarse_clock`** — the same on a 1 ms
  [`CoarseClock`](./API.md#coarseclock).
- **`check/many_keys`** — 10,000 distinct `u64` keys across 64 shards, cycled.
- **`check/contended_single_key_4t`** — four threads hammering one key. Each
  thread times its own loop (after a barrier, so thread start-up is not
  measured), and the figure is the mean time of one check under 4-way
  contention. (`v1.1.0` and earlier reported this benchmark as aggregate
  throughput — wall time divided by the total number of checks across all four
  threads — which reads about four times faster than a caller ever sees; those
  figures are withdrawn.)
- **`check/long_key`, `check/long_key_bytes`** — a 36-byte UUID key through
  `check` (which copies it into an owned key, an allocation) and through
  `check_bytes` (which borrows it).
- **`check/eviction_sweep`** — one shard capped at 64 keys, a new key every
  call, so each measurement inserts and evicts.
- **`check/insert_at_default_cap`** — the default limiter (default shard count,
  1,048,576-key cap) filled to its cap, a new key every call: the unique-key
  flood steady state at realistic scale.
- **`check/direct`**, **`check/direct_coarse_clock`** — a
  [`DirectLimiter`](./API.md#directlimiter): no key, no map, no shard lock.
- **`peek/existing_key`** — reading a snapshot for `RateLimit-*` headers.

The flood harness ([`benches/flood.rs`](../benches/flood.rs)) fills the default
limiter to its cap, then times every check of 1,000 legitimate, tracked keys on
one thread while three other threads insert unique keys as fast as they can, and
reports the legitimate checks' p50 / p99 / p99.9 / max. Three scenarios: no
flood; a flood of keys that have recovered by the time they are evicted
(lossless evictions); and a flood of keys that each drain a 1-per-minute quota,
so every eviction records a debt — millions within one recovery period, far
past what the exact recovery table holds, so most spill into the overflow
sketch. Timing uses `Instant::now()`, whose resolution on Windows is 100 ns —
hence the round numbers.

**Conditions.** Windows 11 x86_64 (32 logical CPUs), Rust 1.99.0, Criterion
`bench` profile (`opt-level = 3`, fat LTO, one codegen unit), against
`better-bucket` 1.1.0. The machine was **shared with several other build and
test jobs** (40–90 % CPU load) throughout. To keep the comparison fair, `v1.1.0`
(from crates.io, the identical benchmark code in a scratch crate) and `v2.0.0`
were run back to back, alternately, several times; the table gives the best run
of each, and anything within ~10 % is noise at this load.

## Results

| Benchmark | v1.1.0 | v2.0.0 |
|-----------|-------:|-------:|
| `check/single_key` | 60.6 ns | 59.1 ns |
| `check/single_key_coarse_clock` | 36.3 ns | 37.5 ns |
| `check/many_keys` | 64.0 ns | 55.4 ns |
| `check/contended_single_key_4t` (per check) | 223 ns | 229 ns |
| `check/long_key` (36-byte key, `check`) | 87.2 ns | 80.9 ns |
| `check/long_key_bytes` (36-byte key, `check_bytes`) | — | **52.4 ns** |
| `check/eviction_sweep` (64-key shard) | 108 ns | 98.7 ns |
| `check/insert_at_default_cap` | 293 ns | 351 ns |
| `check/direct` | 38.1 ns | 37.2 ns |
| `check/direct_coarse_clock` | 12.2 ns | 11.1 ns |
| `peek/existing_key` | 80.4 ns | 54.1 ns |
| `check/blocked_key` (a key whose quota admits nothing) | — | 53 ns |

Legitimate-key check latency, default limiter at its cap:

| Scenario | p50 | p99 | p99.9 |
|----------|----:|----:|------:|
| no flood | 100 ns | 100 ns | 300 ns |
| flood of recovered keys | 100 ns | 300 ns | 500–600 ns |
| flood of drained keys (every eviction records a debt), default `Overflow::FailClosed` | 100 ns | 600 ns | 1.2 µs |
| the same, `Overflow::Forget` | 100 ns | 400 ns | 1.3 µs |
| `v1.1.0`, flood | 100 ns | 300–500 ns | 0.5–1.0 µs |
| `v1.0.0`, flood (audit, 2026-10-13) | — | — | 128 µs |

(Flood rows re-run on the final 2.0.0 build, 2026-10-14, same host at about
40 % load.)

The maximum is not in the table: on this loaded host it was set by the OS
preempting a thread (0.3–160 ms in *every* scenario, the no-flood one
included), not by the limiter.

What changed:

- **Typed, smaller keys.** Per-key state is stored as the algorithm's own type,
  with no enum tag (whose alignment padded a token-bucket key to a third cache
  line), and the token bucket keeps `better-bucket`'s clock-free `BucketCore`
  instead of a clock-carrying `Bucket`. A token-bucket key is 128 bytes in its
  shard; heap per key at the default cap fell from ~202 B to ~138 B
  (`tests/memory.rs`). Fewer cache lines per key is most of the many-key and
  `peek` gains.
- **Borrowed long keys.** `check` still builds an owned key, which allocates
  for anything over 22 bytes; `check_bytes` looks the key up by a borrowed view,
  ~35 % faster for a UUID and allocation-free.
- **One clock read, as late as possible.** The limiter reads the clock itself
  (the bucket no longer carries one) immediately before the key's
  compare-and-swap, so a contended CAS acts on a fresh time and retries less.
- **Insertion at capacity cost more in 2.0.0.** To never reset a limit, the
  CLOCK hand prefers a recovered key, and an unrecovered victim's debt is
  recorded. 2.0.0 measured that at ~58 ns more per insert at the default cap;
  2.1.0 traces it to the recovery-hint arithmetic and removes it (see
  below).
- **Under a flood**, legitimate tail latency stays around a microsecond even
  when every eviction must record a debt and most debts spill past the exact
  recovery table. A cuckoo walk through a full table costs a cache miss per
  move, so after a walk fails the next 32 debts spill without one; without
  that back-off the drained-flood p99.9 was 2.1–2.8 µs.

## New key at capacity: resolved in 2.1.0

`2.0.0` shipped with a known issue: inserting a never-seen key into a full
store was reported ~58 ns slower than in `1.1.0` (`check/insert_at_default_cap`
293 ns against 351 ns, criterion medians on a loaded host). `2.1.0` measures it
again, the steadier way, and removes it.

**Method.** Each variant is its own binary (fat LTO, `opt-level = 3`) against
one shared harness: fill the default limiter (1,048,576-key cap) to twice its
cap, then time 2,000,000 further unique keys; one fresh process per
measurement, pinned to one core, variants alternated round by round (8 to 14
rounds per comparison), Windows 11 x86_64, 32 logical CPUs, host load 10–96 %
from other builds. Reported as medians per variant and as the median of
per-round ratios (each round's variants ran back to back under the same load).

**What the regression was.** Pinned, the gap is +16 ns rather than +58 ns
(`1.1.0` ~220 ns, `2.0.1` ~235 ns; per-round ratio 1.065, n = 34): the larger
figure was mostly scheduler noise. Variants that each switch one piece off put
all of it on one line of arithmetic: every inserted key's "recovers at" hint
was computed exactly when the key was stored, and for the token bucket that is
`better-bucket`'s `time_until_at` (two 128-bit divisions) plus a 128-bit
millisecond rounding in rate-net. With the hint computation off and everything
else on, an insert costs 222 ns; with the eviction's recovery check off and the
hint computation on, 244 ns. Reading candidates during the sweep was never the
cost: the hint sits beside the CLOCK reference bit, on a line the sweep reads
anyway.

**The fix.** The hint for a key being stored now skips the exact arithmetic in
the common cases. A fresh key that spent nothing (or was denied) is whole, so
its hint is `0` with no read at all. A fresh key that spent a little of its
quota is usually whole again within the millisecond, and since a hint is in
whole milliseconds rounded up, the next boundary is then the exact hint:
asking the state whether it is full by then is one multiply and a compare.
Only a key still owing past the boundary (or restored from a recorded debt)
pays for the exact time. The millisecond rounding is now 64-bit arithmetic.
And because hints round up, a small shard evicting keys stored microseconds
earlier used to see every candidate as still owing; a hint due within the
current millisecond is now confirmed with one cheap fullness read before the
key is treated as owing. The guarantee is unchanged: a hint is never earlier
than the truth, so the sweep never takes a still-limited key for a recovered
one (`store::tests::test_first_hint_is_exact_or_a_valid_later_bound`).

| Per insert, pinned medians | `1.1.0` | `2.0.1` | `2.1.0` | `2.1.0`, recovery check off |
|-----------------------------|--------:|--------:|--------:|-----------------------------:|
| New key at the default cap (keys recover at once) | 220 ns | 233 ns | 219 ns | 217 ns |
| New key, 64-key shard (`eviction_sweep`'s setup) | 97 ns | 146 ns | 88 ns | 84 ns |
| New key at the default cap, every key drained (`per_minute(1)`) | 220 ns (resets keys) | 424–448 ns | 434–448 ns | 221 ns (resets keys) |

Per-round ratios at the default cap: `2.1.0` / `1.1.0` = 0.997, `2.1.0` /
`2.0.1` = 0.945. The last column switches the recovery check off entirely (it
would reset still-limited keys, so it is a bound, not an option): `2.1.0` is
within 2 ns of it. Checks of tracked keys are unchanged (hot key ~53 ns, 10,000
keys ~49 ns in all three versions).

In the drained flood every eviction must record a debt, which `1.1.0` and
the check-off bound do not do (they reset the evicted key); that cost is what
2.0's guarantee buys, not the regression. `2.1.0` and `2.0.1` do the same work
there; their per-round ratio, 1.02–1.04 across four runs with quartiles about
±8 %, is within this host's noise for a memory-bound scenario.

Criterion, `2.1.0` against `2.0.1`, two alternating runs each on the same
loaded host: `check/eviction_sweep` 92 to 99 ns against 171 to 173 ns,
`check/insert_at_default_cap` 352 to 361 ns against 376 to 417 ns; the other
paths within the run-to-run spread (the pinned harness puts hot-key and
10,000-key checks at a per-round ratio of 1.006 to `2.0.1`). The flood harness
(`benches/flood.rs`) shows the same legitimate-key tails for both (p99 400 /
700 / 500 ns, p99.9 700 to 800 / 1,500 / 1,200 to 1,300 ns for the recovered,
drained, and `Forget` floods), with about 12 % more flood inserts per run in
the recovered-key flood.

## Liveness under eviction pressure

A still-limited key that is evicted keeps its debt, exactly and without
affecting any other key, while the store's still-limited evicted keys fit
about twice the cap (`tests/eviction_bypass.rs`, `tests/eviction_liveness.rs`).
Past that, the default `Overflow::FailClosed` keeps them in a shared sketch
that can charge or refuse keys that never owed anything, and the opt-in
`Overflow::Forget` drops the debts that do not fit. Measured with the 2.0
re-audit's harness (ManualClock, 128 shards; re-run on the final build,
2026-10-14; "first candidate" is the first 2.0 candidate, then numbered
1.2.0):

| Scenario | First candidate | 2.0.0 `Forget` | 2.0.0 `FailClosed` (default) | 1.1.0 |
|----------|---------------------:|------:|------:|------:|
| Newcomers denied after a drain flood of 1.5x the cap (cap 100,000) | 23 % | 0 % | 0 % | 0 % |
| … 2x the cap | 61 % | 0 % | 0 % | 0 % |
| … 3x the cap | 88 % | 0 % | 0 % | 0 % |
| … 4x the cap | — | 0 % | 3 % | 0 % |
| … 6x the cap | 98 % | 0 % | 38 % | 0 % |
| … 9x the cap | — | 0 % | 86 % | 0 % |
| False denials, well-behaved population 3x the cap (cap 10,000) | 0.6–87 % | 0 % | 0 % | 0 % |
| … 5x the cap, 5,000 visits/s | — | 0 % | 1.4 % | 0 % |
| … 10x the cap, 5,000 visits/s | 89 % | 0 % | 4.2 % | 0 % |
| … 10x the cap, 20,000 visits/s | — | 0 % | 72 % | 0 % |
| … 2x the cap (cap 50,000) | 0.6 % | 0 % | 0 % | 0 % |
| Configurations with over-admission, of 60 capacity-eviction bypass runs (500 drained victims; caps 64 and 10,000; floods 2–100x) | 0 | 48 | 0 | every one in which a victim is evicted (1.1.0 forgot every debt) |
| Real-clock race, 4 drained victims vs drain-flood threads (cap 32): max units a victim got (bound 141–160) | 140 | up to 548,000 | 140 | unbounded: every eviction reset the victim |

The population rows depend on churn, not on the population: the binding
quantity is how many still-limited keys are evicted within one recovery period
(the eviction rate times the recovery time), against about twice the cap.

The 1.1.0 column's bypass and race cells are stated, not re-measured: 1.1.0
evicted without recording anything, so every eviction of a still-limited key
reset it — the 2026-10-13 audit's RN-005, which `tests/eviction_bypass.rs`
reproduces against 1.1.0 for every algorithm. It behaves like `Forget` with no
exact table at all, which is also why it never denied a newcomer.

`Forget` never denies a key that owed nothing, and resets evicted keys only
once more of them are owed at once than the table holds — then, under a
sustained flood, over and over (the last two rows). `FailClosed` never resets a
key, and denies keys that owed nothing once the same bound is passed. `FailClosed`
is the default; opt into `Forget` only where availability beats strictness.
See the README's "Choosing an overflow policy" and "Sizing the cap".

Memory at the default cap after twice the cap of drained churn (counting
allocator): 138 MiB of per-key state, plus 64 MiB of recovery tables under
`FailClosed` (202 MiB) or 32 MiB under `Forget` (170 MiB).

The guardrails cost nothing on the check path: `check/single_key` with the
`metrics` feature measured 57.5 / 58.9 ns without `tracing` and 57.4 / 61.2 ns
with it (two alternating runs; within noise). `recovery_load()` walks the
recovery tables (up to 64 bytes per key of capacity) and is meant for a
periodic exporter, not the request path.

### A blocked key

`check/blocked_key` — a key whose `quota_for` override admits nothing — is
denied without taking the shard write lock: 53 ns per check (criterion median,
same host). With 8 threads hammering such a key, a legitimate key in the same
shard sees p50 400 ns and p99.9 1.5 µs (before: 2.2 µs and 30.9 µs).

## Head-to-head vs `governor`

`governor` (0.10.4) is a benchmark-only dependency gated behind
`cfg(comparison)` so it never enters the default tree or CI. The table below
was measured at `v1.1.0`; an attempt to re-run it during the 2.0 cycle coincided with
the host's heaviest load and produced numbers 3–10x off for *both* crates, so it
is not reported. rate-net's own paths measured within noise of `v1.1.0` above
(the keyed check slightly faster), so the comparison is expected to stand. Run:

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
- **Contended single-key** is the price of four cores writing one cache line:
  every check bounces the read lock's reader count (an atomic increment and
  decrement) and the bucket's compare-and-swap word between cores, so one check
  takes ~230 ns under 4-way contention against ~60 ns alone. That is per caller;
  the four threads together still complete a check every ~57 ns. The eviction
  marks are not rewritten on a hot key, so they add nothing.
- **Eviction** is the cold path — the write lock, a bounded CLOCK step, and (for
  an unrecovered victim) one debt record — paid only when a new key displaces an
  old one, and constant whatever the shard size.

## What's next

- A lock-free read path for existing keys (an epoch- or seqlock-protected index)
  would remove the read-lock acquire from the keyed check; it is deferred until
  it can be model-checked as thoroughly as the current store.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
