//! # rate-net
//!
//! A powerful, lock-free rate limiter for Rust. It answers one question as fast
//! as the hardware allows — *"is this key allowed right now?"* — and answers it
//! with a [`Decision`](#) (allow / deny plus a `retry-after`), across multiple
//! algorithms, while tracking per-key state under high contention. Per-key
//! state lives in a sharded concurrent map, so unrelated keys never contend and
//! throughput scales with core count; each key's bucket is lock-free and memory
//! is bounded by eviction, so a flood of unique keys hits a cap instead of
//! growing without limit.
//!
//! rate-net does not reimplement token-bucket accounting. It consumes
//! [`better-bucket`](https://crates.io/crates/better-bucket) for that and reads
//! time from [`clock-lib`](https://crates.io/crates/clock-lib), then adds the
//! per-key, multi-algorithm, retry-after machinery around them. It is the
//! anchor crate of the `-net` domain group and is consumed by gatekeepers (such
//! as `bouncer-io`) through the clean decision API — they call `check` and never
//! reach into its internal state.
//!
//! ## Status
//!
//! Pre-1.0 **release candidate** (`0.9.5`): the API is frozen (since `0.7.0`),
//! locked at the *type* level (every public type is `Send + Sync + 'static`,
//! asserted at compile time), proven under contention against every algorithm
//! (eight threads on one hot key, exactly the quota — no over-admit, no lost
//! decrement, no deadlock), validated through a representative gatekeeper
//! consumer, and benchmark numbers held at the optimized baseline with no
//! regression across the beta soak. The five algorithms sit behind the one
//! [`Limiter`] trait (token bucket by default; the leaky bucket and window
//! algorithms under the `algorithms` feature), with the Tier-2 [`Builder`], an
//! optional `AsyncLimiter` await-until-ready layer (`async` feature), runnable
//! [examples](https://github.com/jamesgober/rate-net/tree/main/examples), and a
//! `criterion` suite. Per-key state lives in a purpose-built **sharded store**
//! (an existing-key [`check`](RateLimiter::check) takes only a shard read lock
//! plus the algorithm's atomic accounting, so unrelated keys never contend),
//! memory is **bounded by eviction**, and the steady-state check is
//! **allocation-free**. Critical fixes and documentation polish only from here
//! to `1.0`.
//!
//! ```
//! # #[cfg(feature = "std")] {
//! use rate_net::{RateLimiter, Decision};
//!
//! // 100 requests per second, per key.
//! let limiter = RateLimiter::per_second(100);
//!
//! match limiter.check("user:42") {
//!     Decision::Allow => {
//!         // allowed — serve the request
//!     }
//!     Decision::Deny { retry_after } => {
//!         // denied — return 429 with `Retry-After: retry_after`
//!         let _ = retry_after;
//!     }
//!     _ => {}
//! }
//! # }
//! ```
//!
//! ## Design goals
//!
//! - **Lock-free per key.** Each key's bucket delegates to `better-bucket`'s
//!   atomic compare-and-swap core; no lock sits on the check path.
//! - **Sharded state.** Per-key state lives in a sharded concurrent map, so
//!   unrelated keys land in different shards and never serialize on each other.
//!   Throughput scales with cores; shard count is tunable.
//! - **Zero-allocation steady state.** A `check` on an existing key allocates
//!   nothing; allocation happens only the first time a key is seen.
//! - **Bounded memory.** Idle keys are evicted (LRU/TTL) on an amortized,
//!   incremental schedule that never stops the world on the hot path. A hostile
//!   unique-key flood reaches the eviction cap and stays there.
//! - **Never over-admits.** For any key and window, admitted requests never
//!   exceed the configured quota under any concurrent interleaving — proven per
//!   algorithm with `loom` and `proptest`.
//! - **Lazy, runtime-free.** Refill and expiry are computed from a monotonic
//!   clock on access; there is no background timer thread and the core has no
//!   async-runtime dependency.
//!
//! ## Feature flags
//!
//! | Feature | Default | Description |
//! |---------|---------|-------------|
//! | `std`        | yes | Standard library. Required for the sharded per-key store and eviction. With it off the crate is `no_std`; the scaffold then exposes only [`VERSION`], and the pared-down single-key mode follows in a later release. |
//! | `algorithms` | no  | The full algorithm suite beyond the default token bucket: leaky bucket, fixed window, sliding-window log, sliding-window counter. |
//! | `async`      | no  | Optional async-friendly wrapper layer. Additive only — the core has no runtime dependency. |

// `no_std` for the library build when `std` is off, but always link `std` under
// `test` so the unit-test harness (and dev-dependencies) have what they need.
#![cfg_attr(all(not(feature = "std"), not(test)), no_std)]
#![deny(warnings)]
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

// The limiter surface requires the standard library (the concurrent per-key
// store, the clock-driven token bucket, and the domain error type). With `std`
// off the crate is no_std and exposes only `VERSION`.
#[cfg(feature = "std")]
mod algo;
#[cfg(feature = "std")]
mod algorithm;
#[cfg(feature = "async")]
mod async_limiter;
#[cfg(feature = "std")]
mod builder;
#[cfg(feature = "std")]
mod decision;
#[cfg(feature = "std")]
mod error;
#[cfg(feature = "std")]
mod eviction;
#[cfg(feature = "std")]
mod key;
#[cfg(feature = "std")]
mod limiter;
#[cfg(feature = "std")]
mod quota;
#[cfg(feature = "std")]
mod store;

#[cfg(feature = "std")]
pub use crate::algorithm::Algorithm;
#[cfg(feature = "async")]
pub use crate::async_limiter::AsyncLimiter;
#[cfg(feature = "std")]
pub use crate::builder::Builder;
#[cfg(feature = "std")]
pub use crate::decision::Decision;
#[cfg(feature = "std")]
pub use crate::error::RateLimiterError;
#[cfg(feature = "std")]
pub use crate::eviction::{DEFAULT_MAX_KEYS, Eviction};
#[cfg(feature = "std")]
pub use crate::key::Key;
#[cfg(feature = "std")]
pub use crate::limiter::{Limiter, RateLimiter};
#[cfg(feature = "std")]
pub use crate::quota::Quota;

/// The version of this crate, taken from `Cargo.toml` at compile time.
///
/// Exposed so a consumer can report the exact `rate-net` build it links
/// against — useful in diagnostics and version-skew checks across a dependency
/// tree.
///
/// # Examples
///
/// ```
/// // Reports the current 0.x series and carries a major.minor.patch core.
/// let version = rate_net::VERSION;
/// assert!(version.starts_with("0."));
/// assert_eq!(version.split('.').count(), 3);
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

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
