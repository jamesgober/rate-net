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

---

## [2.0.0] — 2026-10-19

**Eviction never resets a limit by default, and the HTTP surface is
complete.** A major release. Its headline change is a security default: a
key evicted while still limited keeps its debt instead of coming back with a
fresh allowance (in `1.x` it was reset) — exactly, and without affecting any
other key, while such keys fit about twice the cap, and beyond that under the
new default `Overflow::FailClosed`, which never resets a key but can charge or
refuse keys that owed nothing once the store is far past its cap. The opt-out
`Overflow::Forget` restores 1.x behaviour past that bound. Two long-deferred
breaking cleanups come with it: `error-forge` is now an optional `forge`
feature, and `DynLimiter` requires `Send + Sync`. It also stores per-key state
a third smaller, checks long keys without allocating, and adds per-key
quotas, key-length limits, bounded async waits, the current IETF structured
headers, `tower` middleware, and saturation guardrails (a recovery-load
gauge, a saturation counter, rate-limited `tracing` warnings, and a sizing
guide). This release was prepared as `1.2.0` and never published under that
number. Upgrading: [`docs/MIGRATION_1_TO_2.md`](./docs/MIGRATION_1_TO_2.md).
Finding IDs below are the 2026-10-13 audit's (`RN-001`…`RN-023`) and the 2.0
re-audits' (`RN2-001`…`RN2-017`, `RN3-001`…`RN3-007`, `RN4-001`…`RN4-005`);
the [1.1.0](#110--2026-10-13) entry used an earlier numbering,
cross-referenced at the end of this entry.

### Breaking changes

- **Evicted, still-limited keys keep their debt; past the exact table the
  default is `Overflow::FailClosed`.** In 1.x every evicted key came back with
  a fresh allowance. Now its debt is recorded. While a store's still-limited
  evicted keys fit about twice the cap, nothing else changes. Beyond that, the
  default keeps every debt in a shared sketch, so some keys that never owed
  anything may start partly or wholly drained, and a saturated shard refuses a
  newcomer with a retry time (`Metrics::keys_refused`). Measured: no newcomer
  denied after drain floods up to 3x the cap, about 3 % at 4x, 38 % at 6x,
  86 % at 9x; a well-behaved population of 10x the cap sees 4 % false denials
  at 5,000 visits a second and 72 % at 20,000. Size the cap so it never gets
  there (README "Sizing the cap"), alert on `recovery_load()`, or opt out with
  `Eviction::with_overflow(Overflow::Forget)`.
- **`error-forge` is optional (feature `forge`, off by default).** The
  `error_forge::ForgeError` impls on `RateLimiterError` and
  `InsufficientCapacity` now need `features = ["forge"]`. The default build
  drops 8 crates (16 → 8 normal dependencies; `parking_lot` and a proc-macro
  among them).
- **`DynLimiter: Send + Sync`.** The object-safe trait now requires
  `Send + Sync`, so `Arc<dyn DynLimiter>` is shareable as written; a limiter
  type that is not `Send + Sync` no longer implements it.
- **`Metrics` gained fields** (`keys_remembered`, `keys_restored`,
  `debts_forgotten`, `debts_overflowed`, `keys_refused`, `keys_rejected`); it is
  `#[non_exhaustive]`, so only exhaustive struct literals outside the crate
  could notice, and those were never possible.

### Security

- **Capacity eviction reset a still-limited key (RN-005).** In `1.1.0` the CLOCK
  hand evicted any key not checked during one revolution, recovered or not, and
  an evicted key that returned started with a full allowance — so flooding the
  store with throwaway keys reset any target's limit (login attempts for one
  username, say). The hand now prefers keys whose allowance has fully
  recovered, and when it must evict one that has not, the shard records the
  debt (units spent, and when they would all be back). A key that returns
  before its debt is repaid resumes **no more permissive than it left**:
  exactly where it was for the token and leaky buckets; with the spent units
  charged at the return for the window algorithms (held back up to one window
  longer). Debts go into a lazily allocated, cuckoo-hashed table of 16-byte
  records, two per key of capacity, where a key reads back only its own record.
  When a shard's still-limited evicted keys outgrow it (about twice the cap at
  once), `Eviction::with_overflow` decides: `Overflow::FailClosed` (the
  default) spills them into a shared overflow sketch (four 8-byte cells per key of
  capacity) that charges a key the smaller of its two cells — never less than
  its own debt, but possibly a key that never owed anything — and a full shard
  that can neither evict nor record refuses a newcomer with a retry time;
  `Overflow::Forget` drops the debts that do not fit, so those keys may come
  back with a fresh allowance as in `1.x` and no other key is affected. A key
  charged from the sketch never writes that charge back (RN2-001: the first
  2.0 candidate's merged records spread and ratcheted debt onto innocent
  keys). An idle-TTL eviction of a key that is still owed (a per-key quota with
  a longer recovery) records its debt the same way, judging the key by its
  current state (RN3-001).
  `remove`, `reset`, and `clear` forget a key's own record. Regression tests
  (`tests/eviction_bypass.rs`) fail against `1.1.0` for every algorithm; liveness
  tests (`tests/eviction_liveness.rs`) bound false denials under floods; `loom`
  models cover a drained key evicted while it is checked and concurrent
  record-and-take; property tests check that no recorded debt is ever lost and
  that no amount of eviction lets a key exceed its token-bucket bound; a fuzz
  target drives a heavily evicting store against an oracle.
- **Key bytes were unbounded (RN-006).** `Builder::max_key_len` refuses
  over-long keys (denied as impossible, never stored), and
  `Builder::digest_keys_longer_than` stores long keys as a 16-byte digest of two
  independently, randomly seeded hashes, so a store's memory is bounded in bytes,
  not just keys. `tower::HeaderKey` stores values over 64 bytes as a digest
  by default (RN2-006).
- **An idle-expired key with a slow per-key quota could be forgotten with its
  debt (RN3-001, found in review of this release).** The eviction trusted a
  "recovers at" hint computed when the key was stored, even after the key had
  spent again; a key whose `quota_for` override recovers more slowly than the
  idle TTL came back with a fresh allowance. The hint is now ignored whenever
  the key's reference bit says it was checked since; covered by a unit test, an
  integration test for every algorithm, a `loom` model of the sweep racing a
  re-check, and the fuzz target (now with slow overrides and idle TTLs).
- **The overflow sketch under-charged window keys with a limit above
  16,777,215 (RN3-002, found in review).** It keeps 24 bits of a spent count;
  a full field now reads back as everything (clamped to the key's limit).
- **`X-Forwarded-For` could be forged by a client reaching the service
  directly (RN2-005).** `ForwardedFor::only_from(network, prefix_len)` uses the
  header only when the connection's peer is inside the proxies' network and
  keys by the peer otherwise; the docs no longer claim the header is never
  client-written. Bracketed and zoned IPv6 entries now parse.

### Fixed

- **Long keys allocated on every check (RN-011).** `check_bytes`,
  `check_n_bytes`, and `peek_bytes` look a key up by a borrowed view; the check
  of an existing key allocates nothing whatever its length (36-byte UUID:
  ~52 ns, against ~81 ns through `check`). The allocation audit now covers long
  keys, every algorithm, and the key-length policies.
- **Tautological hardening assertions** (`tests/hardening.rs`) replaced with
  real ones, and every integration test renamed to the REPS
  `test_<subject>_<condition>_<expected>` convention.
- **Clock-sensitive examples.** Doc examples that assert a reset time now run on
  a frozen `ManualClock`, so a stalled host cannot fail them.
- The benchmark suite's contended figure was aggregate throughput (wall time
  over all four threads' checks), which read about four times faster than any
  caller sees; it is now per-check latency, measured per thread after a
  barrier.
- **The `quota_for` resolver ran under the shard write lock (RN2-003)**, so a
  resolver that called back into the limiter deadlocked and a slow one stalled
  its shard. It now runs before any lock is taken; the rules (may call other
  keys, must not check the key being resolved, should be fast, panics
  propagate) are documented.
- **Per-key overrides bypassed `try_build`'s checks (RN2-010).** An override is
  validated like the default quota; an invalid one makes its key fail closed.
- **`tower`: `with_cost(0)` disabled limiting (RN2-008)** — a cost of `0` now
  counts as `1`; **`RateLimit-Policy` described the default quota for keys
  with an override (RN2-009)** — it now describes the key's own quota.
- **The CI fuzz job could not start (RN2-002)**: it now creates the corpus
  directories first, as do the README's commands.
- **`deny(warnings)` broke the nightly miri and fuzz builds (RN2-012)**: it is
  now off under `miri` and `fuzzing` as well as `docsrs`.
- **A key whose quota admits nothing took the shard write lock on every check
  (RN3-003)**, so a blocked tenant hammering its key serialised its shard
  (same-shard p99.9 30.9 µs with 8 threads). It is now denied before the lock
  (p99.9 1.5 µs); `check/blocked_key` benchmarks it.
- **`tower::ForwardedFor::only_from` compared IPv4-compatible peers and
  IPv4-mapped networks as IPv6 (RN3-006).** Addresses are now canonicalised as
  `Key::ip` does (`::ffff:10.0.0.0/104` is `10.0.0.0/8`), and a `/0` network
  matches every peer.

### Added

- `RateLimiter::check_bytes`, `check_n_bytes`, `peek_bytes` — allocation-free
  checks of raw keys of any length.
- `Builder::quota_for` — per-key quota overrides (tiered limits) from a
  resolver called, outside any lock, when a key is about to be stored; an
  invalid override, or one that admits nothing, blocks the key (fail closed,
  never stored). `RateLimiter::quota_of(key)` reports the quota a key is
  under.
- `Builder::max_key_len`, `Builder::digest_keys_longer_than` — the key-length
  policies above.
- `AsyncLimiter::until_ready_within` / `until_ready_n_within` and the same on
  `DirectLimiter` — awaits bounded by a budget: they sleep only while the next
  admission fits the remaining budget and otherwise return the denial at once.
  `with_jitter` / `jitter` on both add uniform random jitter to every sleep.
  The awaiting methods document that they panic outside a Tokio runtime.
- `Snapshot::ratelimit` and `Quota::ratelimit_policy` — the current IETF draft's
  structured `RateLimit` (`"name";r=..;t=..`) and `RateLimit-Policy`
  (`"name";q=..;w=..`) field values, formatted without allocating;
  `headers::RATELIMIT`, `headers::RATELIMIT_POLICY`, `headers::RateLimitValue`,
  `headers::RateLimitPolicyValue`.
- `tower` feature: `rate_net::tower::RateLimitLayer` for inbound HTTP in front
  of any `tower::Service` (axum, hyper, tonic), with `KeyExtractor`
  implementations `PeerIp`, `ForwardedFor` (trusting a fixed number of
  proxies, optionally only from their network), `HeaderKey`, and closures over
  `RequestInfo`; denied requests get `429` with `Retry-After` and the rate-limit
  headers without reaching the inner service. `with_anonymous(Anonymous)`
  chooses what requests with no identity are charged to — one shared key (the
  default), a key of your choice, or nothing, rejected with a status (RN2-007).
  Adds `http`, `tower-layer`, `tower-service`, and `pin-project-lite` (not
  `tower` itself). `examples/tower_layer.rs`.
- `Eviction::with_overflow` / `Eviction::overflow` and the `#[non_exhaustive]`
  `Overflow` enum (`FailClosed`, the default; `Forget`): what happens to debts
  of still-limited evicted keys past the exact table.
- Saturation guardrails: `RateLimiter::recovery_load()` and `RecoveryLoad`
  (feature `metrics`) — the debts held against the exact capacity, overall and
  in the fullest shard, to alert on before anything spills or is refused;
  `Metrics::keys_refused` as the saturation counter; the `tracing` feature,
  which logs a `warn!` (target `rate_net::eviction`) on the first spill, drop,
  or saturation refusal and at most once a minute while they continue, with
  nothing on the check path (`check/single_key` within noise with it on).
- The `forge` feature (see Breaking changes).
- A capacity sizing guide and operations checklist (README, API reference,
  `Eviction` docs), with how NGINX `limit_req` and `governor` behave for
  comparison.
- `Metrics`: `keys_remembered`, `keys_restored`, `debts_forgotten`,
  `debts_overflowed`, `keys_refused`, `keys_rejected`.
- `Key::len`, `Key::is_empty`, `From<[u8; N]>`, `From<&[u8; N]>` for `Key`.
- Tests: `tests/eviction_bypass.rs`, `tests/eviction_liveness.rs`,
  `tests/key_options.rs`, the eviction and recovery-table property tests,
  allocation audits for long keys and every algorithm, `loom` models of the
  evict-and-return race and of concurrent record-and-take, unwind-safety
  assertions.
- Fuzzing: `fuzz/` with `limiter_ops` (arbitrary checks, clock advances, resets,
  removals, and sweeps on a heavily evicting store, against a token-bucket
  oracle) and `http_keys` (key extraction from hostile headers, and structured
  field values from arbitrary policy names). Smoke-run in CI.
- Benchmarks: `check/long_key`, `check/long_key_bytes`, and `benches/flood.rs`
  (a legitimate key's p50/p99/p99.9 while three threads flood the default
  limiter at its cap, with recovering and with draining flood keys).

### Changed

- **Typed per-key state.** The store is generic over the algorithm's state, so
  a key holds exactly that state with no enum tag, and the token bucket keeps
  `better-bucket`'s clock-free `BucketCore` (the limiter reads the clock once,
  just before the key's accounting). A token-bucket key is 128 bytes in its
  shard; heap per key at the default cap fell from ~202 B to ~138 B (~140 MiB
  at the 1,048,576-key default cap). A quota that admits nothing is never
  stored.
- `better-bucket` is used without features (only `BucketCore`), so it no longer
  pulls `clock-lib` and `error-forge` in a second time.
- An insert at capacity costs ~58 ns more at the default cap (293 → 351 ns)
  because eviction now reads each candidate's state to prefer recovered keys —
  a known issue (below); a small, cache-resident shard's insert got slightly
  faster. Many-key checks and `peek` are faster (fewer cache lines per key).
  See `docs/BENCHMARKS.md`.
- The recovery tables cost 32 bytes per key of capacity once a shard first
  evicts a still-limited key, 64 under `Overflow::FailClosed` (32 or 64 MiB at
  the default cap: about 170 or 202 MiB in all under eviction pressure).
- `Cargo.lock` is committed and `rust-toolchain.toml` pins Rust 1.99.0 for
  development (excluded from the package); every CI job selects its toolchain
  explicitly and prints `rustc -V`; a weekly and per-PR *latest dependencies*
  job runs `cargo update` (true latest on 1.99.0, MSRV-compatible latest on
  1.85) before building, testing, linting, and auditing; the
  package no longer ships `REPS.md`, `.github/`, or editor configuration
  (RN2-016).
- `#![deny(warnings)]` stays in the crate root as REPS requires, but not on
  docs.rs (`cfg(docsrs)`), whose nightly rustdoc can add lints after a release
  and fail the published docs; downstream builds were never affected (Cargo caps
  dependency lints), and CI enforces `-D warnings` on stable and the MSRV.
- Clippy pedantic is clean on every target (a CI job enforces it); each allowed
  lint is narrowly scoped with a reason.
- CI: `actions/checkout@v7`; an each-feature matrix via `cargo-hack` (clippy,
  tests, and docs for every feature alone, on stable and the MSRV); pedantic,
  release-mode stress, coverage, and fuzz-smoke jobs; `semver-checks` on all
  features; miri over the recovery-table and eviction tests.
- `tests/memory.rs` budget tightened from 512 to 256 B per key.

### Not changed, with reasons

- **No `From` impls for other integer types (RN-011).** A second integer
  conversion would make an unsuffixed literal (`limiter.check(42)`) ambiguous —
  verified to stop compiling — so `u32` / `i64` / `u128` keys use `Key::id` or
  an explicit `u64`.
- **The default capacity stays at 1,048,576 keys (RN-006).** The bound (about
  140 MiB idle, 202 MiB under eviction pressure) is documented precisely, with
  a sizing guide, instead.
- **No blanket `Limiter` impls for `&L` and `Box<L>`.** They would conflict
  with impls downstream code may have written; not worth a break on their own.
- **Idle keys are reclaimed only by inserts or `retain_recent` (RN-021).**
  Automatic background reclamation needs a thread, which the runtime-free
  design avoids; `retain_recent` is the maintenance hook (and now also releases
  an expired recovery table).
- **`CHANGELOG` dates for 0.9.0–1.0.0 (RN-023).** The entries are a historical
  record and are not rewritten; the "soak" wording there describes intent, not a
  measured period.

### Known issues

- **Inserting a new key into a full store costs ~58 ns more than in `1.1.0`**
  (≈293 → 351 ns at the default 1,048,576-key cap; measured on a loaded Windows
  host, best of runs, so approximate), because eviction checks that each
  candidate has fully recovered before evicting it — the price of never
  resetting a still-limited key. Checks of tracked keys are unaffected, and
  below the cap nothing changes. To be optimised in a future release; a
  blocker for the next minor release, 2.1.0.
- **Past about twice the cap of still-limited evictions, one side gives.**
  Under the default `Overflow::FailClosed`, keys that owed nothing may be
  charged or refused. Under `Overflow::Forget`, the debts that do not fit are
  dropped and those keys may come back with a fresh allowance — a sustained
  unique-key flood can reset a targeted key repeatedly. Measured (ManualClock, 128
  shards): `FailClosed` denies no newcomer after drain floods up to 3x the
  cap, about 3 % at 4x, 38 % at 6x, 86 % at 9x; a well-behaved population of
  10x the cap sees 4 % false denials at 5,000 visits a second and 72 % at
  20,000. `Forget` denies none of them. Under `FailClosed`, `reset` and
  `remove` cannot clear a shared charge for one key (it expires within one
  recovery period), and a slow tier's charge can drain a fast-tier newcomer
  for the slow tier's recovery time.
- **`clock-lib` 1.1.0 does not build under `--cfg loom` (RN2-015)**; the `loom`
  job pins 1.0.0 until it is fixed upstream.

### Audit cross-reference (2026-10-13, `RN-0xx`)

| ID | Status |
|----|--------|
| RN-001 token-bucket over-admission | Fixed in 1.1.0 (`better-bucket` 1.1); 2.0 drives `BucketCore` directly |
| RN-002 fixed-window lost update | Fixed in 1.1.0 |
| RN-003 O(n) eviction scan | Fixed in 1.1.0; 2.0 adds the flood tail-latency benchmark |
| RN-004 O(n) idle sweep | Fixed in 1.1.0 |
| RN-005 eviction resets limited keys | Idle TTL fixed in 1.1.0; capacity eviction fixed in **2.0.0** |
| RN-006 memory bound, unbounded key bytes | **2.0.0**: ~138 B/key, documented; `max_key_len`, `digest_keys_longer_than` |
| RN-007 sliding-counter retry_after | Fixed in 1.1.0 |
| RN-008 GCRA fail-open, degenerate configs | Fixed in 1.1.0 (`try_build`) |
| RN-009 sliding-log memory per unit | Fixed in 1.1.0 (run-length records) |
| RN-010 token-bucket capacity cap | Fixed in 1.1.0 (`better-bucket` 1.1) |
| RN-011 long keys allocate | **2.0.0**: `check_bytes` & co., array `From` impls; integer impls declined (above) |
| RN-012 hot-path write traffic | Fixed in 1.1.0; 2.0 typed slots and a late clock read |
| RN-013 stale `now` in sliding algorithms | Fixed in 1.1.0 |
| RN-014 validation gaps, `check_n` doc | Fixed in 1.1.0 (`try_build` covers `with_burst(0)`) |
| RN-015 `--no-default-features` tests | Fixed in 1.1.0; 2.0 tests every feature alone in CI |
| RN-016 Retry-After rounding | Fixed in 1.1.0 |
| RN-017 `Limiter` not dyn-compatible | Fixed in 1.1.0 (`DynLimiter`); **2.0.0** adds the `Send + Sync` supertrait |
| RN-018 cross-type key collisions | Fixed in 1.1.0 (tagged keys, redacted `Debug`) |
| RN-019 pedantic lints, `deny(warnings)` | **2.0.0**: pedantic clean and enforced; docs.rs exempted |
| RN-020 broken link, README compile | Fixed in 1.1.0 |
| RN-021 no reclamation / shrink | Fixed in 1.1.0 (key management); background sweep declined (above) |
| RN-022 `error-forge` weight | 1.0.2 in 1.1.0; **2.0.0**: optional `forge` feature, off by default |
| RN-023 CI / process | **2.0.0**: checkout v7, feature matrix, coverage, fuzz; toolchain pinned, lock committed |

The `1.1.0` entry's identifiers come from an earlier pass and map as:
RN-COR-01 = RN-001; RN-SEC-01 = RN-003 and RN-004; RN-COR-03 = RN-002;
RN-COR-04 = RN-008; RN-COR-05 = RN-008 and RN-010; RN-COR-07 = RN-005 (idle
TTL); RN-COR-08 = RN-001 and RN-010 (the 49.7-day wrap); RN-SEC-03 = RN-009;
RN-SEC-04 and RN-SEC-05 = RN-018. RN-COR-02 (a fixed-window `u32` overflow) and
RN-COR-06 (`until_ready` returning as if admitted) had no counterpart in this
audit.

---

## [1.1.0] — 2026-10-13

**Correctness, hardening, and the HTTP surface.** A minor release: every change
is additive or a bug fix, and the `1.0` API is untouched apart from one
deprecation. It fixes a real-clock over-admission in the default token bucket,
an O(n) eviction scan that a unique-key flood could turn into a CPU and latency
denial of service, and a set of window-algorithm bugs; it adds validated
construction, `peek` snapshots and HTTP header helpers, key management, a
direct (un-keyed) limiter and a `no_std` core, tagged and IP-prefix keys, a
fast coarse clock, optional metrics, and an object-safe limiter trait. Several
documentation claims that were not true of `1.0.0` are corrected.

### Security

- **Token-bucket over-admission under contention (RN-COR-01).** With two or
  more threads on one key and a real clock, a thread holding a slightly stale
  timestamp read the negative gap as a ~49.7-day refill: the default limiter
  admitted 20x–600x its quota. Fixed in `better-bucket` 1.1, which rate-net now
  requires (`better-bucket = "1.1"`). A real-clock, multi-thread regression test
  (`tests/real_clock.rs`) and a `loom` model of the real bucket guard it here.
- **O(n) eviction under the shard write lock (RN-SEC-01).** At capacity every
  new key scanned its whole shard (~329 µs per insert at the default cap; one
  existing-key check waited 608 ms during a flood), and an idle TTL ran a full
  `retain` on every insert. Eviction is now CLOCK (second chance) with a scan
  bounded at 32 slots, and the idle TTL is drained by an incremental sweep of
  8 slots per insert: an insert at the default cap costs ~0.36 µs.
- **Key collisions across key types (RN-SEC-04).** New tagged constructors —
  `Key::id`, `Key::ip`, `Key::ipv4_prefix`, `Key::ipv6_prefix`,
  `Key::ip_prefix` — carry their kind in the key's identity (and unwrap
  IPv4-mapped / IPv4-compatible addresses, so `ipv6_prefix` never lumps every
  IPv4 client of a dual-stack socket into one `/64`), so a tagged key can
  never equal an attacker-chosen string with the same bytes. The `From`
  conversions keep their `1.0` (raw, byte-identity) behaviour.
- **Key bytes in logs (RN-SEC-05).** `Key`'s `Debug` now shows only the kind and
  length, never the bytes.
- **Sliding-window log memory amplification (RN-SEC-03).** Units admitted at the
  same instant share one record, so a single `check_n` of any size costs 16
  bytes instead of 8 bytes per unit (a `check_n(50_000_000)` used to allocate
  ~400 MB).
- `cargo audit` / `cargo deny` are clean again: the lockfile resolves
  `crossbeam-epoch` ≥ 0.9.20 (RUSTSEC-2026-0204, dev-only via `criterion`).

### Fixed

- **Fixed-window overflow (RN-COR-02).** `base + n` overflowed `u32` once
  `limit > u32::MAX / 2`: a debug-build panic and a release-build over-admit.
  The sum is now widened.
- **Fixed-window rollback by a late thread (RN-COR-03).** A thread carrying a
  timestamp from a window another thread had already closed wrote its older
  window back, wiping the newer window's count. A `latest` window number now
  makes a late thread count against the newer window. The sliding-window
  counter and log treat a late thread the same way (never judging against an
  already-rolled window, never unsorting the log).
- **Degenerate configurations failed open (RN-COR-04).** A zero period admitted
  everything under every algorithm except the token bucket. Every algorithm now
  fails closed: a zero limit or period denies everything, as does a zero burst
  for the token and leaky buckets (the window algorithms ignore the burst).
- **Leaky-bucket truncation (RN-COR-05).** The emission interval was a truncated
  whole number of nanoseconds: 600 M/s admitted 1 G/s (1.67x), and above
  1 G/s the interval reached zero and everything was admitted. Time is now
  accounted in exact emission intervals (no drift at any rate), and a rate
  above one unit per nanosecond is clamped to it.
- **Sliding-window counter `retry_after` was early.** It waited only to the
  next window boundary (or capped the wait at one period), ignoring that the
  current window's count then becomes the fading one: a client honouring
  `Retry-After` was denied again. The counter now admits with exact integer
  arithmetic and reports the exact earliest instant the request fits;
  `tests/retry_honesty.rs` property-tests, for every algorithm, that retrying
  after `retry_after` succeeds.
- **`until_ready` returned as if admitted when it gave up (RN-COR-06).** See
  `try_until_ready` below; `until_ready` / `until_ready_n` are deprecated.
- **An idle TTL shorter than the quota reset a key early (RN-COR-07).** The
  effective TTL is now raised to the quota's recovery time (the time a key needs
  to regain its full allowance), and rounded up past the millisecond stamps'
  truncation, so idle eviction can never hand out a fresh quota early;
  `try_build` rejects the shorter TTL outright.
- **Token-bucket capacity ceiling and the 49.7-day wrap (RN-COR-05, RN-COR-08).**
  With `better-bucket` 1.1 the ~4.29M-token capacity cap and the
  under-refill after a 49.7-day idle gap are gone.
- `cargo test --no-default-features` failed: every example and bench now
  declares its required features.
- The README Quick Start did not compile (`E0004`, missing wildcard arm on the
  `#[non_exhaustive]` `Decision`). The README and the API reference are now
  compiled and run as doctests.
- The examples floored `Retry-After` (`as_secs().max(1)`), sending clients back
  early; they now round up.

### Added

- `Builder::try_build` / `Builder::try_build_direct` — validate the
  configuration and return a `RateLimiterError`. New variants: `ZeroBurst`,
  `RateTooHigh`, `LimitTooLarge`, `ShardCountOutOfRange`, `IdleTtlTooShort`.
  `MAX_SLIDING_LOG_LIMIT`, `MAX_SHARDS` (the shard count is clamped to it, so
  `shards(usize::MAX)` no longer overflows).
  `Builder::new`, `Default`, and `Debug` for `Builder`.
- `RateLimiter::peek` → `Snapshot` (`limit`, `remaining`, `reset_after`,
  `reset_after_secs`, `is_exhausted`, `headers`) — read an allowance without
  spending it or creating state.
- Key management: `RateLimiter::reset`, `remove`, `clear`, `retain_recent`
  (forget keys whose allowance has fully recovered — lossless), and
  `shrink_to_fit`.
- HTTP helpers: `Decision::retry_after_secs` (rounded up, at least 1),
  `Decision::is_impossible`, `Snapshot::headers` (`RateLimit-Limit`,
  `RateLimit-Remaining`, `RateLimit-Reset`), and the `headers` module of header
  names.
- `DirectLimiter` — an un-keyed limiter for one stream (any algorithm, any
  clock; `check`, `check_n`, `peek`, `reset`), built directly or with
  `Builder::build_direct`.
- `Gcra` — the `no_std`, clock-free, lock-free single-key core (caller-supplied
  time), available in every build with 64-bit atomics. With `std` off the crate
  now exposes `Gcra`, `Quota`, `Decision`, `Snapshot`, `RateLimiterError`,
  `InsufficientCapacity`, and `headers` (previously only `VERSION`).
- `CoarseClock` (and `MIN_COARSE_RESOLUTION`) — a monotonic clock read with one
  atomic load, ticked by a background thread (`spawn`) or by the caller
  (`tick`). Roughly halves the cost of a token-bucket check. A lagging clock
  can admit up to `rate × lag` beyond the quota over a span (the lagged time is
  credited when it catches up); a spawned clock bounds the lag to about one
  interval even when the ticker is starved, because every 61st read on a
  thread re-reads the real clock and ticks if a whole interval behind.
- `AsyncLimiter::try_until_ready` / `try_until_ready_n`, and the same on
  `DirectLimiter` (feature `async`) — return `InsufficientCapacity` instead of
  returning as if admitted. Cancel-safe. `Debug` for `AsyncLimiter`.
- `metrics` feature: `RateLimiter::metrics` / `DirectLimiter::metrics` →
  `Metrics` (`allowed`, `denied`, `keys_inserted`, `keys_evicted`,
  `keys_expired`, `keys_removed`). Compiled out entirely without the feature.
- `DynLimiter` — an object-safe face of `Limiter` (`Box<dyn DynLimiter>`,
  `Arc<dyn DynLimiter + Send + Sync>`), implemented for every `Limiter`; the
  trait objects implement `Limiter` again. `Limiter` is also implemented for
  `Arc<L>` (not `&L` / `Box<L>`, which 1.0 users may implement themselves;
  `tests/downstream_impls.rs` guards that).
- `Key`: `From<&String>`, `From<Ipv4Addr>`, `From<Ipv6Addr>`, `From<SocketAddr>`
  (the port is dropped), `From<&Key>`.
- Tests: real-clock multi-thread contention for every algorithm
  (`tests/real_clock.rs`), `loom` models of the production limiter, key
  management across every algorithm, a per-key memory budget
  (`tests/memory.rs`), metrics, the `no_std` core, and store-index property
  tests.
- Benchmarks: insert at the default cap, the direct limiter, the coarse clock,
  and `peek`; the `governor` comparison gains direct and coarse-clock groups and
  moves to `governor` 0.10.
- `examples/direct.rs`; `retry_after` and `gatekeeper` show `RateLimit-*`
  headers and IPv6 `/64` keys.

### Changed

- The store is a `hashbrown::HashTable` index over dense per-shard slots: one
  hash pass per check (was two), no per-check `fetch_add` on a shared sequence
  counter, eviction marks written only when they change, and shards padded to
  their own cache lines. A capped shard holds at most 16,384 keys — the
  shard count is raised as needed (`shards()` may exceed the request) — so the
  amortized index growth or rehash that remains is a short pass, not the
  ~200 ms a single 1M-key shard took. Per-key heap at the default cap fell from ~516 B to
  ~202 B (~514 MiB → ~202 MiB at the default cap).
- New dependency `hashbrown` 0.17 (default features off, no transitive
  dependencies). `error-forge` requirement raised to 1.0.2 (drops `thiserror`).
- `RateLimiterError` and `InsufficientCapacity` implement `core::error::Error`
  in every build.
- Documentation corrected where `1.0.0` overstated: the check path is not
  lock-free (a shard read lock plus atomic accounting; the sliding algorithms
  take a per-key mutex); the `loom` suite now checks the real code rather than a
  copy; eviction no longer blocks the check path; the benchmark numbers are
  re-measured; the CI platform list is stated as tested.

### Deprecated

- `AsyncLimiter::until_ready` / `until_ready_n` — they return as if the caller
  were admitted when the request can never fit. Use `try_until_ready` /
  `try_until_ready_n`; the old methods remain until `2.0`.

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

[Unreleased]: https://github.com/jamesgober/rate-net/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/jamesgober/rate-net/compare/v1.1.0...v2.0.0
[1.1.0]: https://github.com/jamesgober/rate-net/compare/v1.0.0...v1.1.0
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
