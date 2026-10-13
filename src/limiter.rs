//! The rate limiter and the trait every algorithm shares.

use std::fmt;
use std::num::NonZeroUsize;
use std::time::Duration;

use clock_lib::{Clock, Monotonic, SystemClock};

use crate::algo::{AlgoState, fresh_snapshot};
use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::eviction::Eviction;
use crate::key::Key;
use crate::quota::Quota;
use crate::snapshot::Snapshot;
use crate::store::Store;

/// Default shard count when the caller does not choose one: four shards per
/// available core, rounded to a power of two and clamped to a sane range. More
/// shards means less contention between unrelated keys, at the cost of a little
/// more memory.
pub(crate) fn default_shard_count() -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1);
    (parallelism.saturating_mul(4))
        .next_power_of_two()
        .clamp(1, 4096)
}

/// The shared rate-limiting surface, independent of the algorithm behind it.
///
/// Every limiter — whatever algorithm it uses — answers the same question:
/// *is this key allowed right now?* `Limiter` is that contract, so generic code
/// can hold any limiter and call [`check`](Self::check) without naming the
/// concrete type or its clock. [`RateLimiter`] is the implementation this crate
/// ships.
///
/// Implementors only need to provide [`check_n`](Self::check_n);
/// [`check`](Self::check) defaults to one unit.
///
/// # Examples
///
/// ```
/// use rate_net::{Limiter, RateLimiter};
///
/// // Generic over any limiter implementation.
/// fn admit_one<L: Limiter>(limiter: &L, key: &str) -> bool {
///     limiter.check(key).is_allow()
/// }
///
/// let limiter = RateLimiter::per_second(2);
/// assert!(admit_one(&limiter, "user:1"));
/// ```
pub trait Limiter {
    /// Checks `n` units against `key`, returning the [`Decision`].
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision;

    /// Checks a single unit against `key`. Equivalent to `check_n(key, 1)`.
    fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_n(key, 1)
    }
}

/// The object-safe face of [`Limiter`], for holding "any limiter" behind a
/// pointer.
///
/// `Limiter`'s methods are generic over `impl Into<Key>`, which keeps call
/// sites short but means `dyn Limiter` cannot exist. `DynLimiter` has one
/// non-generic method, so `&dyn DynLimiter`, `Box<dyn DynLimiter>`, and
/// `Arc<dyn DynLimiter + Send + Sync>` all work — the shape middleware wants
/// when the limiter is chosen at runtime. Every [`Limiter`] implements it
/// automatically, and the trait objects implement `Limiter` again, so the
/// ergonomic `check` / `check_n` stay available on them.
///
/// # Examples
///
/// ```
/// use rate_net::{DynLimiter, Limiter, RateLimiter};
/// use std::sync::Arc;
///
/// // Pick an implementation at runtime and hold it as a trait object.
/// let limiter: Arc<dyn DynLimiter + Send + Sync> = Arc::new(RateLimiter::per_second(1));
///
/// assert!(limiter.check_key("user:42".into(), 1).is_allow());
/// // The trait object is itself a `Limiter`, so the generic helpers work too.
/// assert!(limiter.check("user:42").is_deny());
/// ```
pub trait DynLimiter {
    /// Checks `n` units against `key`. The non-generic core of
    /// [`Limiter::check_n`].
    fn check_key(&self, key: Key, n: u32) -> Decision;
}

impl<L: Limiter> DynLimiter for L {
    #[inline]
    fn check_key(&self, key: Key, n: u32) -> Decision {
        self.check_n(key, n)
    }
}

impl Limiter for dyn DynLimiter + '_ {
    #[inline]
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_key(key.into(), n)
    }
}

impl Limiter for dyn DynLimiter + Send + Sync + '_ {
    #[inline]
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_key(key.into(), n)
    }
}

impl<L: Limiter + ?Sized> Limiter for &L {
    #[inline]
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        (**self).check_n(key, n)
    }
}

impl<L: Limiter + ?Sized> Limiter for std::sync::Arc<L> {
    #[inline]
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        (**self).check_n(key, n)
    }
}

impl<L: Limiter + ?Sized> Limiter for Box<L> {
    #[inline]
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        (**self).check_n(key, n)
    }
}

/// A keyed rate limiter.
///
/// Tracks an independent allowance for every key it sees and answers
/// [`check`](Self::check) in the time it takes to hash the key and run its
/// bucket. Per-key state lives in a [sharded](Self::with_shards) concurrent
/// store: unrelated keys land in different shards and never contend, an
/// existing-key check takes only a shared read lock plus the bucket's atomic
/// accounting, and memory is bounded by [eviction](Self::with_eviction) so a
/// flood of unique keys hits a cap instead of growing without limit. The
/// limiter is `Send + Sync` and is meant to be shared — behind an
/// [`Arc`](std::sync::Arc), or as a `static` — across all the threads serving
/// requests.
///
/// The default algorithm is the token bucket, whose accounting is delegated to
/// [`better-bucket`](https://crates.io/crates/better-bucket): each key bursts up
/// to its [`Quota`] immediately, then sustains the quota rate as the allowance
/// refills. Time comes from an injectable [`Clock`] — [`SystemClock`] in
/// production, or a `ManualClock` in tests via [`with_clock`](Self::with_clock).
///
/// # Examples
///
/// ```
/// use rate_net::{RateLimiter, Decision};
///
/// // 100 requests per second, per key.
/// let limiter = RateLimiter::per_second(100);
///
/// match limiter.check("user:42") {
///     Decision::Allow => { /* serve the request */ }
///     Decision::Deny { retry_after } => {
///         // 429, Retry-After: retry_after
///         let _ = retry_after;
///     }
///     _ => {}
/// }
/// ```
pub struct RateLimiter<C: Clock + Clone = SystemClock> {
    algorithm: Algorithm,
    quota: Quota,
    clock: C,
    epoch: Monotonic,
    shards: usize,
    eviction: Eviction,
    store: Store<C>,
    /// Whether the check path must read the clock: the token bucket reads its
    /// own clock and capacity-only eviction needs no real time, so the common
    /// case skips the read entirely. Window algorithms (which need `now`) and an
    /// idle TTL (which measures real elapsed time) turn it on.
    reads_clock: bool,
}

impl RateLimiter<SystemClock> {
    /// Creates a limiter allowing `limit` requests per second, per key, driven
    /// by the OS monotonic clock.
    ///
    /// The headline Tier-1 constructor. A `limit` of `0` yields a limiter that
    /// denies every request.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// assert_eq!(limiter.quota().limit(), 10);
    /// ```
    #[must_use]
    pub fn per_second(limit: u32) -> Self {
        Self::with_quota(Quota::per_second(limit))
    }

    /// Creates a limiter allowing `limit` requests per minute, per key.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::per_minute(600);
    /// assert_eq!(limiter.quota().period(), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn per_minute(limit: u32) -> Self {
        Self::with_quota(Quota::per_minute(limit))
    }

    /// Creates a limiter from an explicit [`Quota`], driven by the OS monotonic
    /// clock, with default sharding and a bounded-memory [`Eviction`] policy.
    ///
    /// Use this with [`Quota::rate`] when the window is neither a second nor a
    /// minute.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Quota};
    /// use std::time::Duration;
    ///
    /// let quota = Quota::rate(5, Duration::from_millis(100))?;
    /// let limiter = RateLimiter::with_quota(quota);
    /// assert_eq!(limiter.quota().limit(), 5);
    /// # Ok::<(), rate_net::RateLimiterError>(())
    /// ```
    #[must_use]
    pub fn with_quota(quota: Quota) -> Self {
        Self::build(
            Algorithm::default(),
            quota,
            SystemClock::new(),
            default_shard_count(),
            Eviction::default(),
        )
    }

    /// Starts a [`Builder`](crate::Builder) — the Tier-2 path that selects the
    /// algorithm, quota, burst, shard count, eviction policy, and clock in one
    /// fluent surface.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Eviction};
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::builder()
    ///     .quota(1000, Duration::from_secs(60)) // 1000 / minute
    ///     .burst(50)
    ///     .shards(64)
    ///     .eviction(Eviction::idle(Duration::from_secs(300)))
    ///     .build();
    /// assert_eq!(limiter.quota().limit(), 1000);
    /// assert_eq!(limiter.quota().burst(), 50);
    /// ```
    pub fn builder() -> crate::builder::Builder<SystemClock> {
        crate::builder::Builder::new()
    }
}

impl<C: Clock + Clone> RateLimiter<C> {
    /// Assembles a limiter from its parts, anchoring the eviction clock. Shared
    /// with [`Builder`](crate::Builder).
    pub(crate) fn build(
        algorithm: Algorithm,
        quota: Quota,
        clock: C,
        shards: usize,
        eviction: Eviction,
    ) -> Self {
        let epoch = clock.now();
        // An idle TTL never evicts a key before it could have recovered its
        // full allowance; evicting sooner would hand out a fresh quota early.
        let store = Store::new(shards, eviction, algorithm.recovery_time(&quota));
        // The token bucket reads its own clock and capacity-only eviction orders
        // by a logical counter, so that combination needs no clock read here.
        let reads_clock = algorithm != Algorithm::TokenBucket || eviction.idle_ttl().is_some();
        Self {
            algorithm,
            quota,
            clock,
            epoch,
            shards,
            eviction,
            store,
            reads_clock,
        }
    }

    /// Replaces the limiter's time source, discarding any per-key state.
    ///
    /// This is the clock-injection seam, intended for use immediately after
    /// construction. Injecting a `ManualClock` makes refill behaviour
    /// deterministic so window and rollover tests run with no `sleep`. The shard
    /// count and eviction policy are preserved.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use clock_lib::ManualClock;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let clock = Arc::new(ManualClock::new());
    /// let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));
    ///
    /// // Drain the key's allowance.
    /// for _ in 0..5 {
    ///     assert!(limiter.check("k").is_allow());
    /// }
    /// assert!(limiter.check("k").is_deny());
    ///
    /// // Advance one second — no real sleep — and the allowance is back.
    /// clock.advance(Duration::from_secs(1));
    /// assert!(limiter.check("k").is_allow());
    /// ```
    #[must_use]
    pub fn with_clock<C2: Clock + Clone>(self, clock: C2) -> RateLimiter<C2> {
        RateLimiter::build(
            self.algorithm,
            self.quota,
            clock,
            self.shards,
            self.eviction,
        )
    }

    /// Selects the algorithm, discarding any per-key state.
    ///
    /// Intended immediately after construction. The leaky bucket and the window
    /// algorithms require the `algorithms` feature; without it the only
    /// selectable variant is [`Algorithm::TokenBucket`].
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(feature = "algorithms")] {
    /// use rate_net::{RateLimiter, Algorithm};
    ///
    /// let limiter = RateLimiter::per_second(100).with_algorithm(Algorithm::SlidingWindowCounter);
    /// assert_eq!(limiter.algorithm(), Algorithm::SlidingWindowCounter);
    /// # }
    /// ```
    #[must_use]
    pub fn with_algorithm(self, algorithm: Algorithm) -> Self {
        Self::build(
            algorithm,
            self.quota,
            self.clock,
            self.shards,
            self.eviction,
        )
    }

    /// Sets the shard count, discarding any per-key state.
    ///
    /// Intended immediately after construction. More shards reduce contention
    /// between unrelated keys; the value is rounded up to a power of two. A good
    /// starting point is a small multiple of the core count.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1000).with_shards(64);
    /// assert_eq!(limiter.shards(), 64);
    /// ```
    #[must_use]
    pub fn with_shards(self, shards: usize) -> Self {
        Self::build(
            self.algorithm,
            self.quota,
            self.clock,
            shards,
            self.eviction,
        )
    }

    /// Sets the eviction policy, discarding any per-key state.
    ///
    /// Intended immediately after construction. The default policy bounds memory
    /// with a generous key-capacity cap; override it to tune the cap or add an
    /// idle TTL.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Eviction};
    /// use std::time::Duration;
    ///
    /// let limiter = RateLimiter::per_second(1000)
    ///     .with_eviction(Eviction::capacity(100_000).with_idle(Duration::from_secs(300)));
    /// assert_eq!(limiter.eviction().max_keys(), Some(100_000));
    /// ```
    #[must_use]
    pub fn with_eviction(self, eviction: Eviction) -> Self {
        Self::build(
            self.algorithm,
            self.quota,
            self.clock,
            self.shards,
            eviction,
        )
    }

    /// Checks a single unit against `key`.
    ///
    /// Returns [`Decision::Allow`] if the key is within its limit (the unit is
    /// counted), or [`Decision::Deny`] with the wait until it would be admitted.
    /// The key can be anything that converts into a [`Key`] — a string, an IP
    /// address, a user id.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Decision};
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// assert_eq!(limiter.check("user:42"), Decision::Allow);
    /// assert!(limiter.check("user:42").is_deny()); // limit reached
    /// ```
    #[inline]
    pub fn check(&self, key: impl Into<Key>) -> Decision {
        self.check_inner(key.into(), 1)
    }

    /// Checks `n` units against `key` in one operation.
    ///
    /// Useful when a single request costs more than one unit (a batch, a
    /// weighted endpoint). Either all `n` units are admitted or none are.
    /// Requesting `0` always succeeds; requesting more than the quota can never
    /// succeed, and the denial's `retry_after` is [`Duration::MAX`].
    ///
    /// [`Duration::MAX`]: std::time::Duration::MAX
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Decision};
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// assert_eq!(limiter.check_n("tenant:acme", 4), Decision::Allow);
    /// assert_eq!(limiter.check_n("tenant:acme", 6), Decision::Allow);
    /// assert!(limiter.check_n("tenant:acme", 1).is_deny()); // 10 spent
    /// ```
    #[inline]
    pub fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_inner(key.into(), n)
    }

    /// The quota every key is limited to.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// assert_eq!(RateLimiter::per_second(50).quota().limit(), 50);
    /// ```
    #[must_use]
    pub fn quota(&self) -> Quota {
        self.quota
    }

    /// The algorithm this limiter applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, Algorithm};
    ///
    /// assert_eq!(RateLimiter::per_second(1).algorithm(), Algorithm::TokenBucket);
    /// ```
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    /// The eviction policy bounding the per-key store.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{RateLimiter, DEFAULT_MAX_KEYS};
    ///
    /// assert_eq!(RateLimiter::per_second(1).eviction().max_keys(), Some(DEFAULT_MAX_KEYS));
    /// ```
    #[must_use]
    pub const fn eviction(&self) -> Eviction {
        self.eviction
    }

    /// The number of shards the per-key store is split across (a power of two).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// assert_eq!(RateLimiter::per_second(1).with_shards(32).shards(), 32);
    /// ```
    #[must_use]
    pub fn shards(&self) -> usize {
        self.store.shard_count()
    }

    /// The number of keys with live state right now.
    ///
    /// A momentary snapshot, advisory under concurrent access — and bounded by
    /// the [eviction](Self::eviction) policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// assert_eq!(limiter.tracked_keys(), 0);
    /// let _ = limiter.check("a");
    /// assert_eq!(limiter.tracked_keys(), 1);
    /// ```
    #[must_use]
    pub fn tracked_keys(&self) -> usize {
        self.store.len()
    }

    /// Reads `key`'s remaining allowance without consuming any.
    ///
    /// The [`Snapshot`] carries the key's capacity, what is left of it right
    /// now, and the wait until it is whole again — what `RateLimit-*` response
    /// headers report on every response, admitted or not. Peeking takes only
    /// the shard read lock, never counts a unit, never creates state for an
    /// unseen key (which reports its full allowance), and does not refresh the
    /// key's eviction recency. Like any read of shared state it is advisory
    /// under concurrency; the [`Decision`] from a check is the authority.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(10);
    /// assert_eq!(limiter.peek("user:42").remaining(), 10); // unseen: full
    ///
    /// assert!(limiter.check_n("user:42", 4).is_allow());
    /// let snapshot = limiter.peek("user:42");
    /// assert_eq!(snapshot.limit(), 10);
    /// assert_eq!(snapshot.remaining(), 6);
    /// assert_eq!(limiter.tracked_keys(), 1); // peeking created nothing
    /// ```
    #[must_use]
    pub fn peek(&self, key: impl Into<Key>) -> Snapshot {
        let key = key.into();
        self.store
            .peek(&key, self.now())
            .unwrap_or_else(|| fresh_snapshot(self.algorithm, &self.quota))
    }

    /// Restores `key`'s full allowance, as if it had been idle long enough to
    /// refill completely. The key stays tracked.
    ///
    /// For administrative unblocks ("this customer was throttled by mistake")
    /// and tests. Returns `true` if the key had state, `false` if it was not
    /// tracked (an untracked key already has its full allowance).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_minute(2);
    /// assert!(limiter.check_n("user:42", 2).is_allow());
    /// assert!(limiter.check("user:42").is_deny());
    ///
    /// assert!(limiter.reset("user:42"));
    /// assert!(limiter.check("user:42").is_allow());
    /// assert!(!limiter.reset("never-seen"));
    /// ```
    pub fn reset(&self, key: impl Into<Key>) -> bool {
        self.store.reset(&key.into())
    }

    /// Forgets `key` entirely, releasing its state. Its next check starts from
    /// a fresh allowance.
    ///
    /// Returns `true` if the key had state.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// let _ = limiter.check("user:42");
    /// assert_eq!(limiter.tracked_keys(), 1);
    ///
    /// assert!(limiter.remove("user:42"));
    /// assert_eq!(limiter.tracked_keys(), 0);
    /// assert!(!limiter.remove("user:42"));
    /// ```
    pub fn remove(&self, key: impl Into<Key>) -> bool {
        self.store.remove(&key.into())
    }

    /// Forgets every key and releases the store's memory. Every key's next
    /// check starts from a fresh allowance.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// for user in 0..100u64 {
    ///     let _ = limiter.check(user);
    /// }
    /// limiter.clear();
    /// assert_eq!(limiter.tracked_keys(), 0);
    /// ```
    pub fn clear(&self) {
        self.store.clear();
    }

    /// Forgets every key whose allowance has fully recovered, returning how
    /// many were removed.
    ///
    /// A key with its whole allowance back behaves exactly like a key never
    /// seen, so this cleanup changes no decision — it only returns memory.
    /// Eviction already bounds the store, so calling this is optional; it suits
    /// a periodic maintenance task that wants the store to shrink back after a
    /// traffic spike. Each shard is walked in short chunks with the lock
    /// released in between, so concurrent checks keep flowing; the cost is
    /// proportional to the number of tracked keys.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use clock_lib::ManualClock;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let clock = Arc::new(ManualClock::new());
    /// let limiter = RateLimiter::per_second(10).with_clock(Arc::clone(&clock));
    /// let _ = limiter.check("busy");
    /// let _ = limiter.check("quiet");
    ///
    /// clock.advance(Duration::from_secs(1)); // both refill completely
    /// let _ = limiter.check("busy");         // ...and one spends again
    ///
    /// assert_eq!(limiter.retain_recent(), 1); // "quiet" is forgotten
    /// assert_eq!(limiter.tracked_keys(), 1);
    /// ```
    pub fn retain_recent(&self) -> usize {
        self.store.retain_recent(self.now())
    }

    /// Releases memory the store holds beyond its current keys — for example
    /// after [`retain_recent`](Self::retain_recent) or a burst of evictions.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    ///
    /// let limiter = RateLimiter::per_second(1);
    /// let _ = limiter.check("k");
    /// limiter.shrink_to_fit();
    /// assert_eq!(limiter.tracked_keys(), 1);
    /// ```
    pub fn shrink_to_fit(&self) {
        self.store.shrink_to_fit();
    }

    /// The shared check path: hand the key to the store as of the elapsed time,
    /// seeding fresh per-key state if this is the first time the key is seen.
    #[inline]
    fn check_inner(&self, key: Key, n: u32) -> Decision {
        let now = if self.reads_clock {
            self.now()
        } else {
            Duration::ZERO
        };
        self.store.check(key, n, now, || self.new_state(now))
    }

    /// Builds fresh per-key state for the configured algorithm and quota,
    /// anchored at the elapsed time `now`.
    fn new_state(&self, now: Duration) -> AlgoState<C> {
        AlgoState::new(self.algorithm, &self.quota, self.clock.clone(), now)
    }

    /// Monotonic elapsed time since this limiter's epoch, for the window
    /// algorithms and eviction timestamps. Saturating, so a multi-million-year
    /// uptime cannot wrap it.
    fn now(&self) -> Duration {
        self.clock.now().saturating_duration_since(self.epoch)
    }
}

impl<C: Clock + Clone> Limiter for RateLimiter<C> {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        self.check_inner(key.into(), n)
    }
}

impl<C: Clock + Clone> fmt::Debug for RateLimiter<C> {
    /// Formats the limiter without exposing any key. Keys can be caller
    /// identities or other sensitive values, so only the configuration and the
    /// live key count are shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimiter")
            .field("algorithm", &self.algorithm())
            .field("quota", &self.quota)
            .field("shards", &self.shards())
            .field("eviction", &self.eviction)
            .field("tracked_keys", &self.store.len())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;
    use std::time::Duration;

    use clock_lib::ManualClock;

    use super::{DynLimiter, Limiter, RateLimiter};
    use crate::algorithm::Algorithm;
    use crate::decision::Decision;
    use crate::eviction::Eviction;
    use crate::key::Key;
    use crate::quota::Quota;

    fn manual() -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(5).with_clock(Arc::clone(&clock));
        (clock, limiter)
    }

    #[test]
    fn test_fresh_key_is_admitted() {
        let limiter = RateLimiter::per_second(1);
        assert_eq!(limiter.check("user:1"), Decision::Allow);
    }

    #[test]
    fn test_quota_is_exhausted_then_refills_on_advance() {
        let (clock, limiter) = manual();

        for _ in 0..5 {
            assert_eq!(limiter.check("k"), Decision::Allow);
        }
        let decision = limiter.check("k");
        assert!(decision.is_deny());
        assert!(decision.retry_after().is_some());

        clock.advance(Duration::from_secs(1));
        assert_eq!(limiter.check("k"), Decision::Allow);
    }

    #[test]
    fn test_keys_are_independent() {
        let (_clock, limiter) = manual();

        for _ in 0..5 {
            assert!(limiter.check("a").is_allow());
        }
        assert!(limiter.check("a").is_deny());

        assert!(limiter.check("b").is_allow());
    }

    #[test]
    fn test_check_n_takes_multiple_units_atomically() {
        let (_clock, limiter) = manual();
        assert_eq!(limiter.check_n("batch", 3), Decision::Allow);
        assert_eq!(limiter.check_n("batch", 2), Decision::Allow);
        assert!(limiter.check_n("batch", 1).is_deny());
    }

    #[test]
    fn test_check_n_zero_always_admits() {
        let (_clock, limiter) = manual();
        for _ in 0..5 {
            assert!(limiter.check("k").is_allow());
        }
        assert_eq!(limiter.check_n("k", 0), Decision::Allow);
    }

    #[test]
    fn test_request_larger_than_quota_can_never_succeed() {
        let (clock, limiter) = manual();
        let decision = limiter.check_n("k", 6); // quota is 5
        assert_eq!(
            decision,
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
        clock.advance(Duration::from_secs(10));
        assert_eq!(limiter.check_n("k", 6).retry_after(), Some(Duration::MAX));
    }

    #[test]
    fn test_zero_limit_denies_everything() {
        let limiter = RateLimiter::with_quota(Quota::per_second(0));
        assert!(limiter.check("k").is_deny());
    }

    #[test]
    fn test_partial_refill_admits_proportionally() {
        let clock = Arc::new(ManualClock::new());
        let limiter = RateLimiter::per_second(10).with_clock(Arc::clone(&clock));
        for _ in 0..10 {
            assert!(limiter.check("k").is_allow());
        }
        assert!(limiter.check("k").is_deny());

        clock.advance(Duration::from_millis(300));
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_allow());
        assert!(limiter.check("k").is_deny());
    }

    #[test]
    fn test_tracked_keys_counts_distinct_keys() {
        let (_clock, limiter) = manual();
        assert_eq!(limiter.tracked_keys(), 0);
        let _ = limiter.check("a");
        let _ = limiter.check("b");
        let _ = limiter.check("a");
        assert_eq!(limiter.tracked_keys(), 2);
    }

    #[test]
    fn test_introspection_reports_token_bucket() {
        let limiter = RateLimiter::per_second(1);
        assert_eq!(limiter.algorithm(), Algorithm::TokenBucket);
    }

    #[test]
    fn test_with_shards_rounds_to_power_of_two() {
        let limiter = RateLimiter::per_second(1).with_shards(5);
        assert_eq!(limiter.shards(), 8);
    }

    #[test]
    fn test_with_eviction_is_reported() {
        let limiter = RateLimiter::per_second(1).with_eviction(Eviction::capacity(10));
        assert_eq!(limiter.eviction().max_keys(), Some(10));
    }

    #[test]
    fn test_unique_key_flood_is_bounded_by_capacity() {
        let limiter = RateLimiter::per_second(1)
            .with_shards(8)
            .with_eviction(Eviction::capacity(100));
        for k in 0..50_000u64 {
            let _ = limiter.check(k);
        }
        // Per-shard rounding of a 100-key cap across 8 shards.
        let bound = 100usize.div_ceil(8).max(1) * 8;
        assert!(
            limiter.tracked_keys() <= bound,
            "flood grew to {} keys, bound {bound}",
            limiter.tracked_keys()
        );
    }

    #[test]
    fn test_limiter_trait_generic() {
        fn count_admitted<L: Limiter>(limiter: &L, key: &str, attempts: u32) -> u32 {
            (0..attempts)
                .filter(|_| limiter.check(key).is_allow())
                .count() as u32
        }
        let limiter = RateLimiter::per_second(3);
        assert_eq!(count_admitted(&limiter, "k", 10), 3);
    }

    #[test]
    fn test_dyn_limiter_holds_any_limiter_as_a_trait_object() {
        let clock = Arc::new(ManualClock::new());
        let limiters: Vec<Box<dyn DynLimiter>> = vec![
            Box::new(RateLimiter::per_second(1)),
            Box::new(RateLimiter::per_second(1).with_clock(Arc::clone(&clock))),
        ];
        for limiter in &limiters {
            assert!(limiter.check_key(Key::from("k"), 1).is_allow());
            // The boxed trait object is itself a `Limiter`.
            assert!(limiter.check("k").is_deny());
            assert!(limiter.check_n("other", 1).is_allow());
        }
        let shared: Arc<dyn DynLimiter + Send + Sync> = Arc::new(RateLimiter::per_second(2));
        assert!(shared.check("k").is_allow());
        let by_ref: &dyn DynLimiter = &*limiters[0];
        assert!(by_ref.check("fresh").is_allow());
    }

    #[test]
    fn test_limiter_is_implemented_for_references_and_smart_pointers() {
        fn admit<L: Limiter>(limiter: L) -> bool {
            limiter.check("k").is_allow()
        }
        let limiter = Arc::new(RateLimiter::per_second(2));
        assert!(admit(&*limiter));
        assert!(admit(Arc::clone(&limiter)));
        assert!(admit(Box::new(RateLimiter::per_second(1))));
        assert!(!admit(&limiter));
    }

    #[test]
    fn test_debug_does_not_leak_keys() {
        let (_clock, limiter) = manual();
        let _ = limiter.check("secret-token-do-not-print");
        let rendered = format!("{limiter:?}");
        assert!(!rendered.contains("secret-token"));
        assert!(rendered.contains("RateLimiter"));
        assert!(rendered.contains("tracked_keys"));
    }
}
