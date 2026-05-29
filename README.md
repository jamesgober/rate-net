<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>rate-net</b>
    <br>
    <sub>
        <sup>A POWERFUL RATE LIMITER</sup>
    </sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/rate-net"><img alt="Crates.io" src="https://img.shields.io/crates/v/rate-net"></a>
    <a href="https://crates.io/crates/rate-net" alt="Download rate-net"><img alt="Crates.io Downloads" src="https://img.shields.io/crates/d/rate-net?color=%230099ff"></a>
    <a href="https://docs.rs/rate-net" title="rate-net Documentation"><img alt="docs.rs" src="https://img.shields.io/docsrs/rate-net"></a>
    <a href="https://github.com/jamesgober/rate-net/actions"><img alt="GitHub CI" src="https://github.com/jamesgober/rate-net/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md" title="MSRV"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>rate-net</strong> is a <b>powerful, lock-free rate limiter</b> for Rust. It answers one question as fast as the hardware allows — <em>"is this key allowed right now?"</em> — and answers it with a decision plus a <code>retry-after</code>, across <b>multiple algorithms</b>, while tracking <b>per-key state</b> under brutal concurrency. 
        Built for <b>maximum throughput</b>, <b>minimum overhead</b>, and <b>correctness under hostile traffic</b> on <b>Linux</b>, <b>macOS</b>, and <b>Windows</b>.
    </p>
    <p>
        Per-key state lives in a <b>sharded concurrent map</b>, so unrelated keys never contend and throughput scales with core count. Each key's bucket is lock-free. Memory is <b>bounded</b>: a flood of unique keys hits an eviction cap instead of growing without limit — the difference between a rate limiter and a denial-of-service vector you built yourself.
    </p>
    <p>
        The common case is one line — <code>RateLimiter::per_second(100)</code> then <code>limiter.check(key)</code>. Power users pick the algorithm, quota, burst, shard count, and eviction policy through a builder. rate-net does not reimplement token-bucket math; it consumes <a href="https://crates.io/crates/better-bucket"><code>better-bucket</code></a> for that and adds the per-key, multi-algorithm, retry-after machinery around it.
    </p>
</div>

<br>

> **Status — pre-release (`v0.3.0`).** The concurrent core is real: per-key state
> lives in a tunable **sharded store** (unrelated keys never contend — an
> existing-key check takes only a read lock plus an atomic), memory is **bounded
> by eviction** so a unique-key flood hits a cap instead of growing without
> limit, and the steady-state check is **allocation-free** — all verified by
> `loom`, a multi-threaded stress test, and an allocation audit. Tune sharding
> and eviction with `.with_shards(n)` and `.with_eviction(policy)`. Still ahead:
> the **leaky-bucket, fixed-window, and sliding-window algorithms** and the
> unified **Tier-2 builder** in `v0.4.0`. Examples for those not-yet-shipped
> surfaces describe the target API; the roadmap drives the order in which they
> become real.

<br>

<h2>Why rate-net</h2>

Rate limiting looks trivial until production traffic finds the edges — the over-admit race at a window boundary, the unbounded key map that OOMs under a unique-key flood, the API so generic you write a wrapper just to limit a loop. rate-net is built against all of them:

- **Lock-free per key.** Each key's bucket delegates to `better-bucket`'s atomic CAS core. No lock on the check path.
- **Sharded state.** Per-key state lives in a sharded concurrent map; unrelated keys never serialize on each other. Throughput scales with cores.
- **Bounded memory.** Idle keys are evicted (LRU/TTL); a hostile unique-key flood hits a cap, it does not grow unbounded.
- **Never over-admits.** For any key and window, admitted requests never exceed the configured quota — under any interleaving. Proven per algorithm with `loom` and `proptest`.
- **Multiple algorithms, one trait.** Token bucket, leaky bucket, fixed window, sliding-window log, sliding-window counter — all behind a single `RateLimiter` trait.
- **One-line API.** `RateLimiter::per_second(n)` then `.check(key)`. The simple path is also the fast path.
- **Honest `retry-after`.** Denials carry how long until the caller may retry.


<br>

## Features

- **Multi-algorithm** — token bucket, leaky bucket, fixed window, sliding-window log, sliding-window counter, all behind one `RateLimiter` trait
- **Per-key limiting** — limit per-IP, per-user, per-endpoint, per-anything; sharded so keys don't contend
- **Lock-free hot path** — per-key buckets delegate to `better-bucket`'s atomic core; zero allocation on the steady-state `check`
- **Bounded memory** — LRU/TTL eviction caps the key space; survives a hostile unique-key flood
- **Retry-after** — every denial reports when the caller may try again
- **Deterministic tests** — inject a mockable clock (via `clock-lib`); advance windows without `sleep`
- **Tier-1 API** — `RateLimiter::per_second(n)` + `.check(key)`; a builder for control; a trait for the 1%
- **No over-admit guarantee** — verified per algorithm with `loom` and `proptest`
- **Sync core** — no async runtime dependency; optional additive async layer

<hr>
<br>

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rate-net = "0.1"

# Full algorithm suite + optional async layer:
rate-net = { version = "0.1", features = ["algorithms", "async"] }
```

<hr>
<br>

## Quick Start

```rust
use rate_net::{RateLimiter, Decision};

// 100 requests per second, per key.
let limiter = RateLimiter::per_second(100);

match limiter.check("user:42") {
    Decision::Allow => {
        // allowed — serve the request
    }
    Decision::Deny { retry_after } => {
        // denied — return 429 with Retry-After: {retry_after}
    }
}
```

One constructor, one method. The key can be anything hashable — an IP, a user
id, an API token, an endpoint name.

<br>

## Configured Limiter (Tier 2)

Today, build an explicit quota with `Quota::rate`, then tune the shard count and
eviction policy with chainable adjusters:

```rust
use rate_net::{RateLimiter, Quota, Eviction};
use std::time::Duration;

// 1000 requests / minute, per key.
let quota = Quota::rate(1000, Duration::from_secs(60)).expect("non-zero quota");

let limiter = RateLimiter::with_quota(quota)
    .with_shards(64)                                  // tune for core count
    .with_eviction(Eviction::capacity(100_000)        // cap the key space
        .with_idle(Duration::from_secs(300)));        // and reclaim idle keys

// Take more than one unit at a time.
let decision = limiter.check_n("tenant:acme", 5);
```

> _Planned for `v0.4`._ The single builder below — folding algorithm selection
> in with these knobs — lands with the full algorithm suite.

Choose the algorithm, quota, burst, shard count, and eviction policy:

```rust
use rate_net::{RateLimiter, Algorithm, Eviction};
use std::time::Duration;

let limiter = RateLimiter::builder()
    .algorithm(Algorithm::SlidingWindowCounter)
    .quota(1000, Duration::from_secs(60)) // 1000 / minute
    .burst(50)                            // allow short bursts
    .shards(64)                           // tune for core count
    .eviction(Eviction::idle(Duration::from_secs(300)))
    .build();

// Take more than one unit at a time.
let decision = limiter.check_n("tenant:acme", 5);
```

<br>

## Algorithms

All algorithms share the same `RateLimiter` trait and `check` surface — swap
the strategy without touching call sites.

| Algorithm | Shape | Good for |
|-----------|-------|----------|
| **Token bucket** | Smooth refill, allows bursts up to capacity | General-purpose; the default |
| **Leaky bucket** | Constant drain, smooths spikes | Shaping bursty traffic to a steady rate |
| **Fixed window** | Counter resets each window | Cheapest; tolerates boundary bursts |
| **Sliding-window log** | Exact timestamps in a window | Highest accuracy; higher memory |
| **Sliding-window counter** | Weighted blend of two windows | Accuracy/cost balance; common production choice |

The token bucket strategy is provided by
[`better-bucket`](https://crates.io/crates/better-bucket) — rate-net does not
reimplement it.

<br>

## Deterministic Testing (mockable clock)

```rust
use rate_net::RateLimiter;
use clock_lib::ManualClock;
use std::sync::Arc;
use std::time::Duration;

// Share the clock with the limiter via `Arc`, then drive it by hand.
let clock = Arc::new(ManualClock::new());
let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));

// Drain the key's quota.
for _ in 0..5 {
    assert!(limiter.check("k").is_allow());
}
assert!(limiter.check("k").is_deny());

// Advance one second — no real sleep — and the allowance is back.
clock.advance(Duration::from_secs(1));
assert!(limiter.check("k").is_allow());
```

<br>

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | ✅ | Standard library. Required for the sharded store and eviction. |
| `algorithms` | ❌ | The full suite beyond the default token bucket (leaky, fixed window, sliding-window log, sliding-window counter). |
| `async`      | ❌ | Optional async-friendly wrapper layer. Additive — the core has no runtime dependency. |

```toml
# Everything:
rate-net = { version = "0.1", features = ["algorithms", "async"] }
```

<br>

## Testing

```bash
# Unit + integration + property tests
cargo test --all-features

# Concurrency model checking (no over-admit under interleaving)
RUSTFLAGS="--cfg loom" cargo test --test loom_check

# Benchmarks
cargo bench --bench rate_bench

# Format + lints (must be clean)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

<hr>
<br>

## Performance

The single-key `check` is designed to land in single-digit nanoseconds in
steady state, and many-key throughput scales near-linearly with shard count.
The Criterion suite covers single-key, many-key (shard scaling), contended
single-key, and eviction-sweep cost:

```bash
cargo bench --bench rate_bench
```

A head-to-head comparison against `governor` (and `tower`'s limiters where
comparable) ships with the performance write-up as the suite matures toward
1.0, numbers recorded honestly.

<br>

## Design

### Sharded, lock-free, bounded

Per-key state lives in a sharded concurrent map. A `check` is: hash the key →
locate the shard → run the key's lock-free bucket. Unrelated keys land in
different shards and never contend, so throughput scales with core count. The
steady-state `check` allocates nothing; allocation happens only the first time
a key is seen.

Memory is **bounded by eviction**. Idle keys are swept (LRU/TTL) on an
amortized, incremental schedule that never stops the world on the hot path. A
flood of unique keys reaches the eviction cap and stays there — it cannot grow
without limit or corrupt live keys. This is a deliberate defense: an
unbounded key map is a self-inflicted denial-of-service.

### The no-over-admit invariant

The defining correctness property: **for any key and window, the number of
admitted requests never exceeds the configured quota — under any concurrent
interleaving.** Verified per algorithm:

- **`loom`** explores the shard + per-key acquire interleavings and asserts no
  key is ever over-admitted.
- **`proptest`** throws arbitrary interleavings of checks and time advances at
  each algorithm and asserts the per-window quota holds.

### Clean boundary

rate-net exposes a decision API — `Allow` / `Deny { retry_after }` — and
nothing else of its internals. Consumers (e.g. a gatekeeper like `bouncer-io`)
call `check` and never reach into rate-net's per-key state. Library reuse, not
state sharing.

<hr>
<br>


## The `-net` family

rate-net is the anchor of the `-net` domain group — independent,
foreign-compatible crates that interoperate within a shared, hyper-optimized
pipeline but never force each other as dependencies. rate-net consumes
[`better-bucket`](https://github.com/jamesgober/better-bucket) for token-bucket
math and [`clock-lib`](https://github.com/jamesgober/clock-lib) for time, and
is itself consumed by gatekeepers via the clean `check` API. It works
standalone, with no obligation to adopt the rest of the family.

<hr>
<br>

## Cross-Platform Support

**Tier 1 Support:**
- ✅ Linux (x86_64, aarch64)
- ✅ macOS (x86_64, Apple Silicon)
- ✅ Windows (x86_64)

Atomic and sharding behavior is identical across all three; the CI matrix runs
every target on stable and MSRV.

<br><br>

## Contributing

Contributions are welcome. Before opening a PR, make sure `cargo fmt`,
`cargo clippy --all-targets --all-features -- -D warnings`, and
`cargo test --all-features` are all clean. Any new algorithm must arrive with
its own `proptest` over-admit proof and retry-after tests; any change to the
shard or eviction path needs a `loom` test and a benchmark.

<br><br>

<!-- LICENSE
############################################# -->
<div id="license">
    <h2>⚖️ License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> — see <a href="./LICENSE-APACHE">LICENSE-APACHE</a> (<a href="http://www.apache.org/licenses/LICENSE-2.0" target="_blank">http://www.apache.org/licenses/LICENSE-2.0</a>)</li>
        <li><b>MIT License</b> — see <a href="./LICENSE-MIT">LICENSE-MIT</a> (<a href="http://opensource.org/licenses/MIT" target="_blank">http://opensource.org/licenses/MIT</a>)</li>
    </ul>
    <p>at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.</p>
</div>

<!-- FOOT COPYRIGHT
################################################# -->
<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>JAMES GOBER.</strong></sup>
</div>
