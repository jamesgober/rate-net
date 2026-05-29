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
> **Status: pre-1.0.** This document tracks the API surface as it lands across
> the `0.x` series. The [Public API](#public-api) section documents everything
> callable in the current release; sections marked _(planned)_ describe the
> intended surface and are filled in as each roadmap phase ships. As of
> `v0.1.0` (the scaffold) the only callable item is [`VERSION`](#version).

## Table of Contents

- [Overview](#overview)
- [Public API](#public-api)
  - [`VERSION`](#version)
- [Tier 1 — the lazy path](#tier-1--the-lazy-path)
  - [`RateLimiter::per_second`](#ratelimiterper_second) _(planned: 0.2)_
  - [`RateLimiter::check`](#ratelimitercheck) _(planned: 0.2)_
  - [`RateLimiter::check_n`](#ratelimitercheck_n) _(planned: 0.2)_
  - [`Decision`](#decision) _(planned: 0.2)_
- [Tier 2 — the configured path](#tier-2--the-configured-path)
  - [`RateLimiter::builder`](#ratelimiterbuilder) _(planned: 0.4)_
  - [`Algorithm`](#algorithm) _(planned: 0.4)_
  - [`Eviction`](#eviction) _(planned: 0.3)_
  - [`RateLimiter::with_clock`](#ratelimiterwith_clock) _(planned: 0.2)_
- [Tier 3 — the power path](#tier-3--the-power-path)
  - [`RateLimiter` trait](#ratelimiter-trait) _(planned: 0.2)_
- [Algorithms](#algorithms)
- [Errors](#errors) _(planned: 0.2)_
- [Feature flags](#feature-flags)

---

## Overview

`rate-net` answers "is this key allowed right now?" with a `Decision`
(`Allow` / `Deny { retry_after }`), across multiple algorithms, tracking
per-key state in a sharded, bounded-memory store. The common case is a
constructor plus `check`; advanced use is a builder selecting algorithm,
quota, burst, shards, and eviction.

```rust
use rate_net::{RateLimiter, Decision};

let limiter = RateLimiter::per_second(100);
match limiter.check("user:42") {
    Decision::Allow => { /* serve */ }
    Decision::Deny { retry_after } => { /* 429, Retry-After */ }
}
```

The core guarantee: **for any key and window, admitted requests never exceed
the configured quota** under any concurrent interleaving.

> The snippet above is the target Tier-1 surface. It is documented in full under
> [Tier 1](#tier-1--the-lazy-path) and becomes callable in `v0.2.0`. The only
> item callable in `v0.1.0` is [`VERSION`](#version), documented next.

---

## Public API

Everything in this section is callable in the current release.

### `VERSION`

```rust
pub const VERSION: &str;
```

The version of the crate, captured from `Cargo.toml` at compile time via
`env!("CARGO_PKG_VERSION")`. It is a `major.minor.patch` string (for example
`"0.1.0"`).

Expose it when a process needs to report the exact `rate-net` build it links
against — startup banners, `/version` or `/healthz` endpoints, structured logs,
and version-skew checks across a dependency tree. Because the value is resolved
at compile time it costs nothing at runtime and can never drift from the crate
that produced the binary.

**Parameters:** none — `VERSION` is an associated constant, not a function.

**Returns:** a `&'static str` borrowing program-static data; it is valid for the
entire lifetime of the process and never allocates.

#### Read the version

```rust
// Report the linked build at startup.
println!("rate-net {}", rate_net::VERSION);
```

#### Record the build in diagnostics

```rust
use rate_net::VERSION;

// Attach the limiter build to a structured diagnostic record so a captured
// log line is unambiguous about which release produced it.
fn limiter_build_field() -> (&'static str, &'static str) {
    ("rate_net_version", VERSION)
}

let (key, value) = limiter_build_field();
assert_eq!(key, "rate_net_version");
assert!(!value.is_empty());
```

#### Parse the components

```rust
use rate_net::VERSION;

// Split the semantic-version core into its three numeric components.
let mut parts = VERSION.split('.');
let major: u64 = parts.next().unwrap().parse().unwrap();
let minor: u64 = parts.next().unwrap().parse().unwrap();
let patch: u64 = parts.next().unwrap().parse().unwrap();

// Pre-1.0: the major component is 0 while the surface stabilises.
assert_eq!(major, 0);
let _ = (minor, patch);
```

#### Guard against version skew

```rust
use rate_net::VERSION;

// A consumer that pins behaviour to a known series can assert the linked
// crate is within the range it was tested against.
assert!(
    VERSION.starts_with("0."),
    "this integration was validated against the rate-net 0.x series, found {VERSION}",
);
```

---

## Tier 1 — the lazy path

_The one-line surface for the ~80% case. Documented in full as the `0.2`
foundation release lands. Intended signatures:_

- `RateLimiter::per_second(n: u32) -> RateLimiter` — `n` requests per second
  per key, default token-bucket algorithm.
- `RateLimiter::check(&self, key: impl Into<Key>) -> Decision` — one unit
  against `key`; never blocks, zero allocation in steady state.
- `RateLimiter::check_n(&self, key, n: u32) -> Decision` — take `n` units.
- `Decision` — `Allow` or `Deny { retry_after: Duration }`.

---

## Tier 2 — the configured path

_Builder selecting algorithm / quota / burst / shard count / eviction policy
and clock injection. Documented in full at the `0.4` release._

---

## Tier 3 — the power path

_The `RateLimiter` trait — implemented by every algorithm, and the seam custom
state stores plug into. Documented as the trait stabilises at `0.2`._

---

## Algorithms

| Algorithm | Lands | Notes |
|-----------|-------|-------|
| Token bucket | 0.3 | Default; delegates to `better-bucket`. |
| Leaky bucket | 0.4 | Constant-drain shaping. |
| Fixed window | 0.4 | Cheapest; boundary-burst tolerant. |
| Sliding-window log | 0.4 | Exact; higher memory. |
| Sliding-window counter | 0.4 | Weighted two-window blend. |

---

## Errors

_Construction-time validation returns a domain-specific error built on
`error-forge`. The `check` path returns a `Decision`, not a `Result`. Variants
documented at `0.2`._

---

## Feature flags

| Feature | Default | Description |
|---------|---------|-------------|
| `std`        | yes | Standard library. Required for the sharded store and eviction; with it off the crate is `no_std` and the scaffold exposes only [`VERSION`](#version). |
| `algorithms` | no  | The full suite beyond the default token bucket. |
| `async`      | no  | Optional additive async-friendly wrapper. Implies `std`. |

---

<sub>Copyright &copy; 2026 <strong>James Gober</strong>. All rights reserved.</sub>
