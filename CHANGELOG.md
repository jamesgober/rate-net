<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br>
    <b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>rate-net</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [1.0.0] - 2026-05-29

**Stable.** The public API is frozen until `2.0`. The `0.9.5` release candidate
soaked clean — no bugs and no friction from real consumption — and is promoted
to `1.0.0` with no functional changes, only the status declaration and
documentation polish. Pinning `rate-net = "1"` is now the supported install.

### Changed

- Status declared **stable (`1.0.0`)** in the lib, README, and API reference.
- README install snippets and the API quickstart use `rate-net = "1"`.
- `docs/BENCHMARKS.md` re-anchored at the `v1.0.0` tag; the four tracked paths
  (single-key, many-key, contended single key, eviction sweep) remained within
  ~±2 ns of the `0.6.0` baseline through the beta and RC soak — sample-to-sample
  variance, not regression.
- `VERSION` doctest re-anchored from the `0.x` series to a generic SemVer
  `major.minor.patch` shape check.
- Historical pre-RC release notes (`docs/release/v0.1.0`..`v0.8.0`) removed; the
  immediate pre-release line (`v0.9.0`, `v0.9.5`) is kept for context. The full
  pre-`1.0` history remains in this changelog.

### Notes

- **No breaking changes.** The public surface — `RateLimiter` and its methods,
  `Builder`, `AsyncLimiter`, `Limiter`, `Decision`, `Quota`, `Eviction`,
  `Algorithm`, `RateLimiterError`, `Key`, `VERSION`, and the `DEFAULT_MAX_KEYS`
  constant — is unchanged from `0.7.0` and is now frozen until `2.0`.
- The honest head-to-head against `governor` is unchanged: rate-net's per-key
  overhead is competitive once the clock is held equal, and the path to beating
  `governor` end-to-end remains a faster monotonic source in `clock-lib` (raised
  as a separate sibling enhancement).

---

## [0.9.5] - 2026-05-29

Release candidate. The beta soak surfaced no bugs and no API friction. Final
benchmark numbers re-captured and unchanged from the `0.6.0` baseline (within
~±2 ns noise across the four tracked paths). Critical fixes and documentation
polish only before `1.0`.

### Changed

- Status declared **release candidate** in the lib, README, and API reference.
- No functional changes; no breaking changes; the public surface is unchanged
  from `0.7.0`.

---

## [0.9.0] - 2026-05-29

Beta. The surface stays frozen — only bug fixes and documentation polish before
`1.0`. This release widens concurrency coverage and locks the thread-safety
guarantees in at the type level.

### Added

- `tests/stress.rs` — every algorithm under real contention. Eight threads hammer
  one hot key against the token bucket, the leaky bucket, the fixed window, the
  sliding-window log, and the sliding-window counter; with the clock frozen,
  each admits **exactly** the quota — proving the lock-free CAS paths and the
  per-key `Mutex` paths are both correct under load (no over-admit, no lost
  decrements, no deadlock).
- Compile-time `Send + Sync + 'static` assertions for every public type:
  `RateLimiter`, `AsyncLimiter`, `Decision`, `Quota`, `Eviction`, `Algorithm`,
  `RateLimiterError`, and `Key`. The "shared across threads" promise is now
  enforced by the compiler, not just documented.

---

## [0.8.0] - 2026-05-29

Alpha. The first-consumer shake-out: the public surface is validated the way a
real gatekeeper (`bouncer-io`) integrates it, confirming the allow/deny boundary
holds. The integration needed no API additions, so the surface stays frozen.

### Added

- `tests/consumer_pattern.rs` — a representative gatekeeper coded against the
  `Limiter` trait (not the concrete type), keying by caller identity (IP / user),
  turning a `Decision` into an HTTP-shaped verdict, and shared across threads via
  `Arc`. Covers per-identity isolation, honest retry-after, exact per-client
  limits under concurrency, every configured algorithm, and an async
  (await-until-ready) gatekeeper. The consumer touches **only** the public API —
  never any internal state.
- `examples/gatekeeper.rs` — a runnable minimal HTTP-style gatekeeper that limits
  per client IP and returns `429` + `Retry-After`, using only the public surface.

### Notes

- The integration read naturally with no friction: no new public items were
  needed, so the frozen API is confirmed consumable. The "consumers call the
  API, never touch internal state" boundary is enforced by visibility and
  demonstrated by the gatekeeper.

---

## [0.7.0] - 2026-05-29

Hardening and **API freeze**. The limiter is exercised against the threat model
it is built for — hostile traffic — and the public surface is now fixed.

### Added

- `tests/hardening.rs` — an adversarial-traffic and edge-matrix suite (15 tests)
  asserting no panic, no wrap, no over-admit, and bounded memory under:
  - a flood of 100 000 unique keys (stays within the capacity bound, and a
    concurrently-live key is never corrupted);
  - a burst storm of 10× the quota at one instant (admits exactly the quota);
  - no clock advance (admits exactly the quota);
  - a 100-day clock jump (refills to the cap, never beyond);
  - a request of `u32::MAX` units (denied, no panic, limiter still usable);
  - a `u32::MAX` quota and a zero quota;
  - rapid reconfiguration (`with_shards` / `with_eviction` chained);
  - and, per algorithm, the edge matrix — zero units, exact quota, `n > limit`,
    refill after a window, and the exact window boundary.

### Changed

- **API frozen.** The public surface — `RateLimiter` and its methods, `Builder`,
  `AsyncLimiter`, `Limiter`, `Decision`, `Quota`, `Eviction`, `Algorithm`,
  `RateLimiterError`, `Key`, and `VERSION` — is fixed. Any pre-1.0 additions will
  be additive and backward-compatible; nothing will be removed or have its
  signature changed before `1.0`.

---

## [0.6.0] - 2026-05-29

Optimization. The per-check overhead is cut substantially, with the work profiled
and the results — including an honest head-to-head against `governor` — recorded
in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). No new features; no breaking
changes.

### Changed

- **Hashing** — shard selection and the shard map now use `ahash` instead of
  SipHash: fast, and still collision-attack resistant thanks to a random
  per-store seed.
- **No redundant clock read on the hot path** — for the default path (token
  bucket + capacity-only eviction) the limiter no longer reads the clock at all.
  `better-bucket` already reads it for refill, and least-recently-seen eviction
  now orders by a cheap per-shard logical counter rather than wall time. The
  clock is read only when a window algorithm or an idle TTL needs real time.
- `#[inline]` on the check path so it inlines across the crate boundary.
- Net effect on the tracked baselines (Windows x86_64): single-key check
  ~77 ns → ~54 ns, many-key ~99 ns → ~54 ns, eviction sweep ~287 ns → ~217 ns.

### Added

- `benches/comparison.rs` — a head-to-head against `governor`, gated on
  `cfg(comparison)` (via `RUSTFLAGS="--cfg comparison"`) so the heavy
  benchmark-only dependency never enters the default tree, `--all-features`, or
  CI.
- An `ahash` dependency, pulled in by `std`.
- CI now also docs the default feature set, catching broken intra-doc links to
  feature-gated items in the always-compiled crate docs.

### Notes

- `governor` is currently faster (~17 ns vs ~44 ns single-key), almost entirely
  because it reads a TSC-based `quanta` clock (~5 ns) while rate-net reads
  `clock-lib`'s `Instant::now()` (~20 ns on Windows). rate-net's own per-key
  overhead is competitive; beating `governor` end-to-end needs a faster monotonic
  clock in `clock-lib` (a sibling enhancement). This is documented honestly
  rather than worked around.

---

## [0.5.0] - 2026-05-29

Feature complete. Everything a consumer needs is in place — runnable examples,
an optional async-wait layer, and a baseline benchmark suite — and features are
frozen. The remaining work toward `1.0` is optimization, hardening, and the
stability soak.

### Added

- `AsyncLimiter` (behind the `async` feature) — an await-until-ready wrapper.
  `until_ready` / `until_ready_n` retry on each denial, sleeping for the reported
  `retry_after` via `tokio::time`, until the key is admitted (or give up
  immediately when the request can never succeed); `check` / `check_n` pass
  straight through. The core stays sync and runtime-free — only this optional,
  additive layer touches `tokio`.
- `examples/` — runnable end-to-end demos: `per_second`, `per_key` (per-IP and
  per-user), `mock_clock` (deterministic refill with no real sleep), and
  `retry_after` (mapping a denial to HTTP `429` + `Retry-After`); plus
  `algorithms` (requires `algorithms`) and `async_wait` (requires `async`).
- A Criterion benchmark suite ([`benches/rate_bench.rs`](benches/rate_bench.rs)):
  `single_key`, `many_keys` (shard scaling), `contended_single_key` (4 threads),
  and `eviction_sweep`. Baseline numbers recorded in
  [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

### Changed

- The optional `tokio` dependency is trimmed to its `time` feature (only the
  timer is used). A `cfg(not(loom))` dev-dependency on `tokio` drives the async
  tests and example without interfering with the `loom` model-check build.
- **Feature freeze.** No new features are planned before `1.0`; subsequent
  releases are optimization, hardening, and stabilization only.

---

## [0.4.0] - 2026-05-29

Extended. The full algorithm suite lands behind the one `Limiter` trait, each
with its own over-admit proof, plus the Tier-2 builder that selects algorithm,
quota, burst, shards, eviction, and clock in one fluent surface.

### Added

- Four algorithms beyond the default token bucket, behind the `algorithms`
  feature, each a `Limiter` implementation with unit, retry-after, and over-admit
  `proptest` coverage:
  - **Leaky bucket (GCRA)** — spaces admitted units at the emission interval,
    smoothing to a steady rate while tolerating the quota's `burst`. Lock-free:
    one atomic theoretical-arrival-time advanced by compare-and-swap.
  - **Fixed window** — the cheapest option; a lock-free packed `(window, count)`
    atomic that resets each window. Tolerates the classic boundary burst.
  - **Sliding-window log** — exact: keeps the timestamp of every admitted unit in
    the trailing window, never bursts at boundaries, memory bounded by `limit`.
  - **Sliding-window counter** — O(1) approximate: a time-weighted blend of the
    current and previous window's counts.
- `RateLimiter::builder()` and the `Builder` type — the Tier-2 path:
  `.algorithm()`, `.quota()` / `.per_second()` / `.per_minute()`, `.burst()`,
  `.shards()`, `.eviction()`, `.clock()`, `.build()`.
- `RateLimiter::with_algorithm` to select the algorithm on an existing limiter.
- `Quota::burst` and `Quota::with_burst` — a burst ceiling distinct from the
  sustained `limit` (honoured by the token and leaky buckets).
- The `Algorithm` selector now drives per-key state via enum dispatch (no
  boxing, no vtable); its non-token variants are gated by the `algorithms`
  feature.
- `tests/proptest_algorithms.rs` — the per-algorithm over-admit proofs through
  the full `RateLimiter`.
- CI now also runs clippy and the test suite on the default (token-bucket-only)
  feature set, alongside `--all-features`.

### Changed

- The internal check path carries elapsed `Duration` (not just milliseconds) to
  each key's state, giving the window and leaky algorithms nanosecond resolution
  so sub-millisecond periods stay accurate.

---

## [0.3.0] - 2026-05-29

Core. The real concurrent machine: a sharded, bounded-memory per-key store with
an allocation-free steady-state check path. The token bucket is wired to
`better-bucket`; memory is bounded by eviction so a flood of unique keys hits a
cap instead of growing without limit.

### Added

- Purpose-built **sharded per-key store**. An existing-key check takes only a
  shard *read* lock plus the bucket's atomic accounting, so unrelated keys — and
  concurrent checks of the same key — never serialise; only first-seeing a key
  takes the brief write lock. Shard count is configurable via
  `RateLimiter::with_shards` and defaults to a small multiple of the core count.
- **Bounded-memory eviction** — the `Eviction` policy type and `DEFAULT_MAX_KEYS`
  constant. A per-shard, lazy, incremental sweep (run while inserting a new key,
  under the write lock already held, never as a background thread or
  stop-the-world pass) drops idle-expired keys and, at capacity, evicts the
  least-recently-seen key. The default is safe: a capacity cap so a unique-key
  flood is bounded out of the box. Configurable via `RateLimiter::with_eviction`.
- `RateLimiter::shards` and `RateLimiter::eviction` introspection.
- Inline-or-heap `Key` storage: the common identities (IP addresses, `u64` ids,
  short string keys) are held inline, so an existing-key check performs no heap
  allocation.
- Concurrency and memory proofs: a `loom` model of the store's get-or-insert
  protocol (two racing first-touches share one bucket and never over-admit); a
  multi-threaded stress test (across many keys, each key is admitted exactly its
  quota — never more, never fewer); an allocation audit asserting the
  steady-state check allocates nothing; and unit tests for the unique-key-flood
  bound, idle-TTL reclamation, and a hot key surviving eviction pressure.

### Changed

- Replaced the `dashmap` dependency with the purpose-built store, which is what
  makes per-shard incremental eviction and the read-lock steady-state path
  possible. `std` no longer pulls in `dashmap`.
- Existing-key checks now take a shard read lock rather than a write lock, so
  unrelated keys sharing a shard no longer serialise.

---

## [0.2.0] - 2026-05-28

Foundation. The public API shape is locked and a correct single-key
token-bucket limiter ships behind it. Per-key state lives in a concurrent map;
the tunable sharded store with bounded-memory eviction and the zero-allocation
steady state arrive in `0.3.0`, and the remaining algorithms in `0.4.0`.

### Added

- `RateLimiter` — the keyed rate limiter. Tier-1 constructors
  `per_second(limit)` and `per_minute(limit)` (infallible) plus `with_quota`,
  the `check(key)` / `check_n(key, n)` request path, the `with_clock`
  clock-injection seam, and `quota` / `algorithm` / `tracked_keys`
  introspection. The default token-bucket accounting is delegated to
  `better-bucket`; time is read from an injectable `clock-lib` clock. `Send +
  Sync`; a `Debug` impl that never prints keys.
- `Limiter` trait — the shared surface every algorithm implements, so generic
  code can hold any limiter and call `check` without naming the concrete type.
- `Decision` (`#[non_exhaustive]`) — `Allow` / `Deny { retry_after }`, with
  `is_allow` / `is_deny` / `retry_after` helpers and a `From<better_bucket::Decision>`
  bridge.
- `Quota` — `per_second` / `per_minute` (infallible) and the validated
  `rate(limit, period)` constructor.
- `Algorithm` (`#[non_exhaustive]`) — the algorithm selector; defaults to
  `TokenBucket`, the only variant wired in this release.
- `RateLimiterError` (`#[non_exhaustive]`) — construction-time validation
  errors (`ZeroQuota`, `ZeroPeriod`), implemented on `error-forge`'s
  `ForgeError` for portfolio-wide error metadata.
- `Key` — the opaque per-key identity, with `From` conversions for `&str`,
  `String`, `&[u8]`, `Vec<u8>`, `u64`, and `IpAddr`.
- `tests/proptest_overadmit.rs` — the per-algorithm over-admit invariant
  (`proptest`): across any interleaving of checks and time advances a key is
  never admitted beyond its quota, and distinct keys are accounted
  independently.
- `MockClock`-driven unit tests covering quota exhaustion, refill across a
  window, partial refill, per-key independence, `check_n`, the `n = 0` and
  `n > quota` edges, and the zero-limit case — all deterministic, no `sleep`.

### Changed

- The `std` feature now also pulls in `better-bucket` (with its `clock`
  feature), `clock-lib`, and `error-forge`; these power the limiter and were
  made optional so the no_std build (which still exposes only `VERSION`) stays
  dependency-light.

---

## [0.1.0] - 2026-05-28

Initial scaffold and repository bootstrap. No rate-limiting logic yet — this
release establishes the structure, dependency wiring, tooling, and quality
gates the implementation will be built on, and compiles clean across the full
CI matrix (Linux/macOS/Windows, stable and MSRV).

### Added

- `Cargo.toml` with full crate metadata, Rust 2024 edition, MSRV 1.85,
  dual `Apache-2.0 OR MIT` license, `docs.rs` configuration, and a
  perf-tuned release profile (`lto = "fat"`, `codegen-units = 1`,
  `strip`).
- `src/lib.rs` — crate root with the strict lint gate (`deny(warnings)`,
  `missing_docs`, the `unwrap`/`expect`/`todo`/`print`/`dbg` clippy denies,
  and `unsafe_op_in_unsafe_fn`), crate-level documentation describing the
  target API, the [`VERSION`] constant, and a smoke test asserting it is
  well-formed SemVer. The crate is `no_std` when the `std` feature is off,
  where it exposes only [`VERSION`].
- Dependencies wired: `better-bucket` (token-bucket strategy) and
  `clock-lib` (monotonic + mockable time), both with `default-features =
  false`; `dashmap` (sharded per-key store) as an optional dependency
  pulled in by the `std` feature; and optional `tokio` behind the `async`
  feature.
- Feature flags: `std` (default; enables `dashmap` for the sharded store
  and eviction), `algorithms` (the full suite beyond the default token
  bucket), and `async` (optional additive async layer, implies `std`).
- Dev-dependencies for the test stack: `criterion` (benchmarks),
  `proptest` (per-algorithm over-admit proofs), and `loom` under
  `cfg(loom)` for concurrency model checking.
- `benches/rate_bench.rs` — Criterion harness. The scaffold benchmarks the
  shard-locate primitive (hash a key, mask to a shard index) as an honest
  floor for the per-request work; the full check/scaling/eviction suite
  lands with the sharded core.
- `tests/loom_check.rs` — `loom` concurrency model (gated on `cfg(loom)`)
  proving the harness runs and that the CAS contention kernel the per-key
  admit path will use never over-admits under any interleaving.
- `[lints.rust]` registering the `loom` and `docsrs` build-time cfgs so the
  `unexpected_cfgs` lint stays quiet under `-D warnings`.
- `README.md` — overview, the "why rate-net" positioning, a pre-release
  status note, Tier-1 quick start, configured-limiter and mockable-clock
  examples, the algorithm table, design notes (sharded lock-free state,
  bounded eviction, the no-over-admit invariant, the clean consumer
  boundary), and cross-platform support.
- `docs/API.md` — reference documenting the [`VERSION`] constant with
  multiple examples; the remaining surface is marked _planned_ per phase.
- `REPS.md` compliance baseline at the repository root.
- `.github/workflows/ci.yml` — Linux/macOS/Windows CI matrix on stable
  and MSRV (fmt, clippy `-D warnings`, test, doc `-D warnings`), plus
  loom and security (audit + deny) jobs.
- `deny.toml` — cargo-deny license / advisory / source policy.
- `.gitattributes` normalising line endings to LF and keeping
  development-only paths out of `git archive` tarballs.
- `.dev/` AI-editor briefing (`PROMPT.md`, `ROADMAP.md`) and `.dev/release/`
  note template — gitignored.

### Notes

- MSRV is **1.85** to match the Rust 2024 edition and the `better-bucket`
  / `clock-lib` dependencies.
- rate-net is sequenced behind `better-bucket`'s lock-free core; see the
  roadmap for the dependency ordering.
- Libraries do not commit `Cargo.lock` (per portfolio convention).

[Unreleased]: https://github.com/jamesgober/rate-net/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/jamesgober/rate-net/compare/v0.9.5...v1.0.0
[0.9.5]: https://github.com/jamesgober/rate-net/compare/v0.9.0...v0.9.5
[0.9.0]: https://github.com/jamesgober/rate-net/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/jamesgober/rate-net/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/jamesgober/rate-net/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/jamesgober/rate-net/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/jamesgober/rate-net/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/jamesgober/rate-net/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/jamesgober/rate-net/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/rate-net/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/rate-net/releases/tag/v0.1.0
[`VERSION`]: https://docs.rs/rate-net/latest/rate_net/constant.VERSION.html
