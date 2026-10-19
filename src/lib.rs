//! # rate-net
//!
//! A rate limiter for Rust. It answers one question — *"is this key allowed
//! right now?"* — with a [`Decision`] (allow, or deny plus a `retry-after`),
//! across five algorithms, while tracking per-key state under heavy
//! concurrency. Per-key state lives in a sharded store: an existing-key check
//! takes one shard read lock plus the algorithm's atomic accounting, so
//! unrelated keys rarely contend, and memory is bounded by O(1) eviction, so a
//! flood of unique keys hits a cap instead of growing without limit — and
//! evicting a still-limited key keeps its debt instead of resetting it.
//!
//! rate-net does not reimplement token-bucket accounting. It consumes
//! [`better-bucket`](https://crates.io/crates/better-bucket) (1.1 or newer) for
//! that and reads time from [`clock-lib`](https://crates.io/crates/clock-lib),
//! then adds the per-key, multi-algorithm, retry-after machinery around them.
//! It is the anchor crate of the `-net` domain group and is consumed by
//! gatekeepers (such as `bouncer-io`) through the clean decision API — they
//! call `check` and never reach into its internal state.
//!
//! ## Status
//!
//! **Stable (`2.0.0`).** The public API is frozen until `3.0`; each `2.x` only
//! adds to it (`docs/MIGRATION_1_TO_2.md` lists what changed from `1.x`). The five algorithms sit behind the one [`Limiter`] trait (and the
//! object-safe [`DynLimiter`]) — token bucket by default; the leaky bucket and
//! window algorithms under the `algorithms` feature — with the validating
//! [`Builder`] (per-key quotas, key-length limits, long-key digests),
//! [`peek`](RateLimiter::peek) snapshots and HTTP [`headers`](mod@headers)
//! helpers, key management, the un-keyed [`DirectLimiter`], the `no_std`
//! [`Gcra`] core, the fast [`CoarseClock`], and optional `AsyncLimiter`
//! (`async`), `Metrics` and `RecoveryLoad` (`metrics`), saturation warnings
//! (`tracing`), and `tower` middleware (`tower`) layers.
//! Checking an already-tracked key allocates nothing when the key is passed
//! by reference (`check_bytes`, `check_n_bytes`) or is at most 22 bytes, and
//! every public type is `Send + Sync + 'static`, asserted at compile time.
//!
//! ```
//! # #[cfg(feature = "std")] {
//! use rate_net::{Decision, RateLimiter};
//!
//! // 100 requests per second, per key.
//! let limiter = RateLimiter::per_second(100);
//!
//! match limiter.check("user:42") {
//!     Decision::Allow => {
//!         // allowed — serve the request
//!     }
//!     Decision::Deny { retry_after } => {
//!         // denied — return 429 with `Retry-After`, rounded up:
//!         let _secs = Decision::Deny { retry_after }.retry_after_secs();
//!     }
//!     _ => {}
//! }
//! # }
//! ```
//!
//! ## Design goals
//!
//! - **Never over-admits.** For any key, admitted requests never exceed the
//!   algorithm's documented bound, under any interleaving of threads and on a
//!   real clock where threads carry stale timestamps. Checked with `loom`
//!   against the production code, with `proptest` per algorithm, and with
//!   real-clock multi-thread contention tests.
//! - **Atomic accounting, sharded locking.** The token bucket, leaky bucket, and
//!   fixed window account each key with one compare-and-swap; the sliding-window
//!   algorithms take a short per-key mutex. Keys are found under a per-shard
//!   read lock, each shard on its own cache lines.
//! - **Bounded memory, bounded work.** Eviction runs only when a new key is
//!   inserted and does a fixed amount of work: a CLOCK sweep of at most 32
//!   slots for the capacity cap, and 8 slots of incremental idle sweep. The
//!   only shard-wide pass left is the amortized growth or rehash of a shard's
//!   index, and capped shards are kept at 16,384 keys or fewer so it stays
//!   short. A token-bucket key costs about 138 bytes at the default cap; key
//!   bytes beyond 22 can be bounded with a maximum length or a digest.
//! - **Eviction does not reset a limit.** A key evicted before its allowance
//!   recovered has its debt recorded, and resumes no more permissive than it
//!   left — unconditionally while the still-limited evicted keys fit about
//!   twice the cap. Past that, [`Overflow`] chooses: keep them all and fail
//!   closed for some other keys (`FailClosed`, the default — no flood ever
//!   resets a key) or drop the debts that do not fit (`Forget`, an opt-out
//!   for availability-first limiters). See [`Eviction`] for the trade-off and
//!   how to size the cap so neither happens.
//! - **Zero-allocation steady state.** A check on an existing key allocates
//!   nothing: always through [`check_bytes`](RateLimiter::check_bytes), which
//!   borrows the key, and through [`check`](RateLimiter::check) for keys of up
//!   to 22 bytes (longer ones are copied into an owned [`Key`] first).
//!   Allocation otherwise happens only the first time a key is seen.
//! - **Fail closed.** The infallible constructors turn a degenerate
//!   configuration into a limiter that denies everything; the validating
//!   [`Builder::try_build`] reports it instead.
//! - **Lazy, runtime-free.** Refill and expiry are computed from a monotonic
//!   clock on access; there is no background thread (unless you start a
//!   [`CoarseClock`] ticker) and the core has no async-runtime dependency.
//!
//! ## Feature flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `std`        | yes | The keyed limiter, [`DirectLimiter`], and [`CoarseClock`]. With it off the crate is `no_std` and offers [`Gcra`] (on targets with 64-bit atomics), [`Quota`], [`Decision`], [`Snapshot`], the error types, and the [`headers`](mod@headers) names. |
//! | `algorithms` | no  | The leaky bucket and the window algorithms (fixed window, sliding-window log, sliding-window counter). |
//! | `async`      | no  | `AsyncLimiter` and `DirectLimiter::try_until_ready` — await until admitted. Additive only. |
//! | `metrics`    | no  | `metrics()` counters on [`RateLimiter`] and [`DirectLimiter`], and `RateLimiter::recovery_load`, the gauge to alert on before eviction runs out of room; compiled out when off. |
//! | `tracing`    | no  | Rate-limited `tracing` warnings (target `rate_net::eviction`) when evicted keys' debts spill, are dropped, or newcomers are refused. Nothing on the check path. Adds `tracing` (no proc-macros). |
//! | `forge`      | no  | `error_forge::ForgeError` on the error types. Adds `error-forge`. |
//! | `tower`      | no  | `rate_net::tower` middleware: `RateLimitLayer` with key extractors and `429` responses. Adds `http`, `tower-layer`, `tower-service`, `pin-project-lite`. |

// `no_std` for the library build when `std` is off, but always link `std` under
// `test` so the unit-test harness (and dev-dependencies) have what they need.
#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
// REPS: warnings are errors — except in the builds that run on a floating
// nightly: docs.rs, Miri, and cargo-fuzz (`fuzzing`), where a lint added after
// a release would otherwise fail the published docs or the CI jobs. Downstream
// builds are unaffected either way (Cargo caps lints for dependencies), and CI
// enforces `-D warnings` on the pinned stable and the MSRV.
#![cfg_attr(not(any(docsrs, miri, fuzzing)), deny(warnings))]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]
#![deny(clippy::undocumented_unsafe_blocks)]
// The crate docs above describe the full `std` surface. Without `std` those
// items do not exist, so their links cannot resolve in a `no_std` doc build.
#![cfg_attr(not(feature = "std"), allow(rustdoc::broken_intra_doc_links))]

// The keyed limiter and everything clock-driven require the standard library
// (the concurrent per-key store, the clock-driven token bucket, the domain
// error integration). With `std` off the crate is no_std and exposes the
// clock-free core: `Gcra`, `Quota`, `Decision`, `Snapshot`, the error types,
// and the header names.
#[cfg(feature = "std")]
mod algo;
#[cfg(feature = "std")]
mod algorithm;
#[cfg(feature = "async")]
mod async_limiter;
#[cfg(feature = "std")]
mod builder;
#[cfg(feature = "std")]
mod coarse_clock;
mod decision;
#[cfg(feature = "std")]
mod direct;
mod error;
#[cfg(feature = "std")]
mod eviction;
#[cfg(target_has_atomic = "64")]
mod gcra;
pub mod headers;
#[cfg(feature = "std")]
mod key;
#[cfg(feature = "std")]
mod limiter;
#[cfg(feature = "metrics")]
mod metrics;
mod quota;
mod snapshot;
#[cfg(feature = "std")]
mod store;
mod sync;
#[cfg(feature = "tower")]
#[cfg_attr(docsrs, doc(cfg(feature = "tower")))]
pub mod tower;

#[cfg(feature = "std")]
pub use crate::algorithm::{Algorithm, MAX_SLIDING_LOG_LIMIT};
#[cfg(feature = "async")]
pub use crate::async_limiter::AsyncLimiter;
#[cfg(feature = "std")]
pub use crate::builder::Builder;
#[cfg(feature = "std")]
pub use crate::coarse_clock::{CoarseClock, MIN_COARSE_RESOLUTION};
pub use crate::decision::Decision;
#[cfg(feature = "std")]
pub use crate::direct::DirectLimiter;
pub use crate::error::{InsufficientCapacity, RateLimiterError};
#[cfg(feature = "std")]
pub use crate::eviction::{DEFAULT_MAX_KEYS, Eviction, Overflow};
#[cfg(target_has_atomic = "64")]
pub use crate::gcra::Gcra;
#[cfg(feature = "std")]
pub use crate::key::Key;
#[cfg(feature = "std")]
pub use crate::limiter::{DynLimiter, Limiter, RateLimiter};
#[cfg(feature = "metrics")]
pub use crate::metrics::{Metrics, RecoveryLoad};
pub use crate::quota::Quota;
pub use crate::snapshot::Snapshot;
#[cfg(feature = "std")]
pub use crate::store::MAX_SHARDS;

/// The version of this crate, taken from `Cargo.toml` at compile time.
///
/// Exposed so a consumer can report the exact `rate-net` build it links
/// against — useful in diagnostics and version-skew checks across a dependency
/// tree.
///
/// # Examples
///
/// ```
/// // Carries a `major.minor.patch` SemVer core.
/// let version = rate_net::VERSION;
/// assert_eq!(version.split('.').count(), 3);
/// assert!(version.split('.').all(|part| !part.is_empty()));
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// The README's and the API reference's examples, compiled and run as doctests
// so they cannot drift from the code. They use every optional feature, so they
// run in the all-features test pass.
#[cfg(all(
    doctest,
    feature = "algorithms",
    feature = "async",
    feature = "metrics",
    feature = "tower"
))]
#[doc = include_str!("../README.md")]
mod readme_doctests {}

#[cfg(all(
    doctest,
    feature = "algorithms",
    feature = "async",
    feature = "metrics",
    feature = "tower"
))]
#[doc = include_str!("../docs/API.md")]
mod api_doctests {}

#[cfg(all(doctest, feature = "std"))]
#[doc = include_str!("../docs/MIGRATION_1_TO_2.md")]
mod migration_doctests {}

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn test_version_is_well_formed_semver() {
        // A `major.minor.patch` core with no empty components.
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(parts.len(), 3, "expected major.minor.patch, got {VERSION}");
        assert!(parts.iter().all(|part| !part.is_empty()));
    }
}
