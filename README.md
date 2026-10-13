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
        <strong>rate-net</strong> is a <b>rate limiter</b> for Rust. It answers one question — <em>"is this key allowed right now?"</em> — with a decision plus a <code>retry-after</code>, across <b>five algorithms</b>, while tracking <b>per-key state</b> under heavy concurrency.
        Built for <b>throughput</b>, <b>low overhead</b>, and <b>correctness under hostile traffic</b> on <b>Linux</b>, <b>macOS</b>, and <b>Windows</b>.
    </p>
    <p>
        Per-key state lives in a <b>sharded store</b>: an existing key costs one shard read lock plus the algorithm's atomic accounting, so unrelated keys rarely contend. Memory is <b>bounded</b> by O(1) eviction: a flood of unique keys hits a cap instead of growing without limit, and eviction never scans the store — the difference between a rate limiter and a denial-of-service vector you built yourself.
    </p>
    <p>
        The common case is one line — <code>RateLimiter::per_second(100)</code> then <code>limiter.check(key)</code>. Power users pick the algorithm, quota, burst, shard count, eviction policy, and clock through a validating builder. rate-net does not reimplement token-bucket math; it consumes <a href="https://crates.io/crates/better-bucket"><code>better-bucket</code></a> for that and adds the per-key, multi-algorithm, retry-after machinery around it.
    </p>
</div>

<br>

> **Stable (`v1.1.0`).** The public API is frozen until `2.0`; `1.1` only adds
> to it. Five algorithms (token bucket by default; leaky bucket, fixed window,
> sliding-window log, and sliding-window counter under the `algorithms`
> feature) behind one `Limiter` trait; a validating builder; `peek` snapshots
> and `RateLimit-*` / `Retry-After` helpers for HTTP; key management; an
> un-keyed `DirectLimiter` and a `no_std` `Gcra` core; a fast `CoarseClock`;
> optional metrics and an async await-until-ready layer. `1.1.0` also fixes a
> real-clock over-admission bug in the default token bucket (via
> `better-bucket` 1.1) and an O(n) eviction scan — see the
> [CHANGELOG](./CHANGELOG.md).

<br>

<h2>Why rate-net</h2>

Rate limiting looks trivial until production traffic finds the edges — the over-admit race when two threads cross a millisecond, the unbounded key map that OOMs under a unique-key flood, the eviction pass that stalls every request behind it, the API so generic you write a wrapper just to limit a loop. rate-net is built against all of them:

- **Atomic accounting, sharded locking.** The token bucket, leaky bucket, and fixed window account each key with a single compare-and-swap; the sliding-window algorithms hold a short per-key lock. Keys are found under a per-shard read lock, so checks of unrelated keys proceed in parallel.
- **Never over-admits.** Admissions for a key never exceed its algorithm's documented bound, under any interleaving of threads, on a real clock. Checked by `loom` against the production code, property-tested per algorithm, and regression-tested by many threads hammering one key on the OS clock.
- **Bounded memory, O(1) eviction.** A capacity cap evicts by CLOCK (second chance) with a bounded scan, and an idle TTL drains incrementally. Capped shards are kept to 16,384 keys, so even the occasional amortized index growth stays short.
- **Five algorithms, one trait.** Token bucket, leaky bucket (GCRA), fixed window, sliding-window log, sliding-window counter — swap the strategy without touching call sites.
- **HTTP-ready.** Denials carry an honest `retry-after`; `retry_after_secs()` rounds it **up** for the header, and `peek` feeds the `RateLimit-*` headers on every response.
- **Safe keys.** Tagged keys (`Key::id`, `Key::ip`) cannot collide with attacker-chosen strings, and `Key::ipv6_prefix(addr, 64)` stops a client rotating through its own `/64`.
- **One-line API.** `RateLimiter::per_second(n)` then `.check(key)`. The simple path is also the fast path.

<br>

## Features

- **Multi-algorithm** — token bucket, leaky bucket, fixed window, sliding-window log, sliding-window counter, all behind one `Limiter` trait (and the object-safe `DynLimiter`)
- **Per-key limiting** — per-IP, per-subnet, per-user, per-endpoint, per-anything; sharded so keys rarely contend
- **Direct limiting** — `DirectLimiter` for one stream: no key, no map, no lock
- **`no_std` core** — `Gcra`, a clock-free single-key limiter driven by caller-supplied time
- **Validated configuration** — `Builder::try_build` rejects zero periods, zero bursts, out-of-range rates, and idle TTLs that would reset a key early; `build` fails closed
- **Bounded memory** — O(1) CLOCK eviction plus an incremental idle sweep; survives a hostile unique-key flood
- **Retry-After and RateLimit headers** — rounded-up `Retry-After`, `peek` snapshots for `RateLimit-Limit` / `-Remaining` / `-Reset`
- **Key management** — `peek`, `reset`, `remove`, `clear`, `retain_recent`, `shrink_to_fit`
- **Fast clock** — `CoarseClock` replaces the OS clock read on the check path with one atomic load
- **Deterministic tests** — inject a mockable clock (via `clock-lib`); advance windows without `sleep`
- **Metrics** — allowed / denied / inserted / evicted / expired / removed counters, compiled out unless the `metrics` feature is on
- **Sync core** — no async runtime dependency; an optional additive async layer

<hr>
<br>

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rate-net = "1.1"

# Full algorithm suite, async waiting, and metrics:
rate-net = { version = "1.1", features = ["algorithms", "async", "metrics"] }
```

<hr>
<br>

## Quick Start

```rust
use rate_net::{Decision, RateLimiter};

// 100 requests per second, per key.
let limiter = RateLimiter::per_second(100);

match limiter.check("user:42") {
    Decision::Allow => {
        // allowed — serve the request
    }
    Decision::Deny { retry_after } => {
        // denied — return 429 with Retry-After: {retry_after}
        let _ = retry_after;
    }
    // `Decision` is `#[non_exhaustive]`: future variants land here.
    _ => {}
}
```

One constructor, one method. The key can be anything that converts into a
`Key` — a string, an IP address, a socket address, a user id.

<br>

## Configured Limiter (Tier 2)

The builder selects the algorithm, quota, burst, shard count, eviction policy,
and clock in one fluent surface. `try_build` validates it; `build` never fails
and fails *closed* on a degenerate configuration (a zero limit or period, or a
zero burst for the buckets, denies everything rather than admitting
everything):

```rust
use rate_net::{Eviction, RateLimiter, RateLimiterError};
use std::time::Duration;

fn build() -> Result<(), RateLimiterError> {
    let limiter = RateLimiter::builder()
        .quota(1000, Duration::from_secs(60)) // 1000 / minute
        .burst(50)                            // token bucket: spend at most 50 at once
        .shards(64)                           // tune for core count
        .eviction(Eviction::idle(Duration::from_secs(300)))
        .try_build()?;

    // Take more than one unit at a time.
    let decision = limiter.check_n("tenant:acme", 5);
    assert!(decision.is_allow());
    Ok(())
}
assert!(build().is_ok());
```

Algorithms other than the token bucket need the `algorithms` feature. The
window algorithms admit at most `limit` per period and ignore `burst`.

Prefer not to use the builder? The same knobs are chainable adjusters on a
constructed limiter — `RateLimiter::with_quota(quota).with_shards(64)
.with_eviction(policy).with_algorithm(algo).with_clock(clock)` — each applied
immediately after construction.

<br>

## HTTP: Retry-After and RateLimit headers

```rust
use rate_net::{RateLimiter, headers};

let limiter = RateLimiter::per_minute(2);
for _ in 0..3 {
    let decision = limiter.check("client:7");
    let mut response_headers: Vec<(&str, u64)> = limiter.peek("client:7").headers().to_vec();
    if decision.is_impossible() {
        // asked for more than the limit can ever grant: 413, not 429
    } else if let Some(secs) = decision.retry_after_secs() {
        // 429 Too Many Requests; whole seconds, rounded up
        response_headers.push((headers::RETRY_AFTER, secs));
    }
}
```

`retry_after_secs()` rounds **up** (a 1.9 s wait is `Retry-After: 2`), so a
client that honours it never comes back early to be denied again. `peek` reads
a key's allowance without spending it or creating state — `limit`, `remaining`,
and `reset_after` — for the `RateLimit-Limit`, `RateLimit-Remaining`, and
`RateLimit-Reset` headers on every response.

<br>

## Choosing keys

```rust
use rate_net::{Key, RateLimiter};
use std::net::IpAddr;

let limiter = RateLimiter::per_second(10);
let client: IpAddr = "2001:db8:aa:bb::42".parse().unwrap();

// One allowance per IPv4 address and per IPv6 /64: a host cannot rotate through
// the 2^64 addresses it owns to reset its limit.
assert!(limiter.check(Key::ip_prefix(client, 32, 64)).is_allow());

// Tagged ids never collide with string keys, even with identical bytes.
assert!(limiter.check(Key::id(42)).is_allow());
assert_ne!(Key::id(42), Key::from(42u64));
```

The `From` conversions (`&str`, `String`, bytes, `u64`, `IpAddr`, `SocketAddr`
— port dropped — …) produce *raw* keys whose identity is the bytes alone; that
is fine for a limiter holding one kind of key. When one limiter mixes kinds,
use the tagged constructors. `Key`'s `Debug` never prints the key's bytes.

<br>

## One stream, and `no_std`

`DirectLimiter` limits a single stream — client-side throttling, a shared
resource, a loop — with no key, map, hash, or lock:

```rust
use rate_net::DirectLimiter;

let limiter = DirectLimiter::per_second(50);
if limiter.check().is_allow() {
    // call the upstream API
}
```

With the `std` feature off the crate is `no_std` and offers `Gcra`, the same
lock-free leaky-bucket engine with time supplied by the caller (any monotonic
`Duration` since a fixed origin — a hardware timer, an RTOS tick):

```rust
use core::time::Duration;
use rate_net::{Gcra, Quota};

let gcra = Gcra::new(Quota::per_second(10));
let now = Duration::from_millis(1_500); // from your timer
assert!(gcra.check_at(now).is_allow());
```

`Gcra` needs only `core` and 64-bit atomics.

<br>

## Algorithms

All algorithms share the same `Limiter` trait and `check` surface — swap the
strategy through the builder without touching call sites. The token bucket is
always available; the rest are behind the `algorithms` feature.

| Algorithm | Shape | Bound per key | Good for |
|-----------|-------|---------------|----------|
| **Token bucket** | Smooth refill, bursts up to `burst` | `burst + rate × elapsed` | General-purpose; the default |
| **Leaky bucket** | Spaces units at a steady interval (GCRA) | `burst + rate × elapsed` | Shaping bursty traffic to a steady rate |
| **Fixed window** | Counter resets each window | `limit` per aligned window | Cheapest; tolerates boundary bursts (`2 × limit` across a boundary) |
| **Sliding-window log** | Exact timestamps in a window | `limit` in any rolling window | Highest accuracy; memory grows with `limit` |
| **Sliding-window counter** | Weighted blend of two windows | `≈ limit`, worst case `2 × limit` | Accuracy/cost balance; common production choice |

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

## A faster clock

Reading the OS monotonic clock is the biggest single cost of a token-bucket
check. `CoarseClock` replaces it with one atomic load, ticked every `resolution`
by a background thread (or by your own code with `tick()`):

```rust
use rate_net::{CoarseClock, RateLimiter};
use std::time::Duration;

let clock = CoarseClock::spawn(Duration::from_millis(1)).expect("spawn the ticker");
let limiter = RateLimiter::per_second(1_000).with_clock(clock.clone());
assert!(limiter.check("user:42").is_allow());
```

Readings never run ahead of real time, but they lag it, and lag has a price:
when the clock catches up, the limiter credits the lagged time as refill. Over
any span the limiter admits at most what the quota allows over that span plus
`rate × L`, where `L` is the clock's lag at the start of the span. A spawned
clock keeps `L` to about one tick (readers re-check the real clock every 64
reads, so even a starved ticker cannot let it grow); a manual clock lags by the
time since its last `tick()`.

<br>

## Awaiting (async)

The core is sync and runtime-free — `check` never sleeps. When you would rather
*wait* for a key to become allowed than shed the request, the optional `async`
feature wraps a limiter in an `AsyncLimiter`:

```rust
use rate_net::{AsyncLimiter, InsufficientCapacity, RateLimiter};

async fn handle(limiter: &AsyncLimiter) -> Result<(), InsufficientCapacity> {
    // Non-blocking, allow/deny — same as the sync API.
    let _ = limiter.check("user:42");

    // Or await until the key is within its limit. A request that can never fit
    // is an error rather than a silent "go ahead".
    limiter.try_until_ready("user:42").await?;
    Ok(())
}

let _limiter = AsyncLimiter::new(RateLimiter::per_second(100));
```

Waiting is cancel-safe: dropping the future consumes nothing. Only this layer
touches a runtime (`tokio`'s timer), and only under the `async` feature.

<br>

## Metrics

With the `metrics` feature, `limiter.metrics()` returns cumulative counters —
`allowed`, `denied`, `keys_inserted`, `keys_evicted`, `keys_expired`,
`keys_removed` — kept per shard beside the lock word a check already touches.
Without the feature the counters do not exist at all.

<br>

## Examples

Runnable, self-contained demos in [`examples/`](./examples):

```bash
cargo run --example per_second    # Tier-1: N requests/second per key
cargo run --example per_key       # per-IP and per-user, independent allowances
cargo run --example mock_clock    # deterministic refill with a ManualClock
cargo run --example retry_after   # 429 + Retry-After + RateLimit-* headers
cargo run --example gatekeeper    # per-client-network gatekeeper (IPv6 /64 keys)
cargo run --example direct        # pacing one stream with a DirectLimiter

cargo run --example algorithms --features algorithms  # compare all five algorithms
cargo run --example async_wait   --features async      # await until allowed
```

<br>

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | ✅ | Standard library: the keyed limiter, `DirectLimiter`, `CoarseClock`, and the `error-forge` integration. Off, the crate is `no_std` and offers `Gcra`, `Quota`, `Decision`, `Snapshot`, the error types, and the header names. |
| `algorithms` | ❌ | The full suite beyond the default token bucket (leaky, fixed window, sliding-window log, sliding-window counter). |
| `async`      | ❌ | `AsyncLimiter::try_until_ready` and `DirectLimiter::try_until_ready` (await until allowed). Additive — only this layer touches a runtime. |
| `metrics`    | ❌ | `metrics()` counters on `RateLimiter` and `DirectLimiter`. Without it they are compiled out. |

<br>

## Testing

```bash
# Unit + integration + property + doc tests
cargo test --all-features
cargo test --no-default-features      # the no_std core

# Concurrency model checking of the production limiter
RUSTFLAGS="--cfg loom" cargo test --test loom_check --features algorithms --release

# Benchmarks
cargo bench --bench rate_bench

# Format + lints (must be clean)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

<hr>
<br>

## Performance

Criterion medians for `v1.1.0` (Windows x86_64, Rust 1.95, `opt-level = 3`):

| Path | Median |
|------|-------:|
| Single-key check | ~66 ns |
| Single-key check on `CoarseClock` | ~35 ns |
| Many-key check (64 shards) | ~55 ns |
| Contended single key (4 threads, aggregate) | ~50 ns/op |
| New key at the default 1,048,576-key cap (insert + evict) | ~0.36 µs |
| `DirectLimiter` check / on `CoarseClock` | ~33 ns / ~8 ns |
| `peek` | ~74 ns |

```bash
cargo bench --bench rate_bench
```

A head-to-head against [`governor`](https://crates.io/crates/governor) 0.10 is
recorded honestly: on the default clock `governor` is faster (~20 ns vs ~52 ns
for a keyed check), mostly because it reads a TSC-based `quanta` clock while
rate-net's default `SystemClock` reads `Instant::now()` (~26 ns on Windows). On
`CoarseClock` the direct paths are at parity (~8.6 ns vs ~8.3 ns) and a keyed
check is within ~3.5 ns. Full method, numbers, and analysis in
[`docs/BENCHMARKS.md`](./docs/BENCHMARKS.md).

<br>

## Design

### Sharded store, bounded memory

Per-key state lives in a sharded store. A `check` is: hash the key once →
pick the shard from one set of hash bits → find the slot under the shard's read
lock using another → run the key's algorithm. Shards sit on their own cache
lines. The steady-state `check` allocates nothing; allocation happens only the
first time a key is seen.

Memory is **bounded by eviction**, which runs only while inserting a new key and
does a fixed amount of work: a CLOCK hand evicts the first key not checked since
its last pass (examining at most 32 slots), and an idle TTL is drained by an
incremental sweep (8 slots per insert).

Eviction itself is O(1), but a shard's index and slot vector still grow (and
the index occasionally rehashes) in one pass under the shard's write lock,
amortized over many inserts. To keep that pass short the store raises its shard
count until no shard's share of the capacity cap exceeds 16,384 keys, so a
growth or rehash touches at most that many entries (a few milliseconds at
worst, against ~200 ms for one 1M-key shard). An unbounded store keeps the
requested shard count; give it enough shards.

A flood of unique keys reaches the cap and stays there, and a key checked at least once per revolution of the hand
keeps its state through it. A tracked key costs about 200–300 bytes of heap
(token bucket; ~202 B measured at the default cap), so the default cap of
1,048,576 keys bounds the store at roughly 200 MiB. Size the cap to your real
key cardinality.

Eviction is lossy by nature: an evicted key that comes back starts with a fresh
allowance. An idle TTL is never allowed to cost that — it is raised to the
quota's recovery time (`try_build` rejects a shorter one), so a key is only
reclaimed once its state is indistinguishable from a fresh one.

### The no-over-admit invariant

The defining correctness property: **for any key, the admitted requests never
exceed the algorithm's bound** (table above) — under any concurrent interleaving
and on a real clock where threads routinely carry stale timestamps. Verified by:

- **`loom`** — model checks of the production `RateLimiter` (not a copy of its
  logic): racing first-touches, eviction racing inserts, and a thread that read
  the clock before another committed a later timestamp, for the token bucket,
  leaky bucket, and fixed window.
- **Real-clock contention tests** — 2, 4, and 8 threads on one key on the OS
  clock, for every algorithm, asserting the bound against the measured elapsed
  time.
- **`proptest`** — arbitrary sequences of checks and time advances per
  algorithm.

### Clean boundary

rate-net exposes a decision API — `Allow` / `Deny { retry_after }` — plus
read-only snapshots, and nothing else of its internals. Consumers (e.g. a
gatekeeper like `bouncer-io`) call `check` and never reach into rate-net's
per-key state.

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

**Tested in CI on every push:**
- ✅ Linux x86_64 (stable and the 1.85 MSRV)
- ✅ macOS Apple Silicon (stable and MSRV)
- ✅ Windows x86_64 (stable and MSRV)
- ✅ 32-bit Linux (`i686-unknown-linux-gnu`) build check
- ✅ `no_std` build for `thumbv7em-none-eabihf`

Linux aarch64 and macOS x86_64 are expected to work (the code has no
platform-specific paths) but are not exercised by CI.

<br><br>

## Contributing

Contributions are welcome. Before opening a PR, make sure `cargo fmt`,
`cargo clippy --all-targets --all-features -- -D warnings`, and
`cargo test --all-features` are all clean. Any new algorithm must arrive with
its own `proptest` over-admit proof, a real-clock contention test, and
retry-after tests; any change to the shard or eviction path needs a `loom` model
and a benchmark.

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
