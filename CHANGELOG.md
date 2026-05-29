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

[Unreleased]: https://github.com/jamesgober/rate-net/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/jamesgober/rate-net/releases/tag/v0.1.0
[`VERSION`]: https://docs.rs/rate-net/latest/rate_net/constant.VERSION.html
