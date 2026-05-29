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
//! Pre-1.0, under active development. This `0.1.0` release is the **scaffold**:
//! it establishes the crate metadata, the quality-gate tooling, and the
//! documented shape of the API that lands across the `0.x` series. There is no
//! rate-limiting logic yet — the foundation API (the [`Decision`](#) result and
//! the `RateLimiter` trait) arrives in `0.2.0`, and the real sharded, lock-free,
//! bounded-memory core in `0.3.0`. The surface below is the target the
//! documentation describes; it is not callable in this release.
//!
//! ```text
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
//!         // denied — return 429 with Retry-After: {retry_after}
//!     }
//! }
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
/// assert!(version.starts_with("0.1"));
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
