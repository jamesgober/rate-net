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
> **Status: pre-1.0 (`v0.2.0`).** The public shape is locked and the Tier-1
> token-bucket limiter works today. Items under [Public API](#public-api) are
> callable now; sections marked _(planned)_ describe the intended surface and
> are filled in as each roadmap phase ships — the Tier-2 builder, bounded-memory
> eviction, and the additional algorithms land across `0.3.0`–`0.4.0`.

## Table of Contents

- [Overview](#overview)
- [Public API](#public-api)
  - [`RateLimiter`](#ratelimiter)
    - [`per_second`](#ratelimiterper_second)
    - [`per_minute`](#ratelimiterper_minute)
    - [`with_quota`](#ratelimiterwith_quota)
    - [`with_clock`](#ratelimiterwith_clock)
    - [`check`](#ratelimitercheck)
    - [`check_n`](#ratelimitercheck_n)
    - [`quota` / `algorithm` / `tracked_keys`](#ratelimiter-introspection)
  - [`Limiter` trait](#limiter-trait)
  - [`Decision`](#decision)
  - [`Quota`](#quota)
  - [`Algorithm`](#algorithm)
  - [`RateLimiterError`](#ratelimitererror)
  - [`Key`](#key)
  - [`VERSION`](#version)
- [Tier 2 — the configured path](#tier-2--the-configured-path) _(planned: 0.4)_
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

<h4 id="ratelimiter-introspection">Introspection: <code>quota</code> / <code>algorithm</code> / <code>tracked_keys</code></h4>

```rust
pub fn quota(&self) -> Quota
pub const fn algorithm(&self) -> Algorithm
pub fn tracked_keys(&self) -> usize
```

- `quota` — the [`Quota`](#quota) every key is limited to.
- `algorithm` — the [`Algorithm`](#algorithm) in force (currently always
  `TokenBucket`).
- `tracked_keys` — the number of keys with live state; a momentary, advisory
  snapshot, exposed mainly for tests and diagnostics.

```rust
use rate_net::{RateLimiter, Algorithm};

let limiter = RateLimiter::per_second(50);
assert_eq!(limiter.quota().limit(), 50);
assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
assert_eq!(limiter.tracked_keys(), 0);
let _ = limiter.check("a");
assert_eq!(limiter.tracked_keys(), 1);
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

A rate limit: `limit` requests per `period`, per key. Under the token bucket a
key starts with a full allowance of `limit`, spends one unit per admitted
request, and accrues the allowance back over `period`.

Constructors:

- `per_second(limit: u32) -> Quota` — infallible; `limit = 0` admits nothing.
- `per_minute(limit: u32) -> Quota` — infallible.
- `rate(limit: u32, period: Duration) -> Result<Quota, RateLimiterError>` —
  validated for arbitrary windows.

Accessors: `limit(&self) -> u32`, `period(&self) -> Duration`.

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

### `Algorithm`

```rust
#[non_exhaustive]
pub enum Algorithm {
    TokenBucket,          // default; the only variant wired in 0.2
    LeakyBucket,          // planned: 0.4
    FixedWindow,          // planned: 0.4
    SlidingWindowLog,     // planned: 0.4
    SlidingWindowCounter, // planned: 0.4
}
```

Selects the algorithm a limiter applies; the selector a future builder uses to
pick between them. `#[non_exhaustive]` and `Default` (`TokenBucket`). The
remaining variants name the surface that lands in `0.4`.

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
token, a route. Stored as owned bytes; two keys are equal exactly when their
bytes are equal, so the identity is the byte content, not the source type. You
rarely name it directly: `check` accepts `impl Into<Key>`.

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

## Tier 2 — the configured path

_Planned: 0.4._ A builder selecting algorithm, quota, burst, shard count, and
eviction policy, plus clock injection. Until it lands, use
[`RateLimiter::with_quota`](#ratelimiterwith_quota) with
[`Quota::rate`](#quota) for explicit per-key rates.

---

## Algorithms

| Algorithm | Status | Notes |
|-----------|--------|-------|
| Token bucket | **shipped (0.2)** | Default; delegates to `better-bucket`. |
| Leaky bucket | planned: 0.4 | Constant-drain shaping. |
| Fixed window | planned: 0.4 | Cheapest; boundary-burst tolerant. |
| Sliding-window log | planned: 0.4 | Exact; higher memory. |
| Sliding-window counter | planned: 0.4 | Weighted two-window blend. |

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | Standard library. Enables the limiter — the sharded store (`dashmap`), the token-bucket core (`better-bucket`'s `clock` feature), the injectable clock (`clock-lib`), and the error type (`error-forge`). With it off the crate is `no_std` and exposes only [`VERSION`](#version). |
| `algorithms` | no  | The full suite beyond the default token bucket. _(wired in 0.4)_ |
| `async`      | no  | Optional additive async-friendly wrapper. Implies `std`. |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
