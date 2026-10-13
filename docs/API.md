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
> **Stable (`v1.1.0`).** The public surface is frozen until `2.0`; `1.1` adds
> to it without changing anything `1.0` offered (one method is deprecated). Every
> public type is asserted `Send + Sync + 'static` at compile time. The leaky
> bucket and window algorithms require the `algorithms` feature, `AsyncLimiter`
> and the `try_until_ready` methods require `async`, and `metrics()` requires
> `metrics`. With `std` off, only the [`no_std` core](#the-no_std-core) remains.

## Table of Contents

- [Overview](#overview)
- [Public API](#public-api)
  - [`RateLimiter`](#ratelimiter)
    - [Constructors and adjusters](#constructors-and-adjusters)
    - [`check` / `check_n`](#check--check_n)
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
  - [`Metrics`](#metrics-1) _(feature: `metrics`)_
  - [`RateLimiterError`](#ratelimitererror)
  - [`InsufficientCapacity`](#insufficientcapacity)
  - [Constants](#constants)
- [The `no_std` core](#the-no_std-core)
- [Sharding and eviction](#sharding-and-eviction)
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
window; a short per-key mutex for the sliding-window algorithms). It never
allocates. The first check of a new key takes the shard write lock and may
evict one other key ([Sharding and eviction](#sharding-and-eviction)).

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

#### `peek`

```rust ignore
pub fn peek(&self, key: impl Into<Key>) -> Snapshot
```

Reads `key`'s allowance without consuming any, returning a
[`Snapshot`](#snapshot) (capacity, remaining, time until whole). It takes only
the shard read lock, never creates state (an unseen key reports its full
allowance), and does not refresh the key's eviction recency. Advisory under
concurrency.

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

`reset` and `remove` return whether the key had state. `retain_recent` removes
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
pub const fn algorithm(&self) -> Algorithm
pub fn shards(&self) -> usize
pub const fn eviction(&self) -> Eviction
pub fn tracked_keys(&self) -> usize
```

`quota` and `eviction` report the configuration as given (an idle TTL raised
to the recovery time is reported unraised). `tracked_keys` is a momentary,
advisory count, bounded by the eviction policy.

```rust
use rate_net::{Algorithm, RateLimiter};

let limiter = RateLimiter::per_second(50).with_shards(16);
assert_eq!(limiter.quota().limit(), 50);
assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
assert_eq!(limiter.shards(), 16);
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
  [`MAX_SLIDING_LOG_LIMIT`](#constants)), `IdleTtlTooShort` (an idle TTL below
  the quota's recovery time — `burst / limit` periods for the buckets, one period
  for the fixed window and log, two for the sliding-window counter).
- **`build`** never fails and **fails closed**: a zero limit, period, or burst
  denies everything for every algorithm; a too-fast leaky-bucket rate is clamped
  to one unit per nanosecond; a short idle TTL is raised to the recovery time.
- **`try_build_direct` / `build_direct`** build a [`DirectLimiter`](#directlimiter)
  from the same algorithm, quota, burst, and clock (shards and eviction are
  ignored).

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
pub fn metrics(&self) -> Metrics                                                 // feature `metrics`
```

One allowance, no keys: no hash, no shard lookup, no lock. For client-side
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
#[deprecated] pub async fn until_ready(&self, key: impl Into<Key>)
#[deprecated] pub async fn until_ready_n(&self, key: impl Into<Key>, n: u32)
```

An await-until-ready wrapper. `try_until_ready_n` checks, and on a denial sleeps
for the reported `retry_after` (via `tokio::time::sleep`) and checks again,
returning `Ok(())` only once the units have been counted. A request that can
never fit returns [`InsufficientCapacity`](#insufficientcapacity) at once. It is
cancel-safe: dropping the future consumes nothing.

The deprecated `until_ready` / `until_ready_n` return *without admitting* when
the request can never fit, which callers cannot tell from an admission; they
remain until `2.0`. Waiting needs a clock that advances; under a frozen
`ManualClock` the wait never ends.

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

pub trait DynLimiter {
    fn check_key(&self, key: Key, n: u32) -> Decision;
}
```

`Limiter` is the shared surface generic code depends on; implementors provide
`check_n`. It is implemented for `RateLimiter` and for `Arc<L>` of any limiter
(not for `&L` or `Box<L>`: code written against 1.0 may implement `Limiter`
for references and boxes of its own types, and blanket impls would conflict). Its methods are generic, so `dyn Limiter` is not
possible; `DynLimiter` is the object-safe face: every `Limiter` implements it,
and `dyn DynLimiter` (with or without `+ Send + Sync`) implements `Limiter`
again, so the generic helpers stay available on trait objects.

```rust
use rate_net::{DynLimiter, Limiter, RateLimiter};
use std::sync::Arc;

fn admit_one<L: Limiter>(limiter: &L, key: &str) -> bool {
    limiter.check(key).is_allow()
}

let limiter: Arc<dyn DynLimiter + Send + Sync> = Arc::new(RateLimiter::per_second(2));
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
```

A read-only view of one key's allowance, from `RateLimiter::peek`,
`DirectLimiter::peek`, or `Gcra::peek_at`. A limiter that admits nothing reports
`limit() == 0` and `reset_after() == Duration::MAX`. `headers` returns
`RateLimit-Limit`, `RateLimit-Remaining`, and `RateLimit-Reset`.

```rust
use rate_net::RateLimiter;

let limiter = RateLimiter::per_second(10);
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
}
```

Header names for rate-limit responses: `Retry-After` (RFC 9110) and the three
`RateLimit-*` fields of the IETF rate-limit headers draft. rate-net hands back
names and numbers; the caller writes them into its framework's response type.

```rust
use rate_net::{RateLimiter, headers};

let limiter = RateLimiter::per_minute(1);
let _ = limiter.check("client");
let decision = limiter.check("client");
let retry = decision.retry_after_secs().map(|secs| (headers::RETRY_AFTER, secs));
assert_eq!(retry, Some(("Retry-After", 60)));
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
```

A rate limit: `limit` units per `period`, per key, with a `burst` ceiling
(defaulting to `limit`). Under the buckets a key starts with `burst` units and
regains `limit` per `period`; the window algorithms admit at most `limit` per
`period` and ignore `burst`. `per_second` / `per_minute` are infallible (a `0`
limit admits nothing); `rate` rejects `ZeroQuota` and `ZeroPeriod`. Available
without `std`.

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
```

How the store bounds its memory — the defense against a unique-key flood. A
**capacity** cap evicts by CLOCK when a new key would exceed it (spread evenly
over the shards, at least one key per shard, so `capacity(0)` behaves as one per
shard); an **idle TTL** reclaims keys not seen for that long, raised to the
quota's recovery time so it never resets a key early. The `Default` is a
[`DEFAULT_MAX_KEYS`](#constants) cap and no TTL. `unbounded` is only safe when
the key space is intrinsically small.

```rust
use rate_net::{Eviction, DEFAULT_MAX_KEYS};
use std::time::Duration;

let policy = Eviction::capacity(100_000).with_idle(Duration::from_secs(300));
assert_eq!(policy.max_keys(), Some(100_000));
assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(300)));
assert_eq!(Eviction::default().max_keys(), Some(DEFAULT_MAX_KEYS));
assert_eq!(Eviction::unbounded().without_capacity().max_keys(), None);
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

impl From<&str> | From<String> | From<&String> | From<&[u8]> | From<Vec<u8>>
   | From<u64> | From<IpAddr> | From<Ipv4Addr> | From<Ipv6Addr>
   | From<SocketAddr> | From<&Key> for Key
```

The identity a limit is tracked against, stored inline up to 22 bytes (no
allocation) and on the heap beyond. The `From` conversions make **raw** keys —
identity is the bytes alone, so `Key::from(1u64)` equals the equivalent 8-byte
slice. The constructors make **tagged** keys whose kind is part of the
identity, so they never collide with raw keys or each other: `id` for numeric
ids, `ip` for a canonical address (IPv4-mapped IPv6 equals the IPv4 address),
and the `*_prefix` constructors for networks (lengths clamp to the address
width and are part of the key). `From<SocketAddr>` drops the port. `Debug`
shows the kind and length only.

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
Readings never run ahead of real time and lag by at most one tick, so a limiter
on it is slightly stricter, never looser.

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
}
```

Cumulative counters since construction, kept per shard beside the shard lock
(one relaxed increment per check) and summed on read. Export them as monotonic
counters. `keys_evicted` should normally be zero: a steady rate means the cap is
below the live key population or the limiter is under a unique-key flood (a
`keys_inserted` spike is the other signature). Without the feature the counters
do not exist.

---

### `RateLimiterError`

```rust ignore
#[non_exhaustive]
pub enum RateLimiterError {
    ZeroQuota, ZeroPeriod, ZeroBurst, RateTooHigh, LimitTooLarge, IdleTtlTooShort,
}
```

A configuration rejected by `Quota::rate`, `Builder::try_build`,
`Builder::try_build_direct`, or `Gcra::try_new`. Implements `Display`,
`core::error::Error`, and (with `std`)
[`error_forge::ForgeError`](https://crates.io/crates/error-forge) — `kind()` is
the variant name, `is_retryable()` is `false`. `#[non_exhaustive]`.

| Variant | Meaning |
|---------|---------|
| `ZeroQuota` | The limit was `0`. |
| `ZeroPeriod` | The period was zero. |
| `ZeroBurst` | The burst was `0` (token / leaky bucket). |
| `RateTooHigh` | A leaky bucket faster than one unit per nanosecond. |
| `LimitTooLarge` | A sliding-window log limit above `MAX_SLIDING_LOG_LIMIT`. |
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
pub const MIN_COARSE_RESOLUTION: Duration;       // 100 µs: CoarseClock::spawn floor
```

```rust
assert_eq!(rate_net::VERSION.split('.').count(), 3);
assert_eq!(rate_net::DEFAULT_MAX_KEYS, 1_048_576);
assert_eq!(rate_net::MAX_SLIDING_LOG_LIMIT, 65_536);
assert_eq!(rate_net::MIN_COARSE_RESOLUTION, std::time::Duration::from_micros(100));
```

---

## The `no_std` core

With `default-features = false` the crate is `no_std` and exposes
[`Gcra`](#gcra) (on targets with 64-bit atomics), [`Quota`](#quota),
[`Decision`](#decision), [`Snapshot`](#snapshot), [`headers`](#headers),
[`RateLimiterError`](#ratelimitererror),
[`InsufficientCapacity`](#insufficientcapacity), and `VERSION`. Nothing
allocates. On a target without 64-bit atomics (for example
`thumbv7em-none-eabihf`) the crate still builds, without `Gcra`.

```toml
[dependencies]
rate-net = { version = "1.1", default-features = false }
```

---

## Sharding and eviction

Per-key state lives in a sharded store: the key is hashed once, one set of hash
bits picks one of a power-of-two number of shards, and another finds the key's
slot in that shard's index. Each shard is an independent `RwLock` on its own
cache lines. An existing-key check takes only the read lock; only first-seeing a
key takes the write lock.

Eviction runs only while inserting a new key and does bounded work:

- **Capacity — CLOCK.** Every check sets the key's referenced bit (only if it
  was clear). At capacity a hand sweeps the shard's slots, clearing bits, and
  evicts the first key whose bit was already clear; it examines at most 32 slots
  and evicts under the hand if all were recently used. A key seen once (a flood)
  goes first; a key checked at least once per revolution stays.
- **Idle TTL — incremental.** Each insert examines the next 8 slots from a
  second cursor and reclaims idle ones.

Removal is a swap-remove plus two index updates, so the whole insert path is
O(1). The cap is enforced per shard (`max_keys / shards`, rounded up), so the
live count stays within one key per shard of the configured maximum. An
evicted key that returns starts fresh; the idle TTL never causes that, because
it is raised to the quota's recovery time.

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
- **Keys:** per-key heap is about 200–300 bytes for the token bucket (~202 B
  measured at the default cap), so the default cap bounds the store at roughly
  200 MiB. `tests/memory.rs` asserts a 512 B/key budget.

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | The keyed limiter, `DirectLimiter`, `CoarseClock`, and the `error-forge` integration. Off, the crate is `no_std` ([the `no_std` core](#the-no_std-core)). |
| `algorithms` | no  | The leaky bucket and the window algorithms, and their [`Algorithm`](#algorithm) variants. |
| `async`      | no  | [`AsyncLimiter`](#asynclimiter) and `DirectLimiter::try_until_ready`. Implies `std`; only this layer touches a runtime (`tokio`'s timer). |
| `metrics`    | no  | `metrics()` on `RateLimiter` and `DirectLimiter` ([`Metrics`](#metrics-1)). Implies `std`; compiled out when off. |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
