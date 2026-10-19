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
        Per-key state lives in a <b>sharded store</b>: an existing key costs one shard read lock plus the algorithm's atomic accounting, so unrelated keys rarely contend. Memory is <b>bounded</b> by O(1) eviction: a flood of unique keys hits a cap instead of growing without limit, eviction never scans the store, and evicting a still-limited key <b>keeps its debt instead of resetting its limit</b> — by default however large the flood, at the cost of refusing some newcomers once the store is far past its cap (the opt-in <code>Overflow::Forget</code> keeps newcomers admitted and resets the overflow instead) — the difference between a rate limiter and a denial-of-service vector you built yourself.
    </p>
    <p>
        The common case is one line — <code>RateLimiter::per_second(100)</code> then <code>limiter.check(key)</code>. Power users pick the algorithm, quota, burst, shard count, eviction policy, and clock through a validating builder. rate-net does not reimplement token-bucket math; it consumes <a href="https://crates.io/crates/better-bucket"><code>better-bucket</code></a> for that and adds the per-key, multi-algorithm, retry-after machinery around it.
    </p>
</div>

<br>

> **Stable (`v2.0.0`).** The public API is frozen until `3.0`; each `2.x` only
> adds to it. Five algorithms (token bucket by default; leaky bucket, fixed
> window, sliding-window log, and sliding-window counter under the `algorithms`
> feature) behind one `Limiter` trait; a validating builder with per-key
> quotas, key-length limits, and long-key digests; `peek` snapshots and
> `Retry-After` / `RateLimit` header helpers; `tower` middleware; key
> management; an un-keyed `DirectLimiter` and a `no_std` `Gcra` core; a fast
> `CoarseClock`; optional metrics and an async layer with bounded waits.
> `2.0.0` closes the last eviction loophole — an evicted, still-limited key
> keeps its debt instead of coming back with a fresh allowance, and by default
> no flood can ever reset a key (`Overflow::FailClosed`) — and cuts per-key
> memory by a third. Upgrading from `1.x`: see
> [the migration guide](./docs/MIGRATION_1_TO_2.md) and the
> [CHANGELOG](./CHANGELOG.md).

<br>

<h2>Why rate-net</h2>

Rate limiting looks trivial until production traffic finds the edges — the over-admit race when two threads cross a millisecond, the unbounded key map that OOMs under a unique-key flood, the eviction pass that stalls every request behind it, the API so generic you write a wrapper just to limit a loop. rate-net is built against all of them:

- **Atomic accounting, sharded locking.** The token bucket, leaky bucket, and fixed window account each key with a single compare-and-swap; the sliding-window algorithms hold a short per-key lock. Keys are found under a per-shard read lock, so checks of unrelated keys proceed in parallel.
- **Never over-admits.** Admissions for a key never exceed its algorithm's documented bound, under any interleaving of threads, on a real clock. Checked by `loom` against the production code, property-tested per algorithm, and regression-tested by many threads hammering one key on the OS clock.
- **Bounded memory, O(1) eviction.** A capacity cap evicts by CLOCK (second chance) with a bounded scan, and an idle TTL drains incrementally. Capped shards are kept to 16,384 keys, so even the occasional amortized index growth stays short. A token-bucket key costs ~138 bytes; key bytes can be capped or digested.
- **Eviction does not reset a limit.** A key evicted before it recovered has its debt recorded and resumes no more permissive than it left — exactly, and without affecting any other key, while the still-limited evicted keys fit about twice the cap. Past that, the default `Overflow::FailClosed` keeps them all, so no flood can ever reset a key, at the cost of charging or refusing some keys that owed nothing; `Overflow::Forget` is the availability-first opt-out that drops the debts that do not fit, as 1.x did for every key. A recovery-load gauge, a saturation counter, and rate-limited warnings tell you before either happens. See [Choosing an overflow policy](#choosing-an-overflow-policy) and [Sizing the cap](#sizing-the-cap-and-running-it).
- **Five algorithms, one trait.** Token bucket, leaky bucket (GCRA), fixed window, sliding-window log, sliding-window counter — swap the strategy without touching call sites.
- **HTTP-ready.** Denials carry an honest `retry-after`; `retry_after_secs()` rounds it **up** for the header, `peek` feeds the `RateLimit-*` headers (and the current IETF draft's structured `RateLimit` / `RateLimit-Policy` fields) on every response, and an optional `tower` layer does all of it in front of any service.
- **Safe keys.** Tagged keys (`Key::id`, `Key::ip`) cannot collide with attacker-chosen strings, and `Key::ipv6_prefix(addr, 64)` stops a client rotating through its own `/64`.
- **One-line API.** `RateLimiter::per_second(n)` then `.check(key)`. The simple path is also the fast path.

<br>

## Features

- **Multi-algorithm** — token bucket, leaky bucket, fixed window, sliding-window log, sliding-window counter, all behind one `Limiter` trait (and the object-safe `DynLimiter`)
- **Per-key limiting** — per-IP, per-subnet, per-user, per-endpoint, per-anything; sharded so keys rarely contend
- **Direct limiting** — `DirectLimiter` for one stream: no key, no map, no shard lock
- **`no_std` core** — `Gcra`, a clock-free single-key limiter driven by caller-supplied time
- **Validated configuration** — `Builder::try_build` rejects zero periods, zero bursts, out-of-range rates, and idle TTLs that would reset a key early; `build` fails closed
- **Tiered limits** — `Builder::quota_for` gives chosen keys (paid tenants, internal callers) their own quota in one limiter
- **Bounded memory** — O(1) CLOCK eviction plus an incremental idle sweep; survives a hostile unique-key flood without resetting anyone's limit
- **Long and untrusted keys** — `check_bytes` checks a key of any length without allocating; `max_key_len` refuses over-long keys and `digest_keys_longer_than` stores long ones as a 16-byte keyed digest
- **Retry-After and RateLimit headers** — rounded-up `Retry-After`, `peek` snapshots for `RateLimit-Limit` / `-Remaining` / `-Reset`, and the structured `RateLimit` / `RateLimit-Policy` fields
- **`tower` middleware** — `RateLimitLayer` with peer-IP, `X-Forwarded-For`, header, and closure key extractors, answering `429` with the headers (feature `tower`)
- **Key management** — `peek`, `reset`, `remove`, `clear`, `retain_recent`, `shrink_to_fit`
- **Fast clock** — `CoarseClock` replaces the OS clock read on the check path with one atomic load
- **Deterministic tests** — inject a mockable clock (via `clock-lib`); advance windows without `sleep`
- **Metrics** — allowed / denied / inserted / evicted / expired / removed / remembered / restored / merged / rejected counters, compiled out unless the `metrics` feature is on
- **Sync core** — no async runtime dependency; an optional additive async layer with deadline-bounded waits and retry jitter

<hr>
<br>

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
rate-net = "2"

# Full algorithm suite, async waiting, metrics, saturation warnings, and tower middleware:
rate-net = { version = "2", features = ["algorithms", "async", "metrics", "tracing", "tower"] }
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

The current revision of the IETF draft replaces those three with two
structured fields; both format without allocating:

```rust
use clock_lib::ManualClock;
use rate_net::{headers, RateLimiter};
use std::sync::Arc;

let clock = Arc::new(ManualClock::new()); // frozen, so the numbers are exact
let limiter = RateLimiter::per_minute(60).with_clock(clock);
let _ = limiter.check_n("client:7", 2);

// RateLimit-Policy: "default";q=60;w=60
let policy = limiter.quota().ratelimit_policy("default").to_string();
// RateLimit: "default";r=58;t=2
let current = limiter.peek("client:7").ratelimit("default").to_string();

assert_eq!((headers::RATELIMIT_POLICY, policy.as_str()), ("RateLimit-Policy", r#""default";q=60;w=60"#));
assert_eq!((headers::RATELIMIT, current.as_str()), ("RateLimit", r#""default";r=58;t=2"#));
```

<br>

## Middleware (`tower`, axum, hyper)

With the `tower` feature, `RateLimitLayer` puts a limiter in front of any
`tower::Service` over `http` requests. A `KeyExtractor` picks who each request
is charged to — `PeerIp` (the connection's address from the request
extensions), `ForwardedFor::trusting(n)` (the client address `n` trusted
proxies report), `HeaderKey` (an API key), or any closure. Denied requests
never reach the inner service: they get a `429` with `Retry-After` and the
rate-limit headers.

`ForwardedFor` reads `X-Forwarded-For`, which the client can write. It keys
by the address the trusted proxies appended only if every request reaches the
service through them; if the service is also reachable directly, add
`.only_from(network, prefix_len)` so the header is used only when the
connection's peer is in the proxies' network, and the peer itself is the key
otherwise. `HeaderKey` stores a value longer than 64 bytes as a 16-byte keyed
digest, so a client cannot fill memory with huge header values.

```rust
use std::convert::Infallible;
use http::{Request, Response, StatusCode};
use rate_net::RateLimiter;
use rate_net::tower::{ForwardedFor, RateLimitLayer};
use tower::{Layer, Service, ServiceExt, service_fn};

async fn serve() -> StatusCode {
    let layer = RateLimitLayer::new(RateLimiter::per_minute(100), ForwardedFor::trusting(1))
        .with_policy("default");
    let mut service = layer.layer(service_fn(|_: Request<String>| async {
        Ok::<_, Infallible>(Response::new(String::from("ok")))
    }));

    let request = Request::get("/")
        .header("x-forwarded-for", "203.0.113.7")
        .body(String::new())
        .unwrap();
    let response = service.ready().await.unwrap().call(request).await.unwrap();
    response.status() // 200, or 429 with Retry-After once the client is over
}
let _ = serve; // run it on your Tokio runtime
```

With axum, add the layer to a `Router` (`.layer(layer)`) and read the peer
address from axum's `ConnectInfo` with a closure extractor. By default,
requests the extractor cannot identify share one anonymous key rather than
going unlimited; `.with_anonymous(Anonymous::Key(key))` charges them to a key
of your choice (which can have its own `quota_for` tier) and
`.with_anonymous(Anonymous::Reject(status))` answers them without charging
anything. `.with_cost(n)` charges `n` units a request (`0` counts as `1`), and
the `RateLimit-Policy` header describes the quota the key is actually under.

<br>

## Tiered limits and untrusted keys

```rust
use rate_net::{Quota, RateLimiter};

let limiter = RateLimiter::builder()
    .per_second(10)                     // the default tier
    .quota_for(|key| {                  // consulted once, when a key is first stored
        key.as_bytes().starts_with(b"pro:").then(|| Quota::per_second(1_000))
    })
    .max_key_len(256)                   // refuse keys longer than 256 bytes outright
    .digest_keys_longer_than(32)        // store longer ones as a 16-byte keyed digest
    .build();

assert!(limiter.check_n("pro:acme", 500).is_allow());
assert!(limiter.check_n("free:bob", 11).is_impossible());

// A long key (an API token, a JWT subject) checked without allocating:
let token = "9f6c1d2e-8b7a-4c3d-9e1f-0a2b3c4d5e6f-a1b2c3d4";
assert!(limiter.check_bytes(token).is_allow());
```

`check(key)` copies a key longer than 22 bytes into a new allocation on every
call; `check_bytes` borrows it, so the check of an existing key allocates
nothing whatever its length.

The `quota_for` resolver runs on the caller's thread before the limiter takes
any lock, once for each key being stored (never for a tracked key). It may call
back into the limiter for *other* keys, but must not check the key it is
resolving; it should be fast and deterministic. An override that `try_build`
would reject (a zero limit, an over-fast leaky bucket, a sliding-log limit
above `MAX_SLIDING_LOG_LIMIT`) makes its key fail closed. `quota_of(key)`
reports the quota a key is under. The digest is two independently, randomly seeded
64-bit hashes — a caller cannot aim a long key at someone else's limit — but it
is not a cryptographic hash.

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
resource, a loop — with no key, map, hash, or shard lock (the token bucket's
check is one compare-and-swap):

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
clock keeps `L` to about one tick (readers re-check the real clock every 61
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

Waiting is cancel-safe: dropping the future consumes nothing. With a latency
budget, `until_ready_within(key, max_wait)` sleeps only while the next
admission falls inside the budget and otherwise returns the denial at once; and
`with_jitter(max)` adds a random delay to each sleep so tasks denied together do
not all retry together. Only this layer touches a runtime (`tokio`'s timer, so
it must run inside a Tokio runtime), and only under the `async` feature.

<br>

## Metrics

With the `metrics` feature, `limiter.metrics()` returns cumulative counters —
`allowed`, `denied`, `keys_inserted`, `keys_evicted`, `keys_expired`,
`keys_removed`, `keys_remembered` (evicted keys whose debt was recorded),
`keys_restored` (new keys charged a recorded debt), `debts_forgotten` (debts
dropped past the exact recovery table under `Overflow::Forget` — those keys may
come back with a fresh allowance), `debts_overflowed` (debts spilled into the
shared sketch under `Overflow::FailClosed`, which can charge keys that never
owed anything), `keys_refused` (newcomers refused because a full shard held
only still-limited keys whose debts could not be kept), and `keys_rejected`
(over-long keys) — kept per shard beside the lock word a check already
touches. Any sustained `debts_forgotten`, `debts_overflowed`, or
`keys_refused` means the cap is too small for the traffic. Without the feature the counters do not
exist at all.

`limiter.recovery_load()` (also `metrics`) is the gauge to alert on *before*
any of that happens: how many evicted keys' debts the store holds against the
exact capacity it has for them, overall and in the fullest shard. It walks the
recovery tables, so export it every few seconds, not per request. With the
`tracing` feature the limiter logs a rate-limited warning (target
`rate_net::eviction`) on the first spill, drop, or refusal and once a minute
while they continue; nothing on the check path changes.

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

cargo run --example algorithms  --features algorithms  # compare all five algorithms
cargo run --example async_wait  --features async       # await until allowed
cargo run --example tower_layer --features tower       # 429s from tower middleware
```

<br>

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | ✅ | Standard library: the keyed limiter, `DirectLimiter`, and `CoarseClock`. Off, the crate is `no_std` and offers `Gcra`, `Quota`, `Decision`, `Snapshot`, the error types, and the header names. |
| `algorithms` | ❌ | The full suite beyond the default token bucket (leaky, fixed window, sliding-window log, sliding-window counter). |
| `async`      | ❌ | `AsyncLimiter` and `DirectLimiter::try_until_ready` / `until_ready_within` (await until allowed, optionally bounded and jittered). Additive — only this layer touches a runtime. |
| `metrics`    | ❌ | `metrics()` counters on `RateLimiter` and `DirectLimiter`, and the `recovery_load()` saturation gauge. Without it they are compiled out. |
| `tracing`    | ❌ | Rate-limited `tracing` warnings when evicted keys' debts spill, are dropped, or newcomers are refused. Nothing on the check path. Adds `tracing` (without proc-macros). |
| `forge`      | ❌ | `error_forge::ForgeError` on the error types, for the portfolio error stack. Adds `error-forge`. |
| `tower`      | ❌ | `rate_net::tower`: `RateLimitLayer`, key extractors, `429` responses. Adds `http`, `tower-layer`, `tower-service`, `pin-project-lite` — not `tower` itself. |

<br>

## Testing

```bash
# Unit + integration + property + doc tests
cargo test --all-features
cargo test --no-default-features      # the no_std core

# Concurrency model checking of the production limiter (clock-lib 1.1.0 does
# not build under --cfg loom, so pin 1.0.0 for this run, as CI does)
cargo update -p clock-lib --precise 1.0.0
RUSTFLAGS="--cfg loom" cargo test --test loom_check --features algorithms --release
git checkout Cargo.lock

# Fuzzing (nightly + cargo-fuzz): the limiter against an oracle, and the HTTP
# surface. The corpus directories are not committed; create them first.
mkdir -p fuzz/corpus/limiter_ops fuzz/corpus/http_keys
cargo +nightly fuzz run limiter_ops fuzz/corpus/limiter_ops fuzz/seeds/limiter_ops
cargo +nightly fuzz run http_keys fuzz/corpus/http_keys fuzz/seeds/http_keys

# Benchmarks: per-check latency, and legitimate-key tail latency under a flood
cargo bench --bench rate_bench
cargo bench --bench flood

# Format + lints (must be clean)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

<hr>
<br>

## Performance

Criterion medians for `v2.0.0`, per check (Windows 11 x86_64, 32 logical CPUs,
Rust 1.99, `opt-level = 3`, fat LTO). The machine was shared with other builds
while these ran, so treat a few nanoseconds either way as noise; the method and
the full table are in [`docs/BENCHMARKS.md`](./docs/BENCHMARKS.md).

| Path | v1.1.0 | v2.0.0 |
|------|-------:|-------:|
| Single-key check | 60.6 ns | 59.1 ns |
| Single-key check on `CoarseClock` | 36.3 ns | 37.5 ns |
| Many-key check (10,000 keys, 64 shards) | 64.0 ns | 55.4 ns |
| One key, 4 threads contending (per check) | 223 ns | 229 ns |
| 36-byte key via `check` / via `check_bytes` | 87.2 ns / — | 80.9 ns / **52.4 ns** |
| New key at the default 1,048,576-key cap (insert + evict) | 293 ns | 351 ns |
| `DirectLimiter` check / on `CoarseClock` | 38.1 ns / 12.2 ns | 37.2 ns / 11.1 ns |
| `peek` | 80.4 ns | 54.1 ns |

An insert at the cap costs ~58 ns more than in `1.1.0` (see
[Known issues](#known-issues)): the eviction now reads each candidate's state
so that it does not evict and reset a still-limited key.

Under a unique-key flood (three threads inserting new keys as fast as they can,
the default limiter at its 1,048,576-key cap), a legitimate key's check latency
barely moves:

| Scenario | p50 | p99 | p99.9 |
|----------|----:|----:|------:|
| no flood | 100 ns | 100 ns | 300 ns |
| flood of keys that recover at once | 100 ns | 300 ns | 500–600 ns |
| flood of keys that each drain their quota (default `Overflow::FailClosed`) | 100 ns | 600 ns | 1.2 µs |
| the same, `Overflow::Forget` | 100 ns | 400 ns | 1.3 µs |

(`Instant` resolution on Windows is 100 ns.)

`1.0.0` scanned a whole shard under its write lock on every flood insert; the
audit measured a legitimate check's p99.9 rising from 0.3 µs to 128 µs under the
same flood.

A head-to-head against [`governor`](https://crates.io/crates/governor) 0.10 is
recorded honestly in the benchmarks document: on the default clock `governor`
is faster, mostly because it reads a TSC-based `quanta` clock while rate-net's
default `SystemClock` reads `Instant::now()` (~26 ns on Windows); on
`CoarseClock` the direct paths are at parity.

<br>

## Known issues

- **Inserting a new key into a full store is ~58 ns slower than in 1.1.0.**
  When the key table is at its cap, inserting a never-seen key costs about
  +58 ns (≈293 → 351 ns at the default 1,048,576-key cap; criterion medians on
  a loaded Windows host, best of runs, so approximate), because eviction checks
  that each candidate has fully recovered before evicting it — the price of
  never resetting a still-limited key. Checks of keys already tracked are
  unaffected, and below the cap nothing changes. It is to be optimised in a
  future release and is a blocker for the next minor release, 2.1.0.
- **Past about twice the cap of still-limited evictions, one side gives.**
  Debts of evicted, still-limited keys are kept exactly for about twice the
  cap's worth of such keys at once (what counts is the rate at which they are
  evicted times their recovery time). Beyond that, the default
  `Overflow::FailClosed` keeps them, so keys that never owed anything may
  start drained or be refused with a retry time: no false denials up to a
  3x-cap drain flood, about 3 % of newcomers denied after 4x, 38 % after 6x,
  86 % after 9x; a well-behaved population of 10x the cap sees about 4 %
  false denials at 5,000 visits a second but **72 % at 20,000**
  (`docs/BENCHMARKS.md`). The opt-in `Overflow::Forget` drops those debts
  instead, so those keys may come back with a fresh allowance — a sustained
  unique-key flood can reset a targeted key over and over. See
  [Choosing an overflow policy](#choosing-an-overflow-policy) and
  [Sizing the cap](#sizing-the-cap-and-running-it): size the cap so this
  never happens, and alert on `recovery_load()` and `keys_refused` (feature
  `metrics`) before it does.
- **Under `FailClosed`, a sketch charge is shared**: `reset` and `remove`
  cannot clear it for one key (it expires within one recovery period), and a
  slow tier's long deadline can drain a fast-tier newcomer for that long.

<br>

## Design

### Sharded store, bounded memory

Per-key state lives in a sharded store. A `check` is: hash the key once →
pick the shard from one set of hash bits → find the slot under the shard's read
lock using another → run the key's algorithm. Shards sit on their own cache
lines. Checking an already-tracked key allocates nothing when the key is
borrowed (`check_bytes`) or at most 22 bytes; otherwise allocation happens only
the first time a key is seen (a `check` of an owned key longer than 22 bytes
allocates the `Key` itself on every call — use `check_bytes`). A sliding-log
key's run-length log grows amortized, so it occasionally allocates as well.

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

A flood of unique keys reaches the cap and stays there, and a key checked at
least once per revolution of the hand keeps its state through it. Each key's
state is stored typed, with no enum tag: a token-bucket key is 128 bytes in its
shard, about 138 bytes of heap per key at the default cap (measured by
`tests/memory.rs`; up to ~210 bytes just after a shard's tables double), so the
default cap of 1,048,576 keys bounds the per-key state at roughly 140 MiB.
Under eviction pressure the recovery tables add 64 MiB (`Overflow::FailClosed`,
the default) or 32 MiB (`Overflow::Forget`) at the default cap — about 202 or 170 MiB
in all — plus the bytes of keys longer than 22 bytes (cap them with
`max_key_len`, or digest them). Size the cap to your real key cardinality.

### Eviction does not reset a limit

Evicting a key that is still being limited would hand it a fresh allowance when
it returns — a way to reset anyone's limit by flooding the store with throwaway
keys. So the CLOCK hand prefers keys that have fully recovered (forgetting them
loses nothing), and when it must evict one that has not, its shard records the
debt — units spent, and when they would all be back. A key that returns before
its debt is repaid resumes no more permissive than it left: exactly where it
was for the buckets; with the spent units charged at the return for the window
algorithms (held back up to one window longer).

The debts go into a cuckoo-hashed table of 16-byte records, two per key of
capacity, allocated on first use. A key reads back only its own record, so
while a shard's still-limited evicted keys fit — about twice its share of the
cap at once — no evicted key is reset and no other key is ever affected. An
idle TTL is raised to the quota's recovery time (`try_build` rejects a shorter
one), so idle expiry normally removes only recovered keys; a key under a
slower per-key quota that expires anyway is judged by its current state and
has its debt recorded too.

#### Choosing an overflow policy

Past that bound, `Eviction::with_overflow` decides what gives:

| | `Overflow::FailClosed` (default) | `Overflow::Forget` (opt-out) |
|---|---|---|
| A debt that does not fit | spills into a shared sketch (four 8-byte cells per key of capacity) | is dropped |
| Evicted, still-limited keys | never come back looser than they left | may come back with a fresh allowance, as in 1.x |
| Keys that never owed anything | may start partly or wholly drained, or be refused with a retry time | never charged, never refused |
| Drain flood of 4x / 6x / 9x the cap: newcomers denied | 3 / 38 / 86 % | 0 / 0 / 0 % |
| Well-behaved population of 10x the cap, 5,000 / 20,000 visits a second | 4 / 72 % false denials | 0 / 0 % false denials |
| Recovery tables at the default cap | 64 MiB | 32 MiB |

Measured with the re-audit's harness (ManualClock, 128 shards; cap
100,000 for floods, 10,000 for populations). Under `FailClosed`, a key charged
from the sketch never writes that charge back, so it cannot spread or ratchet,
and a spent count beyond the sketch's 24 bits is charged as everything.

**`FailClosed` is the default** — secure by default, and the right choice for
limiters keyed by identities an attacker can target (usernames, API keys,
account ids: login attempts, password resets, paid quotas), where resetting a
key is the attack and a flood that locks out newcomers is the lesser harm.
**Opt into `Forget`** for load shedding in front of a service, keyed by client
address, where keeping newcomers admitted under a flood matters more than a
perfect memory of every evicted key:

```rust
use rate_net::{Eviction, Overflow, RateLimiter};

// Anonymous per-IP load shedding: availability first.
let shedding = RateLimiter::builder()
    .per_second(50)
    .eviction(Eviction::capacity(1_000_000).with_overflow(Overflow::Forget))
    .build();
assert!(shedding.check("203.0.113.7").is_allow());
```

#### Sizing the cap, and running it

The overflow policy only matters when **more still-limited keys are evicted
within one recovery period than about twice the cap** — that is, when more
than roughly three times the cap's worth of keys are limited at once. Below
that, both policies keep every debt exactly, no key is ever reset, and no
legitimate key is ever charged or refused. So the first defence is the cap:

- **Sizing rule:** `cap >= peak distinct keys active within the longest
  recovery window × headroom` (say 1.5–2x). The recovery window is how long a
  drained key takes to be whole again: `burst / limit` periods for the token
  and leaky buckets, one period for the fixed window and the sliding-window
  log, two for the sliding-window counter. A login limiter at 5 per 15
  minutes that sees 200,000 distinct usernames in any 15 minutes wants a cap
  of 300,000–400,000.
- **Memory budget:** a token-bucket key is about 138 bytes of state at the
  default cap, and under eviction pressure the recovery tables add 64 bytes
  per key of capacity with `FailClosed` (about 202 bytes in all) or 32 with
  `Forget` (about 170). The default 1,048,576-key cap is about 140 MiB idle and
  202 MiB under pressure; plus the bytes of keys longer than 22.
- **Operations checklist:**
  1. Size the cap with the rule above, and re-check it when traffic grows.
  2. Export `limiter.recovery_load()` (feature `metrics`) every few seconds
     and alert when `utilization()` or `fullest_shard` passes about 0.7 —
     that is before anything spills, is dropped, or is refused.
  3. Alert on any rate of `keys_refused` (the saturation counter: legitimate
     newcomers refused under `FailClosed`), `debts_overflowed`, or
     `debts_forgotten`; with the `tracing` feature the limiter also logs a
     warning (target `rate_net::eviction`) on the first event and once a
     minute while they continue.
  4. Keep the default `FailClosed` for limiters keyed by identities or
     credentials (usernames, API keys, accounts) — a reset is the attack there.
  5. Consider `Forget` for anonymous per-IP load shedding, where keeping
     newcomers admitted under a flood matters more than strictness.
  6. Keep an upstream DDoS layer (CDN, load balancer, SYN cookies): an
     in-process limiter bounds its own memory and keeps its promises, but it
     cannot absorb a volumetric attack on the machine it runs on.
- **How other limiters behave, to map your expectations:**
  - **NGINX `limit_req`** keeps state in a fixed-size shared-memory zone. When
    the zone is full it removes the least recently used state — even a client
    that is still being limited, which then starts fresh (like `Forget`) — and
    if a new state still cannot be created, the request is rejected with an
    error. Size the zone, as you size the cap.
  - **`governor`**'s keyed limiters keep state in a map that grows without
    bound; nothing is evicted unless you call `retain_recent()` (which drops
    only keys that have fully recovered). A key is never reset, but a
    unique-key flood grows memory until you sweep. rate-net bounds memory
    by default and makes the reset-or-deny choice explicit instead.


### The no-over-admit invariant

The defining correctness property: **for any key, the admitted requests never
exceed the algorithm's bound** (table above) — under any concurrent interleaving
and on a real clock where threads routinely carry stale timestamps. Verified by:

- **`loom`** — model checks of the production `RateLimiter` (not a copy of its
  logic): racing first-touches, eviction racing inserts, a drained key evicted
  while it is being checked, and a thread that read the clock before another
  committed a later timestamp, for the token bucket, leaky bucket, and fixed
  window.
- **Real-clock contention tests** — 2, 4, and 8 threads on one key on the OS
  clock, for every algorithm, asserting the bound against the measured elapsed
  time.
- **`proptest`** — arbitrary sequences of checks and time advances per
  algorithm, and eviction under pressure never letting a key exceed its bound.
- **Fuzzing** — arbitrary operation sequences (checks, clock advances, resets,
  removals, sweeps) against a token-bucket oracle on a heavily evicting store.

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

**Tested in CI on every pull request and every push to `main` and `dev`:**
- ✅ Linux x86_64 (stable and the 1.85 MSRV)
- ✅ macOS Apple Silicon (stable and MSRV)
- ✅ Windows x86_64 (stable and MSRV)
- ✅ 32-bit Linux (`i686-unknown-linux-gnu`), full test suite
- ✅ `no_std` build for `thumbv7em-none-eabihf`

Linux aarch64 and macOS x86_64 are expected to work (the code has no
platform-specific paths) but are not exercised by CI.

<br><br>

## Development

The repository pins its toolchain and its dependency versions so every
contributor and every CI job builds the same thing:

- `rust-toolchain.toml` selects Rust **1.99.0** with `rustfmt` and `clippy` for
  local work. It is not packaged, so it never affects users of the crate.
- `Cargo.lock` is committed. CI builds, tests, audits (`cargo audit`), and
  checks licenses and advisories (`cargo deny`) against exactly that lock, and
  it resolves on the 1.85 MSRV.
- Every CI job that claims a toolchain (MSRV, stable, nightly) selects it
  explicitly and prints `rustc -V`. A separate *latest dependencies* job runs
  weekly and on every pull request: it runs `cargo update`, then builds, tests,
  lints, and audits against the newest versions — on 1.99.0 the true latest
  (whatever their `rust-version`), on 1.85 the newest that still support the
  MSRV — so a breaking upstream release shows up before it reaches users.
- Every non-dev dependency is justified in writing (purpose, in-house
  alternative, cost, verdict) before it is added.

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
