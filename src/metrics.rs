//! Operational counters, compiled in only under the `metrics` feature.

use crate::sync::{AtomicU64, Ordering};

/// Cumulative counters for one limiter, read with `RateLimiter::metrics` or
/// `DirectLimiter::metrics`. Requires the `metrics` feature.
///
/// With the feature off the counters do not exist at all — no field, no
/// increment, no cost. With it on, each check performs one relaxed atomic
/// increment on a counter that lives on its shard's own cache line, next to the
/// lock word the check already touches, so unrelated shards never share a
/// counter.
///
/// Every value counts from the limiter's construction and only ever grows, so
/// export them as monotonic counters (a Prometheus `counter`) and compute rates
/// in the monitoring system. Each is read with a relaxed load, so a snapshot
/// taken under traffic is advisory: the fields are not mutually consistent to
/// the request.
///
/// The struct is `#[non_exhaustive]`: future releases may add counters.
///
/// # Examples
///
/// ```
/// # #[cfg(feature = "metrics")] {
/// use rate_net::RateLimiter;
///
/// let limiter = RateLimiter::per_second(2);
/// for _ in 0..5 {
///     let _ = limiter.check("user:42");
/// }
/// let metrics = limiter.metrics();
/// assert_eq!(metrics.allowed, 2);
/// assert_eq!(metrics.denied, 3);
/// assert_eq!(metrics.keys_inserted, 1);
/// # }
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Metrics {
    /// Checks that admitted their units. Its rate is the admitted request
    /// rate; compare it with `denied` for the rejection ratio.
    pub allowed: u64,
    /// Checks that were refused. A sustained rise means callers are hitting
    /// their limits — expected under abuse, a capacity signal otherwise.
    pub denied: u64,
    /// Keys seen for the first time (state created). Normally tracks the
    /// arrival rate of new callers; a spike far above it is the signature of a
    /// unique-key flood.
    pub keys_inserted: u64,
    /// Keys evicted because their shard was at its capacity cap. Zero is
    /// normal; a steady rate means the cap is smaller than the live key
    /// population (raise it) or the limiter is under a unique-key flood.
    pub keys_evicted: u64,
    /// Keys reclaimed by the idle-TTL sweep. Tracks how fast callers go quiet;
    /// harmless at any rate.
    pub keys_expired: u64,
    /// Keys removed explicitly: `remove`, `clear`, and `retain_recent`.
    pub keys_removed: u64,
    /// Evicted keys that had not yet recovered their full allowance, whose
    /// outstanding debt was recorded so that a returning key resumes no more
    /// permissive than it left (see [`Eviction`](crate::Eviction)). Counted
    /// within `keys_evicted` / `keys_expired`. A steady rate means the cap is
    /// evicting keys that are still being limited — raise it.
    pub keys_remembered: u64,
    /// New keys that resumed from a recorded debt instead of a fresh
    /// allowance: evicted keys coming back while still limited.
    pub keys_restored: u64,
    /// Debts recorded in the shared overflow because the exact recovery table
    /// was full (counted within `keys_remembered`). A new key whose two
    /// overflow cells are both live is charged the smaller of them, so a
    /// steady rate means some new keys start with less than a full allowance:
    /// the cap is too small for the number of keys being limited at once, or
    /// the limiter is under a flood. See [`Eviction`](crate::Eviction).
    pub debts_overflowed: u64,
    /// Debts of still-limited evicted keys that were dropped because the exact
    /// recovery table was full, under the opt-in
    /// [`Overflow::Forget`](crate::Overflow): such a key may come back with a
    /// fresh allowance. Not
    /// counted within `keys_remembered`. A steady rate means more keys are
    /// being limited at once than about twice the cap — raise the cap, or
    /// choose [`Overflow::FailClosed`](crate::Overflow) if no key may ever be
    /// reset.
    pub debts_forgotten: u64,
    /// **The saturation counter.** New keys refused (with a retry time)
    /// because their shard was saturated under
    /// [`Overflow::FailClosed`](crate::Overflow): full, and every examined
    /// key still limited with nowhere to record its debt. These may be
    /// legitimate users. Alert on any non-zero rate; the
    /// [`RecoveryLoad`] gauge rises well before it starts, and the `tracing`
    /// feature logs a warning on the first refusal and once a minute while
    /// they continue. Any refusal means the cap is too small for the traffic,
    /// or a flood is in progress.
    pub keys_refused: u64,
    /// Checks refused because the key exceeded the limiter's maximum key
    /// length (`Builder::max_key_len`). Counted within `denied`; such keys are
    /// never stored.
    pub keys_rejected: u64,
}

/// The live counters behind [`Metrics`], one set per shard (or per direct
/// limiter).
pub(crate) struct Counters {
    allowed: AtomicU64,
    denied: AtomicU64,
    inserted: AtomicU64,
    evicted: AtomicU64,
    expired: AtomicU64,
    removed: AtomicU64,
    remembered: AtomicU64,
    restored: AtomicU64,
    overflowed: AtomicU64,
    forgotten: AtomicU64,
    refused: AtomicU64,
    rejected: AtomicU64,
}

impl Counters {
    pub(crate) fn new() -> Self {
        Self {
            allowed: AtomicU64::new(0),
            denied: AtomicU64::new(0),
            inserted: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            removed: AtomicU64::new(0),
            remembered: AtomicU64::new(0),
            restored: AtomicU64::new(0),
            overflowed: AtomicU64::new(0),
            forgotten: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }
    }

    #[inline]
    pub(crate) fn decision(&self, allowed: bool) {
        let counter = if allowed { &self.allowed } else { &self.denied };
        let _previous = counter.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn inserted(&self) {
        let _previous = self.inserted.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn evicted(&self, count: usize) {
        if count > 0 {
            let _previous = self.evicted.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn expired(&self, count: usize) {
        if count > 0 {
            let _previous = self.expired.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn removed(&self, count: usize) {
        if count > 0 {
            let _previous = self.removed.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn remembered(&self) {
        let _previous = self.remembered.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn restored(&self) {
        let _previous = self.restored.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn overflowed(&self) {
        let _previous = self.overflowed.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn forgotten(&self) {
        let _previous = self.forgotten.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn refused(&self) {
        let _previous = self.refused.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn rejected(&self) {
        let _previous = self.rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Adds this set's values into `into`.
    pub(crate) fn add_to(&self, into: &mut Metrics) {
        into.allowed += self.allowed.load(Ordering::Relaxed);
        into.denied += self.denied.load(Ordering::Relaxed);
        into.keys_inserted += self.inserted.load(Ordering::Relaxed);
        into.keys_evicted += self.evicted.load(Ordering::Relaxed);
        into.keys_expired += self.expired.load(Ordering::Relaxed);
        into.keys_removed += self.removed.load(Ordering::Relaxed);
        into.keys_remembered += self.remembered.load(Ordering::Relaxed);
        into.keys_restored += self.restored.load(Ordering::Relaxed);
        into.debts_overflowed += self.overflowed.load(Ordering::Relaxed);
        into.debts_forgotten += self.forgotten.load(Ordering::Relaxed);
        into.keys_refused += self.refused.load(Ordering::Relaxed);
        into.keys_rejected += self.rejected.load(Ordering::Relaxed);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{Counters, Metrics};

    #[test]
    fn test_counters_accumulate_into_metrics() {
        let counters = Counters::new();
        counters.decision(true);
        counters.decision(true);
        counters.decision(false);
        counters.inserted();
        counters.evicted(2);
        counters.expired(0);
        counters.expired(3);
        counters.removed(4);
        counters.remembered();
        counters.restored();
        counters.overflowed();
        counters.forgotten();
        counters.refused();
        counters.rejected();
        let mut metrics = Metrics::default();
        counters.add_to(&mut metrics);
        counters.add_to(&mut metrics);
        assert_eq!(
            metrics,
            Metrics {
                allowed: 4,
                denied: 2,
                keys_inserted: 2,
                keys_evicted: 4,
                keys_expired: 6,
                keys_removed: 8,
                keys_remembered: 2,
                keys_restored: 2,
                debts_overflowed: 2,
                debts_forgotten: 2,
                keys_refused: 2,
                keys_rejected: 2,
            }
        );
    }
}

/// How full the store's recovery tables are: the gauge to alert on **before**
/// eviction starts to fail — before [`Overflow::FailClosed`](crate::Overflow)
/// charges or refuses keys that owed nothing, or
/// [`Overflow::Forget`](crate::Overflow) starts dropping debts. From
/// [`RateLimiter::recovery_load`](crate::RateLimiter::recovery_load).
///
/// Each shard keeps the debts of its evicted, still-limited keys exactly, up
/// to `debt_capacity / shards` of them; past that, the overflow policy takes
/// over. Shards fill independently, so watch [`fullest_shard`](Self::fullest_shard)
/// as well as the aggregate [`utilization`](Self::utilization): alert well
/// before either reaches `1.0` (say at `0.7`), and size the cap up.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct RecoveryLoad {
    /// Live exact debt records: evicted keys still owed, across all shards.
    pub debts_held: u64,
    /// The exact records the tables hold when full (whether or not a shard has
    /// allocated its table yet).
    pub debt_capacity: u64,
    /// The largest per-shard fraction of exact records in use, `0.0..=1.0`.
    pub fullest_shard: f64,
    /// Live cells of the shared overflow sketch ([`Overflow::FailClosed`](crate::Overflow)
    /// only): debts that did not fit the exact table, which may charge keys
    /// that never owed anything.
    pub overflow_cells_live: u64,
    /// The sketch's cells (zero under `Overflow::Forget`).
    pub overflow_cells: u64,
}

impl RecoveryLoad {
    /// The fraction of exact debt records in use, across all shards
    /// (`debts_held / debt_capacity`; `0.0` for an uncapped store).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "a ratio of counts far below 2^52"
    )]
    pub fn utilization(&self) -> f64 {
        if self.debt_capacity == 0 {
            0.0
        } else {
            self.debts_held as f64 / self.debt_capacity as f64
        }
    }
}
