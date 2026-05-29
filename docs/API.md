<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>rate-net</b><br>
    <sub><sup>API REFERENCE</sup></sub>
</h1>
<div align="center">
    <sup>
        <a href="../README.md" title="Project Home"><b>HOME</b></a>
        <span>&nbsp;│&nbsp;</span>
        <span>API</span>
        <span>&nbsp;│&nbsp;</span>
        <a href="../CHANGELOG.md" title="Changelog"><b>CHANGELOG</b></a>
    </sup>
</div>
<br>

> Complete reference for every public item in `rate-net`, with examples. The
> format mirrors the portfolio standard
> ([metrics-lib API.md](https://github.com/jamesgober/metrics-lib/blob/main/docs/API.md)).
>
> **Status: pre-1.0 (`v0.6.0`).** Feature-frozen and optimized. Five algorithms
> behind one [`Limiter`](#limiter-trait) trait, the Tier-2 [`Builder`](#builder),
> an optional [`AsyncLimiter`](#asynclimiter) await-until-ready layer, runnable
> [examples](https://github.com/jamesgober/rate-net/tree/main/examples), and a
> [benchmark suite](./BENCHMARKS.md) (with an honest head-to-head vs `governor`) —
> over a sharded, bounded-memory, allocation-free core. The leaky bucket and
> window algorithms require the `algorithms` feature; `AsyncLimiter` requires
> `async`. Everything documented here is callable now and the public surface is
> stable through the `0.x` hardening releases.

## Table of Contents

- [Overview](#overview)
- [Public API](#public-api)
  - [`RateLimiter`](#ratelimiter)
    - [`per_second`](#ratelimiterper_second)
    - [`per_minute`](#ratelimiterper_minute)
    - [`with_quota`](#ratelimiterwith_quota)
    - [`with_clock`](#ratelimiterwith_clock)
    - [`with_shards`](#ratelimiterwith_shards)
    - [`with_eviction`](#ratelimiterwith_eviction)
    - [`with_algorithm`](#ratelimiterwith_algorithm)
    - [`check`](#ratelimitercheck)
    - [`check_n`](#ratelimitercheck_n)
    - [`quota` / `algorithm` / `shards` / `eviction` / `tracked_keys`](#ratelimiter-introspection)
  - [`Builder`](#builder)
  - [`AsyncLimiter`](#asynclimiter) _(feature: `async`)_
  - [`Limiter` trait](#limiter-trait)
  - [`Decision`](#decision)
  - [`Quota`](#quota)
  - [`Eviction`](#eviction)
  - [`Algorithm`](#algorithm)
  - [`RateLimiterError`](#ratelimitererror)
  - [`Key`](#key)
  - [`VERSION`](#version)
- [Sharding and eviction](#sharding-and-eviction)
- [Tier 2 — the configured path](#tier-2--the-configured-path)
- [Algorithms](#algorithms)
- [Feature flags](#feature-flags)

---

## Overview

`rate-net` answers "is this key allowed right now?" with a [`Decision`](#decision)
(`Allow` / `Deny { retry_after }`), tracking an independent allowance per key.
The common case is a constructor plus `check`:

```rust
use rate_net::{RateLimiter, Decision};

let limiter = RateLimiter::per_second(100);
match limiter.check("user:42") {
    Decision::Allow => { /* serve */ }
    Decision::Deny { retry_after } => { let _ = retry_after; /* 429 + Retry-After */ }
    _ => {}
}
```

The core guarantee: **for any key and window, admitted requests never exceed the
configured quota.** The token-bucket accounting is delegated to
[`better-bucket`](https://crates.io/crates/better-bucket); time is read from an
injectable [`clock-lib`](https://crates.io/crates/clock-lib) clock.

---

## Public API

### `RateLimiter`

```rust
pub struct RateLimiter<C: Clock + Clone = SystemClock> { /* private */ }
```

A keyed rate limiter. It tracks a separate allowance for every key it sees and
answers `check` in the time it takes to hash the key and run its bucket. It is
`Send + Sync` and is meant to be shared — behind an `Arc`, or as a `static` —
across every thread serving requests; `check` takes `&self`, so no `&mut` or
external lock is needed.

The type parameter `C` is the clock source. It defaults to `SystemClock` (the OS
monotonic clock); tests inject a `ManualClock` via [`with_clock`](#ratelimiterwith_clock).
Its `Debug` impl deliberately prints only the quota, algorithm, and live key
count — never the keys themselves, which can be caller identities.

#### `RateLimiter::per_second`

```rust
pub fn per_second(limit: u32) -> RateLimiter<SystemClock>
```

A limiter allowing `limit` requests per second, per key. The headline Tier-1
constructor.

- `limit` — requests admitted per second per key. `0` yields a limiter that
  denies every request (use [`Quota::rate`](#quota) when you want `0` rejected
  as an error).

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(100);
assert_eq!(limiter.quota().limit(), 100);
```

#### `RateLimiter::per_minute`

```rust
pub fn per_minute(limit: u32) -> RateLimiter<SystemClock>
```

A limiter allowing `limit` requests per minute, per key. Same zero-limit
semantics as `per_second`.

```rust
use rate_net::RateLimiter;
use std::time::Duration;

let limiter = RateLimiter::per_minute(600);
assert_eq!(limiter.quota().period(), Duration::from_secs(60));
```

#### `RateLimiter::with_quota`

```rust
pub fn with_quota(quota: Quota) -> RateLimiter<SystemClock>
```

A limiter built from an explicit [`Quota`](#quota). Pair it with
[`Quota::rate`](#quota) when the window is neither a second nor a minute.

- `quota` — the per-key rate the limiter enforces.

```rust
use rate_net::{RateLimiter, Quota};
use std::time::Duration;

// 5 requests per 100ms, per key.
let quota = Quota::rate(5, Duration::from_millis(100))?;
let limiter = RateLimiter::with_quota(quota);
assert_eq!(limiter.quota().limit(), 5);
# Ok::<(), rate_net::RateLimiterError>(())
```

#### `RateLimiter::with_clock`

```rust
pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> RateLimiter<C2>
```

Replaces the limiter's time source, discarding any per-key state. The
clock-injection seam — inject a `ManualClock` (wrapped in `Arc`) to drive refill
deterministically with no `sleep`.

- `clock` — the new time source. Any `clock_lib::Clock` that is also `Clone`;
  `Arc<ManualClock>` and `SystemClock` both qualify.

```rust
use rate_net::RateLimiter;
use clock_lib::ManualClock;
use std::sync::Arc;
use std::time::Duration;

let clock = Arc::new(ManualClock::new());
let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));

for _ in 0..5 {
    assert!(limiter.check("k").is_allow());
}
assert!(limiter.check("k").is_deny());

clock.advance(Duration::from_secs(1)); // no real sleep
assert!(limiter.check("k").is_allow());
```

#### `RateLimiter::with_shards`

```rust
pub fn with_shards(self, shards: usize) -> Self
```

Sets the shard count, discarding any per-key state. Intended immediately after
construction. More shards reduce contention between unrelated keys; the value is
rounded up to a power of two. See [Sharding and eviction](#sharding-and-eviction).

- `shards` — the desired shard count (rounded up to a power of two). A small
  multiple of the core count is a good starting point.

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(1000).with_shards(64);
assert_eq!(limiter.shards(), 64);
```

#### `RateLimiter::with_eviction`

```rust
pub fn with_eviction(self, eviction: Eviction) -> Self
```

Sets the [eviction policy](#eviction), discarding any per-key state. Intended
immediately after construction. The default bounds memory with a generous
capacity cap; override it to tune the cap or add an idle TTL.

- `eviction` — the [`Eviction`](#eviction) policy.

```rust
use rate_net::{RateLimiter, Eviction};
use std::time::Duration;

let limiter = RateLimiter::per_second(1000)
    .with_eviction(Eviction::capacity(100_000).with_idle(Duration::from_secs(300)));
assert_eq!(limiter.eviction().max_keys(), Some(100_000));
```

#### `RateLimiter::with_algorithm`

```rust
pub fn with_algorithm(self, algorithm: Algorithm) -> Self
```

Selects the [`Algorithm`](#algorithm), discarding any per-key state. Intended
immediately after construction. The leaky bucket and window algorithms require
the `algorithms` feature; without it the only selectable variant is
`Algorithm::TokenBucket`.

- `algorithm` — the strategy to apply.

```rust
# #[cfg(feature = "algorithms")] {
use rate_net::{RateLimiter, Algorithm};

let limiter = RateLimiter::per_second(100).with_algorithm(Algorithm::FixedWindow);
assert_eq!(limiter.algorithm(), Algorithm::FixedWindow);
# }
```

#### `RateLimiter::check`

```rust
pub fn check(&self, key: impl Into<Key>) -> Decision
```

Checks a single unit against `key`. Returns [`Decision::Allow`](#decision) if the
key is within its limit (the unit is counted), or `Decision::Deny` carrying the
wait until it would be admitted.

- `key` — anything convertible into a [`Key`](#key): a `&str`, `String`,
  byte slice, `u64`, or `IpAddr`.

```rust
use rate_net::{RateLimiter, Decision};

let limiter = RateLimiter::per_second(1);
assert_eq!(limiter.check("user:42"), Decision::Allow);
assert!(limiter.check("user:42").is_deny()); // limit reached
```

Per-IP limiting reads naturally:

```rust
use rate_net::RateLimiter;
use std::net::{IpAddr, Ipv4Addr};

let limiter = RateLimiter::per_second(20);
let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
let _ = limiter.check(ip);
```

#### `RateLimiter::check_n`

```rust
pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision
```

Checks `n` units against `key` in one operation, for requests that cost more
than one unit (a batch, a weighted endpoint). All `n` units are admitted or none
are.

- `key` — the key, as for [`check`](#ratelimitercheck).
- `n` — units to take. `0` always succeeds; `n` greater than the quota can never
  succeed, and the denial's `retry_after` is `Duration::MAX`.

```rust
use rate_net::{RateLimiter, Decision};

let limiter = RateLimiter::per_second(10);
assert_eq!(limiter.check_n("tenant:acme", 4), Decision::Allow);
assert_eq!(limiter.check_n("tenant:acme", 6), Decision::Allow);
assert!(limiter.check_n("tenant:acme", 1).is_deny()); // 10 spent
```

A request larger than the quota is permanently refused:

```rust
use rate_net::{RateLimiter, Decision};
use std::time::Duration;

let limiter = RateLimiter::per_second(5);
assert_eq!(
    limiter.check_n("k", 6),
    Decision::Deny { retry_after: Duration::MAX },
);
```

<h4 id="ratelimiter-introspection">Introspection: <code>quota</code> / <code>algorithm</code> / <code>shards</code> / <code>eviction</code> / <code>tracked_keys</code></h4>

```rust
pub fn quota(&self) -> Quota
pub const fn algorithm(&self) -> Algorithm
pub fn shards(&self) -> usize
pub const fn eviction(&self) -> Eviction
pub fn tracked_keys(&self) -> usize
```

- `quota` — the [`Quota`](#quota) every key is limited to.
- `algorithm` — the [`Algorithm`](#algorithm) in force (currently always
  `TokenBucket`).
- `shards` — the number of shards the per-key store is split across (a power of
  two).
- `eviction` — the [`Eviction`](#eviction) policy bounding the store.
- `tracked_keys` — the number of keys with live state; a momentary, advisory
  snapshot bounded by the eviction policy.

```rust
use rate_net::{RateLimiter, Algorithm};

let limiter = RateLimiter::per_second(50).with_shards(16);
assert_eq!(limiter.quota().limit(), 50);
assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
assert_eq!(limiter.shards(), 16);
assert_eq!(limiter.tracked_keys(), 0);
let _ = limiter.check("a");
assert_eq!(limiter.tracked_keys(), 1);
```

---

### `Builder`

```rust
pub struct Builder<C: Clock + Clone = SystemClock> { /* private */ }
```

The Tier-2 path, started with [`RateLimiter::builder`](#ratelimiter). Chain the
knobs you care about, then `build()`. Anything left unset keeps a sane default
(token bucket, default sharding, bounded-memory eviction); the quota defaults to
a limit of `0`, which denies everything, so set it. `build()` is infallible.

Methods: `algorithm(Algorithm)`, `quota(limit, period)`, `per_second(limit)`,
`per_minute(limit)`, `burst(u32)`, `shards(usize)`, `eviction(Eviction)`,
`clock(C2)`, and `build() -> RateLimiter<C>`.

```rust
use rate_net::{RateLimiter, Algorithm, Eviction};
use std::time::Duration;

# #[cfg(feature = "algorithms")]
let limiter = RateLimiter::builder()
    .algorithm(Algorithm::SlidingWindowCounter)
    .quota(1000, Duration::from_secs(60)) // 1000 / minute
    .burst(50)                            // allow short bursts
    .shards(64)                           // tune for core count
    .eviction(Eviction::idle(Duration::from_secs(300)))
    .build();
```

The same knobs are also available as chainable adjusters on a constructed
limiter — [`with_shards`](#ratelimiterwith_shards),
[`with_eviction`](#ratelimiterwith_eviction),
[`with_algorithm`](#ratelimiterwith_algorithm),
[`with_clock`](#ratelimiterwith_clock) — for when the builder is more than you
need.

---

### `AsyncLimiter`

_Requires the `async` feature._

```rust
pub struct AsyncLimiter<C: Clock + Clone = SystemClock> { /* private */ }
```

An await-until-ready wrapper around a [`RateLimiter`](#ratelimiter). The core is
sync and runtime-free; this optional layer adds the one thing that needs a
runtime — *waiting* for a key to become allowed.

- `new(RateLimiter<C>) -> AsyncLimiter<C>` (also `From<RateLimiter<C>>`);
  `inner() -> &RateLimiter<C>`, `into_inner() -> RateLimiter<C>`.
- `check(key) -> Decision`, `check_n(key, n) -> Decision` — synchronous
  pass-throughs.
- `async until_ready(key)`, `async until_ready_n(key, n)` — retry on each
  denial, sleeping for the reported `retry_after` (via `tokio::time::sleep`),
  until the key is admitted. Returns immediately if the request can never
  succeed (a `retry_after` of `Duration::MAX`), so it never waits forever.

`until_ready` needs a clock that actually advances (the default `SystemClock`);
under a frozen `ManualClock` the allowance never refills, so it would wait
indefinitely.

```rust
use rate_net::{AsyncLimiter, RateLimiter};

# async fn demo() {
let limiter = AsyncLimiter::new(RateLimiter::per_second(100));

// Non-blocking, same as the sync API.
let _ = limiter.check("user:42");

// Or await until the key is within its limit.
limiter.until_ready("user:42").await;
# }
```

---

### `Limiter` trait

```rust
pub trait Limiter {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision;
    fn check(&self, key: impl Into<Key>) -> Decision { /* default: check_n(key, 1) */ }
}
```

The shared rate-limiting surface every algorithm implements, so generic code can
hold any limiter and call `check` without naming the concrete type or its clock.
[`RateLimiter`](#ratelimiter) implements it. Implementors provide only `check_n`;
`check` defaults to one unit.

```rust
use rate_net::{Limiter, RateLimiter};

fn admit_one<L: Limiter>(limiter: &L, key: &str) -> bool {
    limiter.check(key).is_allow()
}

let limiter = RateLimiter::per_second(2);
assert!(admit_one(&limiter, "user:1"));
```

---

### `Decision`

```rust
#[non_exhaustive]
pub enum Decision {
    Allow,
    Deny { retry_after: Duration },
}
```

The outcome of a check. A check is infallible — only an allow/deny outcome, so
this is a plain enum, not a `Result`. A denial carries `retry_after`: the minimum
wait until the same request would be admitted (`Duration::MAX` if it can never
succeed). `#[non_exhaustive]`, so a `match` needs a wildcard arm.

Helper methods:

- `is_allow(&self) -> bool`
- `is_deny(&self) -> bool`
- `retry_after(&self) -> Option<Duration>` — the wait, or `None` if allowed.

```rust
use rate_net::Decision;
use std::time::Duration;

let denied = Decision::Deny { retry_after: Duration::from_millis(250) };
assert!(denied.is_deny());
assert_eq!(denied.retry_after(), Some(Duration::from_millis(250)));
assert_eq!(Decision::Allow.retry_after(), None);
```

Mapping a denial onto an HTTP `Retry-After` header:

```rust
use rate_net::{RateLimiter, Decision};

let limiter = RateLimiter::per_second(1);
let _ = limiter.check("u"); // spend the allowance
if let Decision::Deny { retry_after } = limiter.check("u") {
    let header = retry_after.as_secs().max(1); // Retry-After is whole seconds
    assert!(header >= 1);
}
```

---

### `Quota`

```rust
pub struct Quota { /* private */ }
```

A rate limit: `limit` requests per `period`, per key, with a `burst` ceiling.
Under the token bucket a key starts with a full allowance of `burst`, spends one
unit per admitted request, and accrues `limit` units back over `period`.
`burst` defaults to `limit`; the window algorithms admit at most `limit` per
`period` and ignore it.

Constructors:

- `per_second(limit: u32) -> Quota` — infallible; `limit = 0` admits nothing.
- `per_minute(limit: u32) -> Quota` — infallible.
- `rate(limit: u32, period: Duration) -> Result<Quota, RateLimiterError>` —
  validated for arbitrary windows.

Accessors: `limit() -> u32`, `period() -> Duration`, `burst() -> u32`. Builder:
`with_burst(u32) -> Quota`.

```rust
use rate_net::Quota;
use std::time::Duration;

let per_sec = Quota::per_second(100);
assert_eq!(per_sec.limit(), 100);
assert_eq!(per_sec.period(), Duration::from_secs(1));

// Arbitrary window, validated.
let per_100ms = Quota::rate(5, Duration::from_millis(100))?;
assert_eq!(per_100ms.limit(), 5);
# Ok::<(), rate_net::RateLimiterError>(())
```

`rate` rejects values that cannot describe a working limit:

```rust
use rate_net::{Quota, RateLimiterError};
use std::time::Duration;

assert_eq!(Quota::rate(0, Duration::from_secs(1)), Err(RateLimiterError::ZeroQuota));
assert_eq!(Quota::rate(10, Duration::ZERO), Err(RateLimiterError::ZeroPeriod));
```

---

### `Eviction`

```rust
pub struct Eviction { /* private */ }
pub const DEFAULT_MAX_KEYS: usize; // 1 << 20
```

How the limiter bounds the memory its per-key state can occupy — the defense
against a unique-key flood. Two independent bounds compose: a **capacity** cap
(a hard ceiling on live keys; the least-recently-seen key is evicted to make
room) and an **idle TTL** (keys not seen for longer than the TTL are reclaimed).
Eviction is lazy, incremental, and per-shard — it never sweeps the whole store
or blocks the check path.

Constructors:

- `capacity(max_keys)` — a cap, no TTL.
- `idle(ttl)` — a TTL, keeping the [`DEFAULT_MAX_KEYS`] cap (idle expiry alone
  does not bound a flood, so the cap stays as the flood defense).
- `new(max_keys, ttl)` — both bounds.
- `unbounded()` — neither (only safe when the key space is intrinsically
  bounded).

Builders: `with_capacity(max_keys)`, `with_idle(ttl)`, `without_capacity()`.
Accessors: `max_keys() -> Option<usize>`, `idle_ttl() -> Option<Duration>`.

The [`Default`] is safe — a `DEFAULT_MAX_KEYS` cap and no TTL, so memory is
bounded out of the box.

```rust
use rate_net::{Eviction, DEFAULT_MAX_KEYS};
use std::time::Duration;

// Cap at 100k keys and reclaim anything idle for five minutes.
let policy = Eviction::capacity(100_000).with_idle(Duration::from_secs(300));
assert_eq!(policy.max_keys(), Some(100_000));
assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(300)));

// The default is bounded.
assert_eq!(Eviction::default().max_keys(), Some(DEFAULT_MAX_KEYS));
```

[`DEFAULT_MAX_KEYS`]: #eviction
[`Default`]: https://doc.rust-lang.org/std/default/trait.Default.html

---

### `Algorithm`

```rust
#[non_exhaustive]
pub enum Algorithm {
    TokenBucket,                              // always available; the default
    #[cfg(feature = "algorithms")] LeakyBucket,
    #[cfg(feature = "algorithms")] FixedWindow,
    #[cfg(feature = "algorithms")] SlidingWindowLog,
    #[cfg(feature = "algorithms")] SlidingWindowCounter,
}
```

Selects the algorithm a limiter applies; the selector the [`Builder`](#builder)
and [`with_algorithm`](#ratelimiterwith_algorithm) use. `#[non_exhaustive]` and
`Default` (`TokenBucket`). The four non-token variants exist only when the
`algorithms` feature is enabled.

```rust
use rate_net::Algorithm;

assert_eq!(Algorithm::default(), Algorithm::TokenBucket);
```

---

### `RateLimiterError`

```rust
#[non_exhaustive]
pub enum RateLimiterError {
    ZeroQuota,
    ZeroPeriod,
}
```

A limit configuration rejected at construction time, returned by
[`Quota::rate`](#quota). The check path never returns a `Result` — only
describing a limit can fail. Implements `std::error::Error`, `Display`, and
[`error_forge::ForgeError`](https://crates.io/crates/error-forge) (so `kind`,
`caption`, and `is_retryable` are available). `#[non_exhaustive]`.

- `ZeroQuota` — the quota limit was zero.
- `ZeroPeriod` — the quota period was zero.

```rust
use rate_net::{Quota, RateLimiterError};
use std::time::Duration;

let err = Quota::rate(0, Duration::from_secs(1)).unwrap_err();
assert_eq!(err, RateLimiterError::ZeroQuota);
assert!(err.to_string().contains("limit"));
```

---

### `Key`

```rust
pub struct Key(/* private */);
```

The opaque per-key identity a limit is tracked against — an IP, a user id, an API
token, a route. Stored as owned bytes — inline for keys up to a couple dozen
bytes, on the heap beyond that — so the common identities (IP addresses, `u64`
ids, short strings) cost no allocation and the steady-state check stays
allocation-free. Two keys are equal exactly when their bytes are equal, so the
identity is the byte content, not the source type. You rarely name it directly:
`check` accepts `impl Into<Key>`.

`From` conversions: `&str`, `String`, `&[u8]`, `Vec<u8>`, `u64`, `IpAddr`.
`as_bytes(&self) -> &[u8]` borrows the raw bytes.

```rust
use rate_net::Key;

let a: Key = "tenant:acme".into();
let b: Key = String::from("tenant:acme").into();
assert_eq!(a, b);
assert_eq!(a.as_bytes(), b"tenant:acme");
```

---

### `VERSION`

```rust
pub const VERSION: &str;
```

The crate version, captured from `Cargo.toml` at compile time — a
`major.minor.patch` string. Available even in `no_std` builds. Expose it to
report the exact `rate-net` build a process links against.

```rust
println!("rate-net {}", rate_net::VERSION);
assert!(rate_net::VERSION.starts_with("0."));
```

---

## Sharding and eviction

Per-key state lives in a **sharded** store: the key is hashed to one of a
power-of-two number of shards, each guarded by its own lock. An existing-key
check takes only a shard *read* lock and lets the key's bucket do its own atomic
accounting, so unrelated keys — and concurrent checks of the same key — never
serialise. Only first-seeing a key takes the brief write lock. Tune the shard
count with [`with_shards`](#ratelimiterwith_shards); it defaults to a small
multiple of the core count.

Memory is **bounded by eviction** (see [`Eviction`](#eviction)), the defense
against a unique-key flood. Eviction is lazy and per-shard: while inserting a new
key, under the write lock already held, the store drops idle-expired keys in that
one shard and, if the shard is at capacity, evicts its least-recently-seen key.
There is no background timer and no whole-store sweep, so the steady-state check
path is never blocked. The capacity cap is enforced approximately per shard, so
the live-key count stays within a small factor of the configured maximum.

```rust
use rate_net::{RateLimiter, Quota, Eviction};
use std::time::Duration;

let limiter = RateLimiter::with_quota(Quota::rate(1000, Duration::from_secs(60))?)
    .with_shards(64)
    .with_eviction(Eviction::capacity(100_000).with_idle(Duration::from_secs(300)));

assert_eq!(limiter.shards(), 64);
assert_eq!(limiter.eviction().max_keys(), Some(100_000));
# Ok::<(), rate_net::RateLimiterError>(())
```

These guarantees are verified by a `loom` model of the get-or-insert protocol, a
multi-threaded stress test (each key admitted exactly its quota under
contention), and an allocation audit (zero allocations on the steady-state
check).

---

## Tier 2 — the configured path

The [`Builder`](#builder) folds every knob — algorithm, quota, burst, shard
count, eviction policy, and clock — into one fluent surface, started with
[`RateLimiter::builder`](#ratelimiter). The same knobs are also chainable
adjusters ([`with_shards`](#ratelimiterwith_shards),
[`with_eviction`](#ratelimiterwith_eviction),
[`with_algorithm`](#ratelimiterwith_algorithm),
[`with_clock`](#ratelimiterwith_clock)) on a constructed limiter.

---

## Algorithms

All five share the [`Limiter`](#limiter-trait) surface and are selected by
[`Algorithm`](#algorithm). The token bucket is always available; the rest
require the `algorithms` feature. Each carries its own `proptest` over-admit
proof.

| Algorithm | Feature | Notes |
|-----------|---------|-------|
| Token bucket | always | Default; delegates to `better-bucket`. Bursts to capacity, then sustains. |
| Leaky bucket | `algorithms` | GCRA; spaces units at the emission interval, tolerating `burst`. |
| Fixed window | `algorithms` | Cheapest; lock-free packed counter. Tolerates a boundary burst (up to `2 × limit`). |
| Sliding-window log | `algorithms` | Exact; no boundary burst. Memory bounded by `limit` per key. |
| Sliding-window counter | `algorithms` | O(1) weighted two-window blend; approximate (worst case `2 × limit`). |

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | Standard library. Enables the limiter — the purpose-built sharded store, the token-bucket core (`better-bucket`'s `clock` feature), the injectable clock (`clock-lib`), and the error type (`error-forge`). With it off the crate is `no_std` and exposes only [`VERSION`](#version). |
| `algorithms` | no  | The leaky bucket and the window algorithms (fixed, sliding-log, sliding-counter), and their [`Algorithm`](#algorithm) variants. The token bucket is always available without it. |
| `async`      | no  | The [`AsyncLimiter`](#asynclimiter) await-until-ready wrapper. Additive; implies `std`. Only this layer touches a runtime (`tokio`'s timer). |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
