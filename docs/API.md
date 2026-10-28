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
> Every Rust example on this page is compiled and run as a doctest.
>
> **Stable (`v2.1.0`).** The public surface is frozen until `3.0`; each `2.x`
> adds to it without changing anything earlier `2.x` releases offered (two
> methods are deprecated since `1.1`). What changed from `1.x` is listed in
> [`MIGRATION_1_TO_2.md`](./MIGRATION_1_TO_2.md). Every public type is asserted `Send + Sync + 'static`
> at compile time. The leaky bucket and window algorithms require the
> `algorithms` feature, `AsyncLimiter` and the awaiting methods require `async`,
> `metrics()` requires `metrics`, and the [`tower`](#tower-middleware) module
> requires `tower`. With `std` off, only the [`no_std` core](#the-no_std-core)
> remains.

## Table of Contents

- [Overview](#overview)
- [Public API](#public-api)
  - [`RateLimiter`](#ratelimiter)
    - [Constructors and adjusters](#constructors-and-adjusters)
    - [`check` / `check_n`](#check--check_n)
    - [Borrowed keys: `check_bytes` / `check_n_bytes` / `peek_bytes`](#borrowed-keys)
    - [`peek`](#peek)
    - [Key management: `reset` / `remove` / `clear` / `retain_recent` / `shrink_to_fit`](#key-management)
    - [Introspection](#introspection)
    - [`metrics`](#metrics) _(feature: `metrics`)_
  - [`Builder`](#builder)
  - [`DirectLimiter`](#directlimiter)
  - [`AsyncLimiter`](#asynclimiter) _(feature: `async`)_
  - [`Limiter` and `DynLimiter`](#limiter-and-dynlimiter)
  - [`Decision`](#decision)
  - [`Snapshot`](#snapshot)
  - [`headers`](#headers)
  - [`Quota`](#quota)
  - [`Eviction`](#eviction)
  - [`Algorithm`](#algorithm)
  - [`Key`](#key)
  - [`CoarseClock`](#coarseclock)
  - [`Gcra`](#gcra)
  - [`GcraParams`](#gcraparams)
  - [`Metrics`](#metrics-1) _(feature: `metrics`)_
  - [`RateLimiterError`](#ratelimitererror)
  - [`InsufficientCapacity`](#insufficientcapacity)
  - [Constants](#constants)
- [`tower` middleware](#tower-middleware) _(feature: `tower`)_
- [The `no_std` core](#the-no_std-core)
- [Sharding and eviction](#sharding-and-eviction)
  - [Sizing the cap](#sizing-the-cap)
- [Algorithms](#algorithms)
- [Limits and ceilings](#limits-and-ceilings)
- [Feature flags](#feature-flags)

---

## Overview

`rate-net` answers "is this key allowed right now?" with a [`Decision`](#decision)
(`Allow` / `Deny { retry_after }`), tracking an independent allowance per key.
The common case is a constructor plus `check`:

```rust
use rate_net::{Decision, RateLimiter};

let limiter = RateLimiter::per_second(100);
match limiter.check("user:42") {
    Decision::Allow => { /* serve */ }
    Decision::Deny { retry_after } => { let _ = retry_after; /* 429 + Retry-After */ }
    _ => {}
}
```

The core guarantee: **for any key, admitted requests never exceed the
algorithm's bound** ([Algorithms](#algorithms)), under any interleaving of
threads and on a real clock. The token-bucket accounting is delegated to
[`better-bucket`](https://crates.io/crates/better-bucket) (1.1 or newer); time is
read from an injectable [`clock-lib`](https://crates.io/crates/clock-lib) clock.

---

## Public API

### `RateLimiter`

```rust ignore
pub struct RateLimiter<C: Clock + Clone = SystemClock> { /* private */ }
```

A keyed rate limiter. It tracks a separate allowance for every key it sees. It
is `Send + Sync` and meant to be shared — behind an `Arc`, or as a `static` —
across every thread serving requests; every method takes `&self`.

The type parameter `C` is the clock. It defaults to `SystemClock` (the OS
monotonic clock); inject a `ManualClock` for tests or a
[`CoarseClock`](#coarseclock) for speed with `with_clock`. Its `Debug` prints
only the configuration and the live key count — never the keys, which can be
caller identities.

#### Constructors and adjusters

```rust ignore
pub fn per_second(limit: u32) -> RateLimiter<SystemClock>
pub fn per_minute(limit: u32) -> RateLimiter<SystemClock>
pub fn with_quota(quota: Quota) -> RateLimiter<SystemClock>
pub fn builder() -> Builder<SystemClock>

pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> RateLimiter<C2>
pub fn with_algorithm(self, algorithm: Algorithm) -> Self
pub fn with_shards(self, shards: usize) -> Self
pub fn with_eviction(self, eviction: Eviction) -> Self
```

The Tier-1 constructors take a limit per second, per minute, or an explicit
[`Quota`](#quota), and use the token bucket, default sharding (four shards per
core, rounded to a power of two), and the default bounded [`Eviction`](#eviction).
They are infallible and fail closed: a limit of `0` denies everything. The
`with_*` adjusters replace one knob and discard any per-key state, so use them
immediately after construction. `with_shards` rounds up to a power of two.

```rust
use clock_lib::ManualClock;
use rate_net::{Eviction, Quota, RateLimiter};
use std::sync::Arc;
use std::time::Duration;

let clock = Arc::new(ManualClock::new());
let quota = Quota::rate(5, Duration::from_millis(100)).expect("valid quota");
let limiter = RateLimiter::with_quota(quota)
    .with_shards(64)
    .with_eviction(Eviction::capacity(100_000))
    .with_clock(Arc::clone(&clock));

for _ in 0..5 {
    assert!(limiter.check("k").is_allow());
}
assert!(limiter.check("k").is_deny());
clock.advance(Duration::from_millis(100)); // no real sleep
assert!(limiter.check("k").is_allow());
assert_eq!(limiter.shards(), 64);
```

#### `check` / `check_n`

```rust ignore
pub fn check(&self, key: impl Into<Key>) -> Decision
pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision
```

Checks one (or `n`) units against `key`: `Allow` counts them, `Deny` carries the
wait until the request would be admitted. `check_n` is all-or-nothing; `0`
always succeeds, and a request larger than the key's capacity is denied with
`retry_after == Duration::MAX` (see [`Decision::is_impossible`](#decision)).
The key is anything convertible into a [`Key`](#key).

An existing-key check takes the shard read lock and runs the algorithm's
accounting (one compare-and-swap for the token bucket, leaky bucket, and fixed
window; a short per-key mutex for the sliding-window algorithms). It allocates
nothing beyond building the `Key` itself, which copies a key longer than 22
bytes to the heap — use [`check_bytes`](#borrowed-keys) for long keys — except
that a sliding-window-log key's run-length log grows amortized (a few
allocations per thousand checks of a high-limit key, until it reaches its
working size). The first
check of a new key takes the shard write lock and may evict one other key,
recording its debt if it still owes ([Sharding and eviction](#sharding-and-eviction)).
A key whose quota admits nothing is denied without taking the write lock.

```rust
use rate_net::{Decision, RateLimiter};
use std::time::Duration;

let limiter = RateLimiter::per_second(10);
assert_eq!(limiter.check_n("tenant:acme", 4), Decision::Allow);
assert_eq!(limiter.check_n("tenant:acme", 6), Decision::Allow);
assert!(limiter.check("tenant:acme").is_deny()); // 10 spent

// More than the capacity can never succeed.
assert_eq!(limiter.check_n("other", 11).retry_after(), Some(Duration::MAX));
```

#### Borrowed keys

```rust ignore
pub fn check_bytes(&self, key: impl AsRef<[u8]>) -> Decision
pub fn check_n_bytes(&self, key: impl AsRef<[u8]>, n: u32) -> Decision
pub fn peek_bytes(&self, key: impl AsRef<[u8]>) -> Snapshot
```

The same as `check` / `check_n` / `peek` for a raw key (`&str`, `String`,
`&[u8]`, `Vec<u8>`, `[u8; N]` — the same identity as `Key::from` of it), but
the key is only borrowed: the check of an existing key allocates nothing
whatever its length, and an owned `Key` is built only when the key is first
stored. Use them on hot paths with long keys — UUIDs, API tokens, JWT subjects.

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(3);
let token = "9f6c1d2e-8b7a-4c3d-9e1f-0a2b3c4d5e6f";
assert!(limiter.check_bytes(token).is_allow());
assert!(limiter.check_n_bytes(token.as_bytes(), 2).is_allow());
assert!(limiter.check(token).is_deny()); // the same key
assert_eq!(limiter.peek_bytes(token).remaining(), 0);
```

#### `peek`

```rust ignore
pub fn peek(&self, key: impl Into<Key>) -> Snapshot
```

Reads `key`'s allowance without consuming any, returning a
[`Snapshot`](#snapshot) (capacity, remaining, time until whole). It takes only
the shard read lock, never creates state (an unseen key reports its full
allowance under its own quota, and a key evicted while still limited reports
the debt it will resume with), and does not refresh the key's eviction recency.
Advisory under concurrency.

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(10);
assert!(limiter.check_n("user:42", 4).is_allow());
let snapshot = limiter.peek("user:42");
assert_eq!((snapshot.limit(), snapshot.remaining()), (10, 6));
assert_eq!(limiter.peek("unseen").remaining(), 10);
assert_eq!(limiter.tracked_keys(), 1);
```

#### Key management

```rust ignore
pub fn reset(&self, key: impl Into<Key>) -> bool   // full allowance, still tracked
pub fn remove(&self, key: impl Into<Key>) -> bool  // forget the key
pub fn clear(&self)                                // forget every key
pub fn retain_recent(&self) -> usize               // forget fully-recovered keys
pub fn shrink_to_fit(&self)                        // release spare capacity
```

`reset` and `remove` also forget any debt recorded for the key when it was
evicted ([Sharding and eviction](#sharding-and-eviction)), and return whether
the key had state or such a debt. `clear` forgets every key and debt.
`retain_recent` removes
every key whose allowance has fully recovered — such a key behaves exactly like
one never seen, so no decision changes — and returns how many it removed; it
walks each shard in short chunks with the lock released between them.
`shrink_to_fit` returns memory the shards hold beyond their current keys.

```rust
use clock_lib::ManualClock;
use rate_net::RateLimiter;
use std::sync::Arc;
use std::time::Duration;

let clock = Arc::new(ManualClock::new());
let limiter = RateLimiter::per_minute(2).with_clock(Arc::clone(&clock));
assert!(limiter.check_n("user:42", 2).is_allow());
assert!(limiter.reset("user:42"));               // admin unblock
assert!(limiter.check("user:42").is_allow());

let _ = limiter.check("quiet");
clock.advance(Duration::from_secs(60));          // both keys recover
assert_eq!(limiter.retain_recent(), 2);
assert!(!limiter.remove("user:42"));             // already forgotten
limiter.clear();
limiter.shrink_to_fit();
```

#### Introspection

```rust ignore
pub fn quota(&self) -> Quota
pub fn quota_of(&self, key: impl Into<Key>) -> Quota
pub const fn algorithm(&self) -> Algorithm
pub fn shards(&self) -> usize
pub const fn eviction(&self) -> Eviction
pub fn tracked_keys(&self) -> usize
```

`quota` and `eviction` report the configuration as given (an idle TTL raised
to the recovery time is reported unraised). `quota_of(key)` is the quota a key
is under: its valid `quota_for` override, a quota with limit `0` if the
override is invalid (the key fails closed), otherwise `quota()`; it calls the
resolver each time. `tracked_keys` is a momentary,
advisory count, bounded by the eviction policy.

```rust
use rate_net::{Algorithm, RateLimiter};

let limiter = RateLimiter::per_second(50).with_shards(128);
assert_eq!(limiter.quota().limit(), 50);
assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
assert_eq!(limiter.shards(), 128);
let _ = limiter.check("a");
assert_eq!(limiter.tracked_keys(), 1);
```

#### `metrics`

_Requires the `metrics` feature._

```rust ignore
pub fn metrics(&self) -> Metrics
```

The limiter's cumulative counters, summed across shards. See
[`Metrics`](#metrics-1).

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(1);
let _ = limiter.check("a");
let _ = limiter.check("a");
let metrics = limiter.metrics();
assert_eq!((metrics.allowed, metrics.denied, metrics.keys_inserted), (1, 1, 1));
```

---

### `Builder`

```rust ignore
pub struct Builder<C: Clock + Clone = SystemClock> { /* private */ }

pub fn new() -> Builder<SystemClock>          // also Default; usually RateLimiter::builder()
pub fn algorithm(self, Algorithm) -> Self
pub fn quota(self, limit: u32, period: Duration) -> Self
pub fn per_second(self, limit: u32) -> Self
pub fn per_minute(self, limit: u32) -> Self
pub fn burst(self, burst: u32) -> Self       // token / leaky bucket capacity
pub fn shards(self, shards: usize) -> Self
pub fn eviction(self, Eviction) -> Self
pub fn max_key_len(self, len: usize) -> Self                 // refuse longer keys
pub fn digest_keys_longer_than(self, len: usize) -> Self     // store longer keys as a 16-byte digest
pub fn quota_for<F>(self, resolve: F) -> Self                // per-key quota overrides
    where F: Fn(&Key) -> Option<Quota> + Send + Sync + 'static
pub fn clock<C2: Clock + Clone>(self, C2) -> Builder<C2>

pub fn try_build(self) -> Result<RateLimiter<C>, RateLimiterError>
pub fn build(self) -> RateLimiter<C>
pub fn try_build_direct(self) -> Result<DirectLimiter<C>, RateLimiterError>
pub fn build_direct(self) -> DirectLimiter<C>
```

The Tier-2 path. Unset knobs keep their defaults (token bucket, default
sharding, bounded eviction, `SystemClock`); the quota defaults to a limit of
`0`, so set it. `Builder` implements `Debug` (the clock is omitted).

- **`try_build`** validates and names the first problem: `ZeroQuota`,
  `ZeroPeriod`, `ZeroBurst` (token / leaky bucket), `RateTooHigh` (leaky bucket
  above one unit per nanosecond), `LimitTooLarge` (sliding-window log above
  [`MAX_SLIDING_LOG_LIMIT`](#constants)), `ShardCountOutOfRange` (a shard count
  of `0` or above [`MAX_SHARDS`](#constants)), `IdleTtlTooShort` (an idle TTL below
  the quota's recovery time — `burst / limit` periods for the buckets, one period
  for the fixed window and log, two for the sliding-window counter).
- **`build`** never fails and **fails closed**: a zero limit or period denies
  everything for every algorithm, as does a zero burst for the token and leaky
  buckets (the window algorithms ignore the burst); a too-fast leaky-bucket rate is clamped
  to one unit per nanosecond; a short idle TTL is raised to the recovery time;
  a shard count is clamped to `1..=MAX_SHARDS`.
- **`try_build_direct` / `build_direct`** build a [`DirectLimiter`](#directlimiter)
  from the same algorithm, quota, burst, and clock (shards, eviction, and the
  key options are ignored).

Three key options shape per-key behaviour:

- **`quota_for(resolve)`** — tiered limits from one limiter. `resolve` is called
  with a key when it is about to be stored (and when an untracked key is
  peeked or passed to `quota_of`); `Some(quota)` gives the key that quota,
  `None` the default. It is never called for a tracked key. It runs on the
  caller's thread **before the limiter takes any lock**, so it never blocks
  other keys and may call into the limiter for *other* keys — but it must not
  check the key it is resolving (that recurses), should be fast (it delays the
  first request of every new key), and must be deterministic for a key (an
  evicted key that returns is resolved again). A panic propagates to the
  caller; nothing is stored. An override is validated like `try_build`
  validates the default quota (zero limit, period, or bucket burst; a leaky
  bucket above one unit per nanosecond; a sliding-window-log limit above
  `MAX_SLIDING_LOG_LIMIT`): an invalid one makes that key **fail closed** (a
  blocked tenant). The idle TTL is raised to the *default* quota's recovery
  time; a key with a longer override that expires while still limited has its
  debt recorded like any eviction.
- **`max_key_len(len)`** — a check of a key longer than `len` bytes is denied as
  impossible and the key is never stored, so the store's memory is bounded in
  bytes, not just keys.
- **`digest_keys_longer_than(len)`** — keys longer than `len` bytes are stored as
  a 16-byte digest from two independently, randomly seeded `ahash` hashers
  (fresh per limiter): constant memory per key, and no caller can aim a key at
  another's digest. Not a cryptographic hash. A `quota_for` resolver still sees
  the original key.

```rust
use rate_net::{Quota, RateLimiter};

let limiter = RateLimiter::builder()
    .per_second(10)
    .quota_for(|key| key.as_bytes().starts_with(b"pro:").then(|| Quota::per_second(1_000)))
    .max_key_len(512)
    .digest_keys_longer_than(32)
    .build();
assert!(limiter.check_n("pro:acme", 1_000).is_allow());
assert!(limiter.check_n("free:bob", 11).is_impossible());
assert!(limiter.check("x".repeat(513).as_str()).is_impossible());
assert!(limiter.check_bytes("y".repeat(100)).is_allow()); // digested
```

```rust
use rate_net::{Eviction, RateLimiter, RateLimiterError};
use std::time::Duration;

let limiter = RateLimiter::builder()
    .quota(1000, Duration::from_secs(60))
    .burst(50)
    .shards(64)
    .eviction(Eviction::idle(Duration::from_secs(300)))
    .try_build()
    .expect("a valid configuration");
assert_eq!(limiter.quota().burst(), 50);

// A one-second idle TTL would reset a 10-per-minute key after one idle second.
let err = RateLimiter::builder()
    .per_minute(10)
    .eviction(Eviction::idle(Duration::from_secs(1)))
    .try_build()
    .unwrap_err();
assert_eq!(err, RateLimiterError::IdleTtlTooShort);

// The infallible path fails closed.
let closed = RateLimiter::builder().quota(10, Duration::ZERO).build();
assert!(closed.check("k").is_deny());
```

---

### `DirectLimiter`

```rust ignore
pub struct DirectLimiter<C: Clock + Clone = SystemClock> { /* private */ }

pub fn per_second(limit: u32) -> DirectLimiter<SystemClock>
pub fn per_minute(limit: u32) -> DirectLimiter<SystemClock>
pub fn with_quota(quota: Quota) -> DirectLimiter<SystemClock>
pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> DirectLimiter<C2>
pub fn with_algorithm(self, algorithm: Algorithm) -> Self
pub fn check(&self) -> Decision
pub fn check_n(&self, n: u32) -> Decision
pub fn peek(&self) -> Snapshot
pub fn reset(&self)
pub const fn quota(&self) -> Quota
pub const fn algorithm(&self) -> Algorithm
pub async fn try_until_ready(&self) -> Result<(), InsufficientCapacity>          // feature `async`
pub async fn try_until_ready_n(&self, n: u32) -> Result<(), InsufficientCapacity> // feature `async`
pub async fn until_ready_within(&self, max_wait: Duration) -> Decision            // feature `async`
pub async fn until_ready_n_within(&self, n: u32, max_wait: Duration) -> Decision  // feature `async`
pub const fn with_jitter(self, max: Duration) -> Self                            // feature `async`
pub const fn jitter(&self) -> Duration                                           // feature `async`
pub fn metrics(&self) -> Metrics                                                 // feature `metrics`
```

One allowance, no keys: no hash, no shard lookup, no shard lock (the
algorithm's own accounting remains — a compare-and-swap, or a short mutex for
the sliding windows). For client-side
throttling, pacing a loop, or guarding one shared resource. Runs any
[`Algorithm`](#algorithm) on any clock. Without `std`, [`Gcra`](#gcra) is the
clock-free equivalent.

```rust
use rate_net::{DirectLimiter, Quota};

let limiter = DirectLimiter::with_quota(Quota::per_second(20).with_burst(5));
assert!(limiter.check_n(5).is_allow());
assert!(limiter.check().is_deny());
assert_eq!(limiter.peek().remaining(), 0);
limiter.reset();
assert!(limiter.check().is_allow());
```

---

### `AsyncLimiter`

_Requires the `async` feature._

```rust ignore
pub struct AsyncLimiter<C: Clock + Clone = SystemClock> { /* private */ }

pub fn new(inner: RateLimiter<C>) -> AsyncLimiter<C>   // also From<RateLimiter<C>>
pub fn inner(&self) -> &RateLimiter<C>
pub fn into_inner(self) -> RateLimiter<C>
pub fn check(&self, key: impl Into<Key>) -> Decision
pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision
pub async fn try_until_ready(&self, key: impl Into<Key>) -> Result<(), InsufficientCapacity>
pub async fn try_until_ready_n(&self, key: impl Into<Key>, n: u32) -> Result<(), InsufficientCapacity>
pub async fn until_ready_within(&self, key: impl Into<Key>, max_wait: Duration) -> Decision
pub async fn until_ready_n_within(&self, key: impl Into<Key>, n: u32, max_wait: Duration) -> Decision
pub const fn with_jitter(self, max: Duration) -> Self
pub const fn jitter(&self) -> Duration
#[deprecated] pub async fn until_ready(&self, key: impl Into<Key>)
#[deprecated] pub async fn until_ready_n(&self, key: impl Into<Key>, n: u32)
```

An await-until-ready wrapper. `try_until_ready_n` checks, and on a denial sleeps
for the reported `retry_after` (via `tokio::time::sleep`) and checks again,
returning `Ok(())` only once the units have been counted. A request that can
never fit returns [`InsufficientCapacity`](#insufficientcapacity) at once. It is
cancel-safe: dropping the future consumes nothing.

`until_ready_n_within(key, n, max_wait)` is the bounded form: it sleeps only
while the next admission the limiter reports falls within the remaining
budget, and returns `Decision::Allow` once admitted or the last `Deny` — at
once, not after sleeping out the budget — whose `retry_after` is the wait still
needed (`Duration::MAX` if the request can never fit). `with_jitter(max)` adds
a uniformly random delay in `[0, max]` to every sleep (never past a deadline),
so tasks denied together do not retry together. `DirectLimiter` has the same
bounded waits and jitter.

The deprecated `until_ready` / `until_ready_n` return *without admitting* when
the request can never fit, which callers cannot tell from an admission; they
remain until `2.0`. Waiting needs a clock that advances; under a frozen
`ManualClock` the wait never ends. The awaiting methods sleep with
`tokio::time::sleep`, so they panic if polled outside a Tokio runtime with the
time driver enabled.

```rust
use rate_net::{AsyncLimiter, InsufficientCapacity, RateLimiter};

async fn handle(limiter: &AsyncLimiter) -> Result<(), InsufficientCapacity> {
    limiter.try_until_ready("user:42").await?;
    // admitted: serve the request
    Ok(())
}

let limiter = AsyncLimiter::new(RateLimiter::per_second(100));
let _ = limiter.check("user:42"); // the sync pass-through
let _future = handle(&limiter);
```

---

### `Limiter` and `DynLimiter`

```rust ignore
pub trait Limiter {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision;
    fn check(&self, key: impl Into<Key>) -> Decision { /* check_n(key, 1) */ }
}

pub trait DynLimiter: Send + Sync {
    fn check_key(&self, key: Key, n: u32) -> Decision;
}
```

`Limiter` is the shared surface generic code depends on; implementors provide
`check_n`. It is implemented for `RateLimiter` and for `Arc<L>` of any limiter
(not for `&L` or `Box<L>`: code written against 1.0 may implement `Limiter`
for references and boxes of its own types, and blanket impls would conflict). Its methods are generic, so `dyn Limiter` is not
possible; `DynLimiter` is the object-safe face: every `Send + Sync` `Limiter`
implements it, and `dyn DynLimiter` implements `Limiter` again, so the generic
helpers stay available on trait objects. Since 2.0 `DynLimiter` requires
`Send + Sync`, so `Arc<dyn DynLimiter>` can be shared across threads as it is
(`dyn DynLimiter + Send + Sync` still works and means the same).

```rust
use rate_net::{DynLimiter, Limiter, RateLimiter};
use std::sync::Arc;

fn admit_one<L: Limiter>(limiter: &L, key: &str) -> bool {
    limiter.check(key).is_allow()
}

let limiter: Arc<dyn DynLimiter> = Arc::new(RateLimiter::per_second(2));
assert!(limiter.check_key("user:1".into(), 1).is_allow());
assert!(admit_one(&limiter, "user:1"));
assert!(!admit_one(&limiter, "user:1"));
```

---

### `Decision`

```rust ignore
#[non_exhaustive]
pub enum Decision {
    Allow,
    Deny { retry_after: Duration },
}

pub const fn is_allow(&self) -> bool
pub const fn is_deny(&self) -> bool
pub const fn retry_after(&self) -> Option<Duration>
pub fn is_impossible(&self) -> bool
pub fn retry_after_secs(&self) -> Option<u64>
```

The outcome of a check — infallible, so a plain enum, not a `Result`. A denial
carries the minimum wait until the same request would be admitted;
`Duration::MAX` means never (`is_impossible`: the request exceeds the capacity,
or the limiter admits nothing — answer it with `400`/`413`, not `429`).
`retry_after_secs` is the HTTP `Retry-After` value: whole seconds **rounded up**,
at least `1`, and `None` when allowed or impossible. `#[non_exhaustive]`, so a
`match` needs a wildcard arm.

```rust
use rate_net::Decision;
use std::time::Duration;

let denied = Decision::Deny { retry_after: Duration::from_millis(1_900) };
assert!(denied.is_deny());
assert_eq!(denied.retry_after(), Some(Duration::from_millis(1_900)));
assert_eq!(denied.retry_after_secs(), Some(2)); // never early
assert!(Decision::Deny { retry_after: Duration::MAX }.is_impossible());
assert_eq!(Decision::Allow.retry_after_secs(), None);
```

---

### `Snapshot`

```rust ignore
pub struct Snapshot { /* private */ }   // Copy, Eq, Hash, Debug

pub const fn limit(&self) -> u32              // capacity: burst (buckets) or limit (windows)
pub const fn remaining(&self) -> u32          // admissible right now, <= limit
pub const fn reset_after(&self) -> Duration   // until remaining == limit
pub const fn reset_after_secs(&self) -> u64   // rounded up
pub const fn is_exhausted(&self) -> bool      // remaining == 0
pub const fn headers(&self) -> [(&'static str, u64); 3]
pub const fn ratelimit<'a>(&self, policy: &'a str) -> headers::RateLimitValue<'a>
```

A read-only view of one key's allowance, from `RateLimiter::peek`,
`DirectLimiter::peek`, or `Gcra::peek_at`. A limiter that admits nothing reports
`limit() == 0` and `reset_after() == Duration::MAX`. `headers` returns
`RateLimit-Limit`, `RateLimit-Remaining`, and `RateLimit-Reset` (`0` for a
limiter that admits nothing, rather than a 20-digit "never"). `ratelimit(policy)`
is the value of the current draft's structured `RateLimit` field:
`"policy";r=<remaining>;t=<reset>`.

```rust
use clock_lib::ManualClock;
use rate_net::RateLimiter;
use std::sync::Arc;

// A frozen clock keeps the numbers exact however slow the host.
let limiter = RateLimiter::per_second(10).with_clock(Arc::new(ManualClock::new()));
let _ = limiter.check_n("k", 3);
let snapshot = limiter.peek("k");
assert!(!snapshot.is_exhausted());
assert_eq!(
    snapshot.headers(),
    [("RateLimit-Limit", 10), ("RateLimit-Remaining", 7), ("RateLimit-Reset", 1)],
);
```

---

### `headers`

```rust ignore
pub mod headers {
    pub const RETRY_AFTER: &str = "Retry-After";
    pub const RATELIMIT_LIMIT: &str = "RateLimit-Limit";
    pub const RATELIMIT_REMAINING: &str = "RateLimit-Remaining";
    pub const RATELIMIT_RESET: &str = "RateLimit-Reset";
    pub const RATELIMIT: &str = "RateLimit";
    pub const RATELIMIT_POLICY: &str = "RateLimit-Policy";

    pub struct RateLimitValue<'a> { /* private */ }        // Display, Copy, Eq, Debug
    pub struct RateLimitPolicyValue<'a> { /* private */ }  // Display, Copy, Eq, Debug
}
```

Header names for rate-limit responses: `Retry-After` (RFC 9110), the three
`RateLimit-*` fields of the earlier revisions of the IETF rate-limit headers
draft (still the most widely deployed), and the current revision's two
structured fields. The value types format through `Display` without
allocating: `RateLimitValue` (from [`Snapshot::ratelimit`](#snapshot)) as
`"name";r=<remaining>;t=<reset>`, `RateLimitPolicyValue` (from
[`Quota::ratelimit_policy`](#quota)) as `"name";q=<quota>;w=<window>`. The
policy name is written as a quoted Structured Field string — `"` and `\`
escaped, characters outside printable ASCII dropped — and the integers clamp to
fifteen digits, so the value is always a valid header value. rate-net hands back
names and values; the caller writes them into its framework's response type
(or uses the [`tower` middleware](#tower-middleware)).

```rust
use clock_lib::ManualClock;
use rate_net::{RateLimiter, headers};
use std::sync::Arc;

let limiter = RateLimiter::per_minute(1).with_clock(Arc::new(ManualClock::new()));
let _ = limiter.check("client");
let decision = limiter.check("client");
let retry = decision.retry_after_secs().map(|secs| (headers::RETRY_AFTER, secs));
assert_eq!(retry, Some(("Retry-After", 60)));

let policy = limiter.quota().ratelimit_policy("default");
assert_eq!(policy.to_string(), r#""default";q=1;w=60"#);
let current = limiter.peek("client").ratelimit("default");
assert_eq!(current.to_string(), r#""default";r=0;t=60"#);
```

---

### `Quota`

```rust ignore
pub struct Quota { /* private */ }   // Copy, Eq, Debug

pub const fn per_second(limit: u32) -> Quota
pub const fn per_minute(limit: u32) -> Quota
pub const fn rate(limit: u32, period: Duration) -> Result<Quota, RateLimiterError>
pub const fn with_burst(self, burst: u32) -> Quota
pub const fn limit(&self) -> u32
pub const fn period(&self) -> Duration
pub const fn burst(&self) -> u32
pub const fn ratelimit_policy<'a>(&self, name: &'a str) -> headers::RateLimitPolicyValue<'a>
```

A rate limit: `limit` units per `period`, per key, with a `burst` ceiling
(defaulting to `limit`). Under the buckets a key starts with `burst` units and
regains `limit` per `period`; the window algorithms admit at most `limit` per
`period` and ignore `burst`. `per_second` / `per_minute` are infallible (a `0`
limit admits nothing); `rate` rejects `ZeroQuota` and `ZeroPeriod`.
`ratelimit_policy(name)` describes the quota as a `RateLimit-Policy` field
(`q` = limit, `w` = period in whole seconds, rounded up; the field has no burst
parameter). Available without `std`.

```rust
use rate_net::{Quota, RateLimiterError};
use std::time::Duration;

let shaped = Quota::per_minute(1000).with_burst(50);
assert_eq!((shaped.limit(), shaped.burst()), (1000, 50));
assert_eq!(Quota::rate(0, Duration::from_secs(1)), Err(RateLimiterError::ZeroQuota));
assert_eq!(Quota::rate(10, Duration::ZERO), Err(RateLimiterError::ZeroPeriod));
```

---

### `Eviction`

```rust ignore
pub struct Eviction { /* private */ }   // Copy, Eq, Debug, Default

pub const fn capacity(max_keys: usize) -> Eviction       // cap, no TTL
pub const fn idle(ttl: Duration) -> Eviction             // TTL + the DEFAULT_MAX_KEYS cap
pub const fn new(max_keys: usize, ttl: Duration) -> Eviction
pub const fn unbounded() -> Eviction
pub const fn with_capacity(self, max_keys: usize) -> Eviction
pub const fn with_idle(self, ttl: Duration) -> Eviction
pub const fn without_capacity(self) -> Eviction
pub const fn max_keys(&self) -> Option<usize>
pub const fn idle_ttl(&self) -> Option<Duration>
pub const fn with_overflow(self, overflow: Overflow) -> Eviction
pub const fn overflow(&self) -> Overflow

#[non_exhaustive]
pub enum Overflow { Forget, FailClosed /* default */ }  // Copy, Eq, Hash, Debug, Default
```

How the store bounds its memory — the defense against a unique-key flood. A
**capacity** cap evicts by CLOCK when a new key would exceed it, preferring a
key whose allowance has recovered (spread evenly over the shards, at least one
key per shard, so `capacity(0)` behaves as one per shard); an **idle TTL**
reclaims keys not seen for that long, raised to the quota's recovery time. Either
way a key evicted before it recovered has its debt recorded and resumes no more
permissive than it left — unconditionally while the still-limited evicted keys
fit about twice the cap. Past that, the **`Overflow`** policy decides:
`FailClosed` (the default) keeps them all, so no evicted key ever comes back
looser, at the cost of charging or refusing some keys that owed nothing;
`Forget` (an opt-out for availability-first limiters such as anonymous per-IP
load shedding) drops the debts that do not fit, so those keys may come back
with a fresh allowance (1.x behaviour) and no other key is affected. See
[Sharding and eviction](#sharding-and-eviction) for the exact guarantee, the
measured trade-off, and [how to size the cap](#sizing-the-cap) so neither
happens. The `Default` is a [`DEFAULT_MAX_KEYS`](#constants) cap, no TTL, and
`Overflow::FailClosed`. `unbounded` is only safe when the key space is
intrinsically small.

```rust
use rate_net::{Eviction, DEFAULT_MAX_KEYS};
use std::time::Duration;

let policy = Eviction::capacity(100_000).with_idle(Duration::from_secs(300));
assert_eq!(policy.max_keys(), Some(100_000));
assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(300)));
assert_eq!(Eviction::default().max_keys(), Some(DEFAULT_MAX_KEYS));
assert_eq!(Eviction::unbounded().without_capacity().max_keys(), None);

use rate_net::Overflow;
let shedding = Eviction::capacity(100_000).with_overflow(Overflow::Forget);
assert_eq!(shedding.overflow(), Overflow::Forget);
assert_eq!(Eviction::default().overflow(), Overflow::FailClosed);
```

---

### `Algorithm`

```rust ignore
#[non_exhaustive]
pub enum Algorithm {
    TokenBucket,                                   // always available; the default
    #[cfg(feature = "algorithms")] LeakyBucket,
    #[cfg(feature = "algorithms")] FixedWindow,
    #[cfg(feature = "algorithms")] SlidingWindowLog,
    #[cfg(feature = "algorithms")] SlidingWindowCounter,
}
```

Selects the algorithm a limiter applies. `Default` is `TokenBucket`; the other
variants exist only with the `algorithms` feature. See [Algorithms](#algorithms).

```rust
use rate_net::{Algorithm, RateLimiter};

assert_eq!(Algorithm::default(), Algorithm::TokenBucket);
let limiter = RateLimiter::per_second(100).with_algorithm(Algorithm::SlidingWindowCounter);
assert_eq!(limiter.algorithm(), Algorithm::SlidingWindowCounter);
```

---

### `Key`

```rust ignore
pub struct Key(/* private */);   // Clone, Eq, Hash, Debug (redacted)

pub fn id(id: u64) -> Key
pub fn ip(addr: IpAddr) -> Key
pub fn ipv4_prefix(addr: Ipv4Addr, prefix_len: u8) -> Key
pub fn ipv6_prefix(addr: Ipv6Addr, prefix_len: u8) -> Key
pub fn ip_prefix(addr: IpAddr, v4_prefix_len: u8, v6_prefix_len: u8) -> Key
pub fn as_bytes(&self) -> &[u8]
pub fn len(&self) -> usize
pub fn is_empty(&self) -> bool

impl From<&str> | From<String> | From<&String> | From<&[u8]> | From<Vec<u8>>
   | From<[u8; N]> | From<&[u8; N]>
   | From<u64> | From<IpAddr> | From<Ipv4Addr> | From<Ipv6Addr>
   | From<SocketAddr> | From<&Key> for Key
```

The identity a limit is tracked against, stored inline up to 22 bytes (no
allocation) and on the heap beyond. The `From` conversions make **raw** keys —
identity is the bytes alone, so `Key::from(1u64)` equals the equivalent 8-byte
slice. The constructors make **tagged** keys whose kind is part of the
identity, so they never collide with raw keys or each other: `id` for numeric
ids, `ip` for a canonical address (an IPv4-mapped or IPv4-compatible IPv6
address equals the IPv4 address), and the `*_prefix` constructors for networks
(lengths clamp to the address width and are part of the key). `ipv6_prefix`
keys an embedded IPv4 address by that address (as `ipv4_prefix(v4, 32)`), so
IPv4 clients on a dual-stack socket do not all share one `/64`. `From<SocketAddr>` drops the port. `Debug`
shows the kind and length only. There are deliberately no `From` conversions
for integer types other than `u64`: a second one would make an unsuffixed
literal (`limiter.check(42)`) ambiguous and break code that compiles today.

```rust
use rate_net::Key;
use std::net::{IpAddr, Ipv6Addr};

let a: Key = "tenant:acme".into();
assert_eq!(a.as_bytes(), b"tenant:acme");
assert_ne!(Key::id(42), Key::from(42u64));

let one: Ipv6Addr = "2001:db8:1:2::1".parse().unwrap();
let two: Ipv6Addr = "2001:db8:1:2:ffff::9".parse().unwrap();
assert_eq!(Key::ipv6_prefix(one, 64), Key::ipv6_prefix(two, 64));

let mapped: IpAddr = "::ffff:192.0.2.1".parse().unwrap();
let v4: IpAddr = "192.0.2.1".parse().unwrap();
assert_eq!(Key::ip(mapped), Key::ip(v4));
assert!(!format!("{a:?}").contains("acme"));
```

---

### `CoarseClock`

```rust ignore
#[derive(Clone)]
pub struct CoarseClock { /* private */ }   // Clock, Default, Debug

pub fn new() -> CoarseClock                                    // ticked by the caller
pub fn spawn(resolution: Duration) -> std::io::Result<CoarseClock>  // ticked by a thread
pub fn tick(&self)
pub fn lag(&self) -> Duration
```

A `clock_lib::Clock` read with one atomic load instead of a system call. It
holds the time of its last tick: `spawn` starts one background thread that ticks
every `resolution` (at least [`MIN_COARSE_RESOLUTION`](#constants)) and exits
once every clone is dropped; `new` has no thread and moves only on `tick`.
Readings never run ahead of real time but lag it by `L` (`lag()`), and a
limiter on the clock may admit up to `rate × L` beyond the quota over any span
(the lagged time is credited when the clock catches up). For a spawned clock
`L` is about one interval: every 61st read on a thread re-reads the real clock
and ticks if a whole interval behind, so a starved ticker cannot stretch it. A
manual clock lags by the time since its last `tick`.

```rust
use clock_lib::Clock;
use rate_net::{CoarseClock, RateLimiter};
use std::time::Duration;

let clock = CoarseClock::spawn(Duration::from_millis(1)).expect("spawn the ticker");
let limiter = RateLimiter::per_second(1_000).with_clock(clock.clone());
assert!(limiter.check("k").is_allow());

let manual = CoarseClock::new();
let before = manual.now();
manual.tick();
assert!(manual.now() >= before);
```

---

### `Gcra`

```rust ignore
pub struct Gcra { /* private */ }   // Debug

pub fn new(quota: Quota) -> Gcra
pub fn try_new(quota: Quota) -> Result<Gcra, RateLimiterError>
pub fn check_at(&self, now: Duration) -> Decision
pub fn check_n_at(&self, n: u32, now: Duration) -> Decision
pub fn peek_at(&self, now: Duration) -> Snapshot
pub fn reset(&self)
pub const fn quota(&self) -> Quota
pub const fn params(&self) -> &GcraParams        // since 2.1.0
```

The `no_std` single-key core: a lock-free leaky bucket (the Generic Cell Rate
Algorithm) with the time passed in by the caller as a `Duration` since any fixed
origin. Exact at any rate — time is accounted in whole emission intervals —
and conservative under stale readings. `new` fails closed; `try_new` rejects
`ZeroQuota`, `ZeroPeriod`, `ZeroBurst`, and `RateTooHigh`. Available on every
target with 64-bit atomics, with or without `std`.

```rust
use core::time::Duration;
use rate_net::{Gcra, Quota};

let gcra = Gcra::try_new(Quota::per_second(10)).expect("valid quota");
for _ in 0..10 {
    assert!(gcra.check_at(Duration::ZERO).is_allow());
}
assert_eq!(gcra.check_at(Duration::ZERO).retry_after(), Some(Duration::from_millis(100)));
assert!(gcra.check_at(Duration::from_millis(100)).is_allow());
assert_eq!(gcra.peek_at(Duration::from_millis(100)).remaining(), 0);
```

A `Gcra` is one theoretical arrival time plus its [`GcraParams`](#gcraparams);
`params()` returns them.

---

### `GcraParams`

_Since `2.1.0`._

```rust ignore
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GcraParams { /* private */ }   // Debug

pub fn new(quota: Quota) -> GcraParams
pub fn try_new(quota: Quota) -> Result<GcraParams, RateLimiterError>
pub fn check_at(&self, tat: &AtomicU64, now: Duration) -> Decision
pub fn check_n_at(&self, tat: &AtomicU64, n: u32, now: Duration) -> Decision
pub fn peek_at(&self, tat: &AtomicU64, now: Duration) -> Snapshot
pub fn reset(&self, tat: &AtomicU64)
pub const fn quota(&self) -> Quota
```

The [`Gcra`](#gcra) arithmetic with the quota's parameters held once, run
against a theoretical arrival time (TAT) that the caller stores as a
`core::sync::atomic::AtomicU64`. A `Gcra` carries its parameters, about 80
bytes, in every instance; a keyed table that limits many streams under one
quota shares one `GcraParams` and keeps only the 8-byte word per key, beside
the key itself, so a check reads one cache line instead of two.

- **The word.** `AtomicU64::new(0)` is a full bucket, and storing `0` (or
  calling `reset`) restores the full burst. Any other value counts emission
  intervals of the quota it was written under, so use a word with one set of
  parameters only. The methods update it with the same compare-and-swap loop
  `Gcra` uses, so any number of threads may check one word at once; a denial
  leaves it unchanged.
- **Same decisions as `Gcra`.** `Gcra` is built on these methods, so for the
  same quota, word, and times the two decide identically: the exact interval
  arithmetic, the clamp to one unit per nanosecond, fail closed on a zero
  limit, period, or burst (`new`; `try_new` rejects them), `n = 0` always
  admitted, `n` past the burst denied with `retry_after == Duration::MAX`, and
  a late `now` only ever stricter.
- **Time.** As for `Gcra`: `Duration`s since one fixed origin, from one
  monotonic source per word.
- Available on every target with 64-bit atomics, with or without `std`.

```rust
use core::sync::atomic::AtomicU64;
use core::time::Duration;
use rate_net::{GcraParams, Quota};

// One set of parameters for a whole tier; one word per key.
let params = GcraParams::try_new(Quota::per_second(10).with_burst(2)).expect("valid quota");
let table: [(u64, AtomicU64); 2] = [(7, AtomicU64::new(0)), (9, AtomicU64::new(0))];

let (_, tat) = &table[0];
assert!(params.check_n_at(tat, 2, Duration::ZERO).is_allow());
assert_eq!(params.check_at(tat, Duration::ZERO).retry_after(), Some(Duration::from_millis(100)));
assert_eq!(params.peek_at(tat, Duration::from_millis(100)).remaining(), 1);

// Another key's word is independent.
let (_, other) = &table[1];
assert!(params.check_n_at(other, 2, Duration::ZERO).is_allow());

params.reset(tat);
assert_eq!(params.peek_at(tat, Duration::ZERO).remaining(), 2);
```

---

### `Metrics`

_Requires the `metrics` feature._

```rust ignore
#[non_exhaustive]
pub struct Metrics {
    pub allowed: u64,        // checks admitted
    pub denied: u64,         // checks refused
    pub keys_inserted: u64,  // keys seen for the first time
    pub keys_evicted: u64,   // keys evicted by the capacity cap
    pub keys_expired: u64,   // keys reclaimed by the idle sweep
    pub keys_removed: u64,   // remove / clear / retain_recent
    pub keys_remembered: u64, // evicted before recovering: debt recorded
    pub keys_restored: u64,   // new keys charged a recorded debt
    pub debts_forgotten: u64, // debts dropped past the exact table (Overflow::Forget)
    pub debts_overflowed: u64, // debts spilled into the shared sketch (Overflow::FailClosed)
    pub keys_refused: u64,    // the saturation counter: newcomers refused (FailClosed)
    pub keys_rejected: u64,   // refused by max_key_len
}

impl RateLimiter<C> {
    pub fn metrics(&self) -> Metrics
    pub fn recovery_load(&self) -> RecoveryLoad   // walks the recovery tables
}

#[non_exhaustive]
pub struct RecoveryLoad {
    pub debts_held: u64,          // live exact debt records (evicted keys still owed)
    pub debt_capacity: u64,       // exact records the tables hold when full
    pub fullest_shard: f64,       // the largest per-shard fill, 0.0..=1.0
    pub overflow_cells_live: u64, // live overflow-sketch cells (FailClosed)
    pub overflow_cells: u64,      // the sketch's cells (0 under Forget)
}
impl RecoveryLoad { pub fn utilization(&self) -> f64 }  // debts_held / debt_capacity
```

Cumulative counters since construction, kept per shard beside the shard lock
(one relaxed increment per check) and summed on read. Export them as monotonic
counters. `keys_evicted` should normally be zero: a steady rate means the cap is
below the live key population or the limiter is under a unique-key flood (a
`keys_inserted` spike is the other signature). `keys_remembered` counts the
evictions that had to record a debt and `keys_restored` the keys that came back
to one (including keys charged from the shared overflow sketch).
`debts_forgotten`, `debts_overflowed`, and `keys_refused` should be zero: a
sustained `debts_forgotten` (under `Overflow::Forget`) means more still-limited
keys are being evicted than the exact recovery table holds, so some of them come
back with a fresh allowance; a sustained `debts_overflowed` (under
`Overflow::FailClosed`) means the same, and keys that never owed anything can
start drained;
`keys_refused` counts newcomers denied with a retry time because a full shard
held only still-limited keys whose debts could not be kept (both are the store
failing closed). Either means the cap is too small for the traffic. Without
the feature the counters do not exist.

`recovery_load()` is the gauge to alert on **before** any of that: how many
evicted keys' debts the store holds against the exact capacity it has for
them, overall and in the fullest shard. Past `1.0` the overflow policy takes
over, so alert at about `0.7`. It walks every shard's recovery table under its
read lock — up to 64 bytes per key of capacity under the default
`Overflow::FailClosed` (32 of exact records, 32 of overflow sketch), 32 under
`Overflow::Forget` (exact records only): export it every few seconds,
never per request. With the `tracing` feature the limiter also logs a
`warn!` (target `rate_net::eviction`) on the first spill, drop, or refusal,
and at most once a minute while they continue, with the count since the last
warning; nothing on the check path changes.

---

### `RateLimiterError`

```rust ignore
#[non_exhaustive]
pub enum RateLimiterError {
    ZeroQuota, ZeroPeriod, ZeroBurst, RateTooHigh, LimitTooLarge,
    ShardCountOutOfRange, IdleTtlTooShort,
}
```

A configuration rejected by `Quota::rate`, `Builder::try_build`,
`Builder::try_build_direct`, or `Gcra::try_new`. Implements `Display`,
`core::error::Error`, and (with the `forge` feature)
[`error_forge::ForgeError`](https://crates.io/crates/error-forge) — `kind()` is
the variant name, `is_retryable()` is `false`. `#[non_exhaustive]`.

| Variant | Meaning |
|---------|---------|
| `ZeroQuota` | The limit was `0`. |
| `ZeroPeriod` | The period was zero. |
| `ZeroBurst` | The burst was `0` (token / leaky bucket). |
| `RateTooHigh` | A leaky bucket faster than one unit per nanosecond. |
| `LimitTooLarge` | A sliding-window log limit above `MAX_SLIDING_LOG_LIMIT`. |
| `ShardCountOutOfRange` | A shard count of `0` or above `MAX_SHARDS`. |
| `IdleTtlTooShort` | An idle TTL below the quota's recovery time. |

```rust
use rate_net::{Quota, RateLimiterError};
use std::time::Duration;

let err = Quota::rate(0, Duration::from_secs(1)).unwrap_err();
assert_eq!(err, RateLimiterError::ZeroQuota);
assert!(err.to_string().contains("limit"));
```

---

### `InsufficientCapacity`

```rust ignore
pub struct InsufficientCapacity { /* private */ }   // Copy, Eq, Debug

pub const fn new(requested: u32) -> InsufficientCapacity
pub const fn requested(&self) -> u32
```

Returned by the `try_until_ready` methods when a request can never be admitted.
Implements `Display`, `core::error::Error`, and (with `std`) `ForgeError`.

```rust
use rate_net::InsufficientCapacity;

let err = InsufficientCapacity::new(6);
assert_eq!(err.requested(), 6);
assert!(err.to_string().contains('6'));
```

---

### Constants

```rust ignore
pub const VERSION: &str;                         // this crate's version
pub const DEFAULT_MAX_KEYS: usize = 1 << 20;     // the default capacity cap
pub const MAX_SLIDING_LOG_LIMIT: u32 = 1 << 16;  // try_build ceiling for the log
pub const MAX_SHARDS: usize = 1 << 16;           // shard-count ceiling (clamped by build)
pub const MIN_COARSE_RESOLUTION: Duration;       // 100 µs: CoarseClock::spawn floor
```

```rust
assert_eq!(rate_net::VERSION.split('.').count(), 3);
assert_eq!(rate_net::DEFAULT_MAX_KEYS, 1_048_576);
assert_eq!(rate_net::MAX_SLIDING_LOG_LIMIT, 65_536);
assert_eq!(rate_net::MAX_SHARDS, 65_536);
assert_eq!(rate_net::MIN_COARSE_RESOLUTION, std::time::Duration::from_micros(100));
```

---

## `tower` middleware

_Requires the `tower` feature._

```rust ignore
pub mod tower {
    pub struct RateLimitLayer<K, C = SystemClock> { /* private */ }   // Layer, Clone, Debug
    pub fn new(limiter: RateLimiter, extractor: K) -> RateLimitLayer<K>
    pub fn shared(limiter: Arc<RateLimiter<C>>, extractor: K) -> RateLimitLayer<K, C>
    pub fn with_cost(self, cost: u32) -> Self
    pub fn with_policy(self, name: impl Into<String>) -> Self
    pub fn with_headers_on_success(self, enabled: bool) -> Self
    pub fn with_impossible_status(self, status: http::StatusCode) -> Self
    pub fn with_anonymous(self, anonymous: Anonymous) -> Self
    pub const fn limiter(&self) -> &Arc<RateLimiter<C>>

    #[non_exhaustive]
    pub enum Anonymous { Shared, Key(Key), Reject(http::StatusCode) }

    pub struct RateLimited<S, K, C = SystemClock>   // the Service
    pub struct ResponseFuture<F, B>                 // its future

    pub trait KeyExtractor: Clone + Send + Sync + 'static {
        fn extract(&self, request: &RequestInfo<'_>) -> Option<Key>;
    }   // also implemented for Fn(&RequestInfo<'_>) -> Option<Key>
    pub struct RequestInfo<'a>   // method(), uri(), headers(), extensions()
    pub struct PeerIp            // the peer address from the request extensions
    pub struct ForwardedFor      // X-Forwarded-For, trusting a fixed number of proxies
        // trusting(n), only_from(network: IpAddr, prefix_len: u8), with_prefixes(v4, v6)
    pub struct HeaderKey         // the value of one header
        // new(name), with_digest_over(len)
}
```

A `tower::Layer` for inbound HTTP (`http::Request` → `http::Response<B>` with
`B: Default`): an axum `Router`, a hyper service, a tonic server. Each request
is charged `cost` units (default 1; a cost of `0` counts as `1`, so it never
disables limiting) to the key its extractor picks. A request with no
extractable identity is handled by the layer's `Anonymous` policy: `Shared`
(the default) charges one shared anonymous key, which never equals an
extracted key; `Key(key)` charges the given key, which can have its own
`quota_for` tier; `Reject(status)` answers with that status and charges
nothing. The decision is made synchronously in `call`:

- **Admitted** — the inner service's own future runs (no boxing); with
  `with_headers_on_success(true)` its response also gets the rate-limit headers.
- **Denied** — the inner service is not called. The response is `429` with the
  body type's `Default`, `Retry-After` (whole seconds, rounded up), and
  `RateLimit-Limit` / `-Remaining` / `-Reset`; with `with_policy(name)` also the
  structured `RateLimit` and `RateLimit-Policy` fields, which describe the
  quota the key is actually under (its `quota_for` override, if it has one). A
  request that can never
  be admitted ([`Decision::is_impossible`](#decision)) gets
  `with_impossible_status` (default `429`) and no `Retry-After`.

Extractors: `PeerIp` keys by the connection's address (an `IpAddr` or
`SocketAddr` the server put in the request extensions), IPv4 exactly and IPv6
by `/64` (`with_prefixes` to change). `ForwardedFor::trusting(n)` takes the
`X-Forwarded-For` entry `n` places from the right — the address your `n`th
proxy saw; a header with fewer entries yields no key. That entry was written by
your proxy only if the request came through your proxies: a client that can
reach the service directly writes the whole header itself. Add
`.only_from(network, prefix_len)` (repeatable) to use the header only when the
connection's peer address is inside one of the proxies' networks — otherwise
the peer address itself is the key. IPv6 entries may be bracketed and carry a
zone (`[fe80::1%eth0]`), and IPv4-mapped peers match IPv4 networks.
`HeaderKey` keys by a header's value, stored verbatim up to 64 bytes and as a
16-byte keyed digest beyond (`with_digest_over` to change), so a client cannot
fill memory with huge values. With axum, read `ConnectInfo<SocketAddr>` in a
closure extractor.

```rust
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use http::{Request, Response, StatusCode};
use rate_net::RateLimiter;
use rate_net::tower::{PeerIp, RateLimitLayer};
use tower::{Layer, Service, ServiceExt, service_fn};

async fn demo() {
    let layer = RateLimitLayer::new(RateLimiter::per_minute(1), PeerIp::default())
        .with_policy("default");
    let mut service = layer.layer(service_fn(|_: Request<String>| async {
        Ok::<_, Infallible>(Response::new(String::new()))
    }));
    let request = || {
        let mut request = Request::new(String::new());
        request.extensions_mut().insert(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        request
    };
    let first = service.ready().await.unwrap().call(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = service.ready().await.unwrap().call(request()).await.unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(second.headers()["ratelimit-policy"], r#""default";q=1;w=60"#);
}

tokio::runtime::Builder::new_current_thread()
    .enable_time()
    .build()
    .unwrap()
    .block_on(demo());
```

---

## The `no_std` core

With `default-features = false` the crate is `no_std` and exposes
[`Gcra`](#gcra) and [`GcraParams`](#gcraparams) (on targets with 64-bit
atomics), [`Quota`](#quota),
[`Decision`](#decision), [`Snapshot`](#snapshot), [`headers`](#headers),
[`RateLimiterError`](#ratelimitererror),
[`InsufficientCapacity`](#insufficientcapacity), and `VERSION`. Nothing
allocates. On a target without 64-bit atomics (for example
`thumbv7em-none-eabihf`) the crate still builds, without `Gcra` and
`GcraParams`.

```toml
[dependencies]
rate-net = { version = "2.1", default-features = false }
```

---

## Sharding and eviction

Per-key state lives in a sharded store: the key is hashed once, one set of hash
bits picks one of a power-of-two number of shards, and another finds the key's
slot in that shard's index. Each shard is an independent `RwLock` on its own
cache lines. An existing-key check takes only the read lock; only first-seeing a
key takes the write lock.

Eviction runs only while inserting a new key and does bounded work:

- **Capacity — CLOCK, recovered keys first.** Every check sets the key's
  referenced bit (only if it was clear). At capacity a hand sweeps the shard's
  slots, clearing bits, and evicts the first unreferenced (or idle-expired) key
  whose allowance has fully recovered; failing that, the first unreferenced key;
  failing that, the key under the hand. It examines at most 32 slots. A key seen
  once (a flood) goes first; a key checked at least once per revolution stays.
- **Idle TTL — incremental.** Each insert examines the next 8 slots from a
  second cursor and reclaims idle ones.

Removal is a swap-remove plus two index updates, so eviction is O(1), but a shard's index and slot vector still grow (and
the index occasionally rehashes) in one pass under the shard's write lock,
amortized over many inserts. To keep that pass short the store raises its shard
count until no shard's share of the capacity cap exceeds 16,384 keys, so a
growth or rehash touches at most that many entries (a few milliseconds at
worst, against ~200 ms for one 1M-key shard). An unbounded store keeps the
requested shard count; give it enough shards.

The cap is enforced per shard (`max_keys / shards`, rounded up), so the
live count stays within one key per shard of the configured maximum.

**Eviction does not reset a limit.** When an evicted key still owes part of its
allowance, its shard records the debt — units spent, and when they would all
be back. A key inserted before its debt is repaid resumes **no more permissive
than it left**: the token and leaky buckets resume exactly where they were (the
deadline rounded up to the millisecond, against the key); the window
algorithms resume with the spent units charged at the return, so they may be
held back for up to one window longer.

The debts live in a per-shard recovery table, allocated the first time the
shard evicts an unrecovered key and released by `retain_recent` once every
debt has expired. The **exact records** are 16 bytes each, two per key of
capacity, in a cuckoo-hashed table: each key has two candidate buckets of four
records, and a full pair is made room in by moving records to their other
bucket (at most 16 moves), which fills the table past 90 % before anything is
left over. A key reads back only its own record. While a shard's still-limited
evicted keys fit — about twice its share of the cap at once; the binding
quantity is the rate at which still-limited keys are evicted times their
recovery time, not the size of the population — no evicted key is reset, no
other key is charged, and nothing is refused.

Past that bound, [`Overflow`](#eviction) decides:

- **`Overflow::FailClosed`** (the default) spills it into a shared **overflow sketch** of four
  8-byte cells per key of capacity; each cell keeps the larger deadline and
  spend (a spend beyond the cell's 24 bits is charged as everything). A key
  being stored is charged the smaller of its two cells while both are live:
  never less than its own spilled debt, but a key that never owed anything is
  charged when both its cells hold others' debts — it starts partly or wholly
  drained. A key charged from the sketch is never written back into it (its
  debt goes only into the exact table, marked so it is never moved out), so
  sketch charges cannot spread or ratchet; if its candidate records all hold
  such marked debts it is not evicted, and a full shard with nothing else to
  evict **refuses the newcomer** with a retry time (`Metrics::keys_refused`).
- **`Overflow::Forget`** (opt-out) drops the debt that is left over. That
  key may come back with a fresh allowance, as every evicted key did in 1.x;
  nothing else changes, and `Metrics::debts_forgotten` counts the drops. Under
  a sustained flood that evicts far more still-limited keys than the table
  holds, a targeted key can be reset again and again.

Measured with the re-audit's harness (`docs/BENCHMARKS.md`; ManualClock,
128 shards; cap 100,000 for floods, 10,000 for populations):

| | `FailClosed` (default) | `Forget` |
|---|---:|---:|
| Newcomers denied after a drain flood of 3x / 4x / 6x / 9x the cap | 0 / 3 / 38 / 86 % | 0 / 0 / 0 / 0 % |
| False denials, well-behaved population of 10x the cap at 5,000 / 20,000 visits/s | 4 / 72 % | 0 / 0 % |
| Evicted, still-limited keys reset past the bound | never | yes |
| Recovery tables at the default cap | 64 MiB | 32 MiB |

The recovery tables cost 32 bytes per key of capacity, plus 32 more for
`FailClosed`'s sketch (up to twice that when a shard's share of the cap is
just past a power of two).

The idle TTL is raised to the quota's recovery time, so idle expiry normally
removes only recovered keys; a key with a longer per-key quota that expires
while still limited has its debt recorded like any other eviction — the eviction judges such a key by its current
state, not by a recovery time cached before it last spent. `remove`, `reset`,
and `clear` forget a key's own record — they are explicit resets — but a
sketch charge is shared and cannot be cleared for one key (it expires within
one recovery period; `clear` drops the whole table). A sketch charge carries
the debt's own deadline, so under per-key tiers a slow tier's long debt can
drain a fast-tier newcomer for that long.

### Sizing the cap

The overflow policy only matters when more still-limited keys are evicted
within one recovery period than about twice the cap — roughly three times the
cap's worth of keys limited at once. Size the cap so that never happens:

- **Rule:** `cap >= peak distinct keys active within the longest recovery
  window × headroom` (1.5–2x). The recovery window is `burst / limit` periods
  for the token and leaky buckets, one period for the fixed window and the
  sliding-window log, and two for the sliding-window counter.
- **Memory per key of capacity:** about 138 bytes of token-bucket state, plus
  under eviction pressure 64 bytes of recovery tables with `FailClosed` or 32
  with `Forget` — about 202 or 170 bytes (202 or 170 MiB at the default cap),
  plus key bytes beyond 22.
- **Operate it:** export `recovery_load()` and alert at about `0.7`
  utilization (or `fullest_shard`); alert on any `keys_refused`,
  `debts_overflowed`, or `debts_forgotten`; enable `tracing` for the
  warnings; keep `FailClosed` for identity- or credential-keyed limiters and
  consider `Forget` for anonymous per-IP load shedding; keep an upstream DDoS
  layer in front.
- **Compared with:** NGINX `limit_req` keeps state in a fixed-size zone and,
  when it is full, removes the least recently used state (even a client still
  being limited, which then starts fresh — like `Forget`), rejecting the
  request only if no state can be created. `governor`'s keyed limiters grow
  their state map without bound until you call `retain_recent()` (which
  drops only recovered keys): no resets, but no memory bound either.

---

## Algorithms

| Algorithm | Feature | Bound per key | Notes |
|-----------|---------|---------------|-------|
| Token bucket | always | `burst + rate × elapsed` | Default; delegates to `better-bucket`. Bursts to `burst`, then sustains the rate. |
| Leaky bucket | `algorithms` | `burst + rate × elapsed` | GCRA; spaces units at the emission interval. Exact at any rate up to one per nanosecond. |
| Fixed window | `algorithms` | `limit` per aligned window | One packed atomic; tolerates a boundary burst (up to `2 × limit` across a boundary). |
| Sliding-window log | `algorithms` | `limit` in any rolling window | Exact, no boundary burst; up to `16 × limit` bytes per key. |
| Sliding-window counter | `algorithms` | worst case `2 × limit` in a rolling window | O(1) weighted two-window blend; approximate. |

Each bound holds under concurrency and on a real clock; each algorithm has a
`proptest` over-admit property and a real-clock contention test, and the token
bucket, leaky bucket, and fixed window have `loom` models of their stale-timestamp
races.

---

## Limits and ceilings

- **Token bucket:** no capacity ceiling (as of `better-bucket` 1.1); retry
  hints have millisecond resolution, rounded up.
- **Leaky bucket:** at most one unit per nanosecond (`try_build` rejects more,
  `build` clamps).
- **Fixed window:** any `u32` limit.
- **Sliding-window log:** `try_build` caps the limit at `MAX_SLIDING_LOG_LIMIT`
  (65,536) for memory; `build` does not.
- **Keys:** a token-bucket key is 128 bytes in its shard — about 138 bytes of
  heap per key at the default cap, up to ~210 just after a shard's tables have
  doubled — so the default cap bounds the store at roughly 140 MiB, plus the
  recovery tables under eviction pressure (64 MiB with the default
  `Overflow::FailClosed`, 32 MiB with `Forget`: about 202 or 170 MiB in all),
  plus the bytes of any key
  longer than 22 (bounded with `max_key_len`, or fixed at 16 with
  `digest_keys_longer_than`). `tests/memory.rs` asserts a 256 B/key budget.

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | The keyed limiter, `DirectLimiter`, and `CoarseClock`. Off, the crate is `no_std` ([the `no_std` core](#the-no_std-core)). |
| `algorithms` | no  | The leaky bucket and the window algorithms, and their [`Algorithm`](#algorithm) variants. |
| `async`      | no  | [`AsyncLimiter`](#asynclimiter) and `DirectLimiter`'s awaiting methods. Implies `std`; only this layer touches a runtime (`tokio`'s timer). |
| `metrics`    | no  | `metrics()` on `RateLimiter` and `DirectLimiter` ([`Metrics`](#metrics-1)) and `RateLimiter::recovery_load()`. Implies `std`; compiled out when off. |
| `tracing`    | no  | Rate-limited `tracing` warnings (target `rate_net::eviction`) on spills, drops, and saturation refusals. Implies `std`; adds `tracing` without its proc-macros; nothing on the check path. |
| `forge`      | no  | `error_forge::ForgeError` on [`RateLimiterError`](#ratelimitererror) and `InsufficientCapacity`. Implies `std`; adds `error-forge`. |
| `tower`      | no  | The [`tower` middleware](#tower-middleware). Implies `std`; adds `http`, `tower-layer`, `tower-service`, and `pin-project-lite` (not `tower` itself). |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
