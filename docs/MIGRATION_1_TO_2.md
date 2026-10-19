<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br><b>rate-net</b><br>
    <sub><sup>MIGRATING FROM 1.x TO 2.0</sup></sub>
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

Every change from `1.1.0` (the last 1.x release) that you can observe, and what
to do about it. Most code compiles and runs unchanged: the three breaking
changes are a default behaviour under heavy eviction, an optional feature that
used to be implied, and a `Send + Sync` bound. Everything else is additive.

```toml
[dependencies]
rate-net = "2"
```

## Breaking changes

### 1. Evicted, still-limited keys keep their debt — and by default no flood resets them

**1.x:** when the store reached its capacity cap, the key it evicted was
forgotten. If that key was still being limited — it had spent part of its
allowance — it came back with a fresh one. Flooding the limiter with throwaway
keys therefore reset any key's limit (audit RN-005).

**2.0:** the evicted key's debt (units spent, and when they would all be back)
is recorded, and the key resumes no more permissive than it left. Debts are
kept exactly, without affecting any other key, while the store's still-limited
evicted keys fit about twice the cap. Past that bound, the overflow policy
decides, and the default is `Overflow::FailClosed`:

| | `Overflow::FailClosed` (2.0 default) | `Overflow::Forget` (opt-out; 1.x-like) |
|---|---|---|
| Evicted, still-limited keys past the bound | never come back looser than they left | may come back with a fresh allowance |
| Keys that never owed anything | may start partly or wholly drained, or be refused with a retry time | never charged, never refused |

Below the bound — a cap sized for the traffic — the two are identical, and
nothing a 1.x user relied on changes except that limits now survive eviction.

**What to do:**

1. Size the cap so the bound is never reached:
   `cap >= peak distinct keys active within the longest recovery window × 1.5–2`
   (the recovery window is `burst / limit` periods for the token and leaky
   buckets, one period for the fixed window and sliding-window log, two for
   the sliding-window counter).
2. Watch it. With the `metrics` feature, export `recovery_load()` every few
   seconds and alert at about 0.7 `utilization()` (or `fullest_shard`), and
   alert on any `Metrics::keys_refused`. With the `tracing` feature the limiter
   logs a rate-limited warning when debts spill, are dropped, or newcomers are
   refused.
3. Keep `FailClosed` for limiters keyed by identities or credentials
   (usernames, API keys, accounts). For availability-first limiters — anonymous
   per-IP load shedding — opt back into 1.x behaviour past the bound:

```rust
// 1.x: nothing to choose; eviction forgot every key.
use rate_net::{Eviction, RateLimiter};
let limiter = RateLimiter::builder()
    .per_second(50)
    .eviction(Eviction::capacity(1_000_000))
    .build();
assert!(limiter.check("203.0.113.7").is_allow());
```

```rust
// 2.0: the same availability-first behaviour past the bound.
use rate_net::{Eviction, Overflow, RateLimiter};
let limiter = RateLimiter::builder()
    .per_second(50)
    .eviction(Eviction::capacity(1_000_000).with_overflow(Overflow::Forget))
    .build();
assert_eq!(limiter.eviction().overflow(), Overflow::Forget);
```

`Eviction::default()`, `capacity`, `idle`, `new`, and `unbounded` all start
from `Overflow::FailClosed`; `with_overflow` changes it. The recovery tables
cost memory only under eviction pressure: 64 bytes per key of capacity with
`FailClosed`, 32 with `Forget` (at the default cap, about 202 or 170 MiB in
all, against about 140 MiB idle).

### 2. `error-forge` is behind the `forge` feature

**1.x:** the `std` feature (on by default) pulled in `error-forge`, and
`RateLimiterError` and `InsufficientCapacity` implemented
`error_forge::ForgeError`.

**2.0:** that implementation needs the `forge` feature, off by default. The
default build has 8 normal dependencies instead of 16.

```toml
# 1.x
rate-net = "1.1"

# 2.0, if you call `kind()` / `caption()` / `is_retryable()` through ForgeError
rate-net = { version = "2", features = ["forge"] }
```

Code that only uses `Display` or `std::error::Error` on these types needs
nothing.

### 3. `DynLimiter` requires `Send + Sync`

**1.x:** `pub trait DynLimiter { … }`, so a shared trait object had to spell
the bounds: `Arc<dyn DynLimiter + Send + Sync>`.

**2.0:** `pub trait DynLimiter: Send + Sync { … }`. Every `Send + Sync`
`Limiter` still implements it automatically.

```rust
use rate_net::{DynLimiter, Limiter, RateLimiter};
use std::sync::Arc;

// 1.x (still compiles in 2.0):
let a: Arc<dyn DynLimiter + Send + Sync> = Arc::new(RateLimiter::per_second(1));
// 2.0: the bounds are implied.
let b: Arc<dyn DynLimiter> = Arc::new(RateLimiter::per_second(1));
assert!(a.check("k").is_allow() && b.check("k").is_allow());
```

Only a `Limiter` you implemented for a type that is not `Send + Sync` loses
`DynLimiter`; hold it through `Limiter` directly, or make it thread-safe.

## Other changes you can observe

- **`reset`, `remove`, `clear`** also forget a recorded debt of the key (and
  `clear` the whole recovery table). Under `FailClosed`, a shared overflow
  charge cannot be cleared for one key; it expires within one recovery period.
- **A key whose quota admits nothing** (a zero limit, period, or bucket burst)
  is denied and never stored, without taking a shard lock.
- **`retain_recent`** also releases a recovery table whose debts have all
  expired.
- **Memory:** a token-bucket key is about 138 bytes of heap at the default cap
  (1.1: about 202). `tests/memory.rs` asserts 256 bytes per key.
- **Dependencies:** `better-bucket` is used without features (only its
  clock-free `BucketCore`), so `clock-lib` and `error-forge` are no longer
  pulled in twice; `tracing` is optional (feature `tracing`, without proc
  macros).
- **`Metrics`** (feature `metrics`) has new fields — `keys_remembered`,
  `keys_restored`, `debts_forgotten`, `debts_overflowed`, `keys_refused`,
  `keys_rejected`. It is `#[non_exhaustive]`, so reading fields is unaffected.
- **The idle TTL** is still raised to the default quota's recovery time; a key
  under a slower per-key quota that expires anyway has its debt recorded.

## New in 2.0 (additive)

- **Borrowed keys:** `check_bytes`, `check_n_bytes`, `peek_bytes` check a key
  of any length without allocating.

  ```rust
  use rate_net::RateLimiter;
  let limiter = RateLimiter::per_second(10);
  let token = "9f6c1d2e-8b7a-4c3d-9e1f-0a2b3c4d5e6f";
  // 1.x: limiter.check(token) — copies a key over 22 bytes on every call.
  assert!(limiter.check_bytes(token).is_allow());
  ```

- **Per-key quotas and key-length policies:** `Builder::quota_for`,
  `max_key_len`, `digest_keys_longer_than`, and `RateLimiter::quota_of`.
- **HTTP:** `Snapshot::ratelimit` and `Quota::ratelimit_policy` for the IETF
  structured `RateLimit` / `RateLimit-Policy` fields; the `tower` feature's
  `RateLimitLayer` with `PeerIp`, `ForwardedFor` (and `only_from`),
  `HeaderKey`, closures, and an `Anonymous` policy.
- **Async:** `until_ready_within` / `until_ready_n_within` and `with_jitter` on
  `AsyncLimiter` and `DirectLimiter`.
- **Eviction policy and guardrails:** `Eviction::with_overflow`, `Overflow`,
  `RateLimiter::recovery_load` / `RecoveryLoad`, and the `tracing` feature.
- **Keys:** `Key::len`, `Key::is_empty`, `From<[u8; N]>` and `From<&[u8; N]>`.

## Not changed

- The `Limiter` trait, `RateLimiter` constructors, `check` / `check_n` /
  `peek`, `Decision`, `Snapshot`, `Quota`, `Algorithm`, `Key` conversions, the
  `no_std` `Gcra` core, `CoarseClock`, `DirectLimiter`, and the default
  capacity cap (1,048,576 keys) are as in 1.1.
- MSRV is still Rust 1.85.
- The two methods deprecated in 1.1 are still there, still deprecated.

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
