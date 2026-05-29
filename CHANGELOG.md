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

[Unreleased]: https://github.com/jamesgober/rate-net/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/jamesgober/rate-net/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/rate-net/releases/tag/v0.1.0
[`VERSION`]: https://docs.rs/rate-net/latest/rate_net/constant.VERSION.html
