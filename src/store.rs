//! The sharded, bounded-memory per-key state store.
//!
//! State for each key is an [`AlgoState`] (the algorithm's per-key data) plus a
//! last-seen timestamp. Keys are spread across a fixed number of shards by hash,
//! each shard guarded by its own `RwLock`, so unrelated keys never contend: an
//! existing-key check takes only a shard *read* lock (many run concurrently) and
//! the algorithm's own accounting does the rest. Only first-seeing a key takes
//! the shard write lock, briefly.
//!
//! Memory is bounded by eviction that is **lazy and per-shard**: it runs while
//! inserting a new key, under the write lock already held, and only ever looks
//! at the one shard being written — never the whole store, never a background
//! thread. Idle keys past the TTL are dropped; if the shard is at capacity, its
//! least-recently-seen key is evicted to make room.

use core::time::Duration;
use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use clock_lib::Clock;

use crate::algo::AlgoState;
use crate::decision::Decision;
use crate::eviction::Eviction;
use crate::key::Key;

/// Per-key state: the algorithm's data, plus the last time the key was checked
/// (monotonic milliseconds since the limiter's epoch), used for idle expiry and
/// least-recently-seen eviction.
struct Entry<C: Clock> {
    state: AlgoState<C>,
    last_seen_ms: AtomicU64,
}

/// One shard of the store: an independently locked slice of the key space.
struct Shard<C: Clock> {
    map: RwLock<HashMap<Key, Entry<C>>>,
}

impl<C: Clock> Shard<C> {
    fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }
}

/// The sharded per-key store.
pub(crate) struct Store<C: Clock> {
    shards: Box<[Shard<C>]>,
    /// `shard_count - 1`; `shard_count` is always a power of two so this masks a
    /// hash down to a shard index without a division.
    shard_mask: u64,
    hasher: RandomState,
    /// Maximum live keys per shard, or `None` for an unbounded key count.
    per_shard_cap: Option<usize>,
    /// Idle expiry in milliseconds, or `None` to never expire on idle.
    idle_ttl_ms: Option<u64>,
}

impl<C: Clock> Store<C> {
    /// Builds a store with `shards` shards (rounded up to a power of two, at
    /// least one) and the given eviction policy.
    pub(crate) fn new(shards: usize, eviction: Eviction) -> Self {
        let shard_count = shards.max(1).next_power_of_two();
        // Spread a total cap evenly across shards; at least one key per shard so
        // a tiny cap with many shards still admits keys.
        let per_shard_cap = eviction
            .max_keys()
            .map(|max| max.div_ceil(shard_count).max(1));
        let idle_ttl_ms = eviction
            .idle_ttl()
            .map(|ttl| u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX));

        let shards = (0..shard_count)
            .map(|_| Shard::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            shards,
            shard_mask: shard_count as u64 - 1,
            hasher: RandomState::new(),
            per_shard_cap,
            idle_ttl_ms,
        }
    }

    /// Checks `n` units against `key` as of elapsed time `now`, creating the
    /// key's state from `make_state` if this is the first time the key is seen.
    pub(crate) fn check(
        &self,
        key: Key,
        n: u32,
        now: Duration,
        make_state: impl FnOnce() -> AlgoState<C>,
    ) -> Decision {
        let now_ms = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
        let shard = self.shard_for(&key);

        // Fast path: a shared read lock is enough for an existing key. The
        // algorithm does its own accounting, so concurrent checks — of this key
        // or any other key in the shard — proceed without serialising.
        {
            let guard = read_guard(&shard.map);
            if let Some(entry) = guard.get(&key) {
                entry.last_seen_ms.store(now_ms, Ordering::Relaxed);
                return entry.state.acquire(n, now);
            }
        }

        // Slow path: first-seen key. Take the write lock, re-check (another
        // thread may have inserted it in the gap), evict to make room, insert.
        let mut guard = write_guard(&shard.map);
        if let Some(entry) = guard.get(&key) {
            entry.last_seen_ms.store(now_ms, Ordering::Relaxed);
            return entry.state.acquire(n, now);
        }

        self.evict_for_insert(&mut guard, now_ms);
        let state = make_state();
        let outcome = state.acquire(n, now);
        let _ = guard.insert(
            key,
            Entry {
                state,
                last_seen_ms: AtomicU64::new(now_ms),
            },
        );
        outcome
    }

    /// The number of keys with live state across all shards. A momentary,
    /// advisory snapshot.
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| read_guard(&shard.map).len())
            .sum()
    }

    /// The number of shards (a power of two).
    pub(crate) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    fn shard_for(&self, key: &Key) -> &Shard<C> {
        let index = (self.hasher.hash_one(key) & self.shard_mask) as usize;
        &self.shards[index]
    }

    /// Makes room in a shard about to receive a new key: drop idle-expired keys,
    /// then, if still at capacity, evict the least-recently-seen key. Runs under
    /// the caller's write lock and touches only this shard.
    fn evict_for_insert(&self, map: &mut HashMap<Key, Entry<C>>, now_ms: u64) {
        if let Some(ttl) = self.idle_ttl_ms {
            map.retain(|_, entry| {
                now_ms.saturating_sub(entry.last_seen_ms.load(Ordering::Relaxed)) < ttl
            });
        }

        if let Some(cap) = self.per_shard_cap {
            while map.len() >= cap {
                let victim = map
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_seen_ms.load(Ordering::Relaxed))
                    .map(|(key, _)| key.clone());
                match victim {
                    Some(key) => {
                        let _ = map.remove(&key);
                    }
                    None => break,
                }
            }
        }
    }
}

/// Recovers a read guard even if a previous holder panicked. A poisoned limiter
/// shard should degrade gracefully — keep limiting — rather than propagate a
/// panic into every caller.
fn read_guard<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Recovers a write guard even if a previous holder panicked. See [`read_guard`].
fn write_guard<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use core::time::Duration;
    use std::sync::Arc;

    use better_bucket::Bucket;
    use clock_lib::{ManualClock, SystemClock};

    use super::Store;
    use crate::algo::AlgoState;
    use crate::eviction::Eviction;
    use crate::key::Key;

    fn make_store(shards: usize, eviction: Eviction) -> Store<SystemClock> {
        Store::new(shards, eviction)
    }

    fn token_state(rate: u32) -> impl Fn() -> AlgoState<SystemClock> {
        move || AlgoState::TokenBucket(Bucket::per_second(rate))
    }

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_shard_count_rounds_up_to_power_of_two() {
        assert_eq!(make_store(1, Eviction::unbounded()).shard_count(), 1);
        assert_eq!(make_store(5, Eviction::unbounded()).shard_count(), 8);
        assert_eq!(make_store(16, Eviction::unbounded()).shard_count(), 16);
    }

    #[test]
    fn test_first_check_creates_one_key() {
        let store = make_store(4, Eviction::unbounded());
        let make = token_state(10);
        assert!(store.check(Key::from("a"), 1, at(0), &make).is_allow());
        assert_eq!(store.len(), 1);
        assert!(store.check(Key::from("a"), 1, at(0), &make).is_allow());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_capacity_bounds_total_keys_under_unique_flood() {
        let shards = 8;
        let cap = 100usize;
        let store = make_store(shards, Eviction::capacity(cap));
        let make = token_state(10);

        for k in 0..10_000u64 {
            let _ = store.check(Key::from(k), 1, at(k), &make);
        }

        let per_shard_cap = cap.div_ceil(shards).max(1);
        let bound = per_shard_cap * shards;
        assert!(
            store.len() <= bound,
            "flood grew to {} keys, bound {bound}",
            store.len()
        );
    }

    #[test]
    fn test_ttl_reclaims_idle_keys_on_later_insert() {
        let store = make_store(1, Eviction::idle(Duration::from_millis(1000)));
        let make = token_state(10);

        let _ = store.check(Key::from("idle"), 1, at(0), &make);
        assert_eq!(store.len(), 1);

        let _ = store.check(Key::from("fresh"), 1, at(2_000), &make);
        assert_eq!(store.len(), 1, "the idle key should have been reclaimed");
    }

    #[test]
    fn test_recently_seen_key_survives_eviction_pressure() {
        let store = make_store(1, Eviction::capacity(4));
        let make = token_state(1_000);

        let mut now = 0u64;
        for round in 0..50u64 {
            now += 1;
            assert!(store.check(Key::from("hot"), 1, at(now), &make).is_allow());
            now += 1;
            let _ = store.check(Key::from(round), 1, at(now - 1), &make);
        }

        now += 10_000;
        assert!(store.check(Key::from("hot"), 1, at(now), &make).is_allow());
    }

    #[test]
    fn test_manual_clock_store_refills_across_window() {
        let clock = Arc::new(ManualClock::new());
        let store: Store<Arc<ManualClock>> = Store::new(4, Eviction::unbounded());
        let clock_for_make = Arc::clone(&clock);
        let make = move || {
            AlgoState::TokenBucket(Bucket::per_second(3).with_clock(Arc::clone(&clock_for_make)))
        };

        for _ in 0..3 {
            assert!(store.check(Key::from("k"), 1, at(0), &make).is_allow());
        }
        assert!(store.check(Key::from("k"), 1, at(0), &make).is_deny());

        clock.advance(Duration::from_secs(1));
        assert!(store.check(Key::from("k"), 1, at(1_000), &make).is_allow());
    }
}
