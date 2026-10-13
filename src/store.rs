//! The sharded, bounded-memory per-key state store.
//!
//! State for each key is an [`AlgoState`] (the algorithm's per-key data) plus
//! two small eviction marks. Keys are spread across a fixed number of shards by
//! hash, each shard guarded by its own `RwLock`, so unrelated keys rarely
//! contend: an existing-key check takes only a shard *read* lock (many run
//! concurrently) and the algorithm's own accounting does the rest. Only
//! first-seeing a key takes the shard write lock, briefly.
//!
//! # Layout
//!
//! A shard keeps its entries densely in a `Vec` of slots and finds them through
//! a `hashbrown::HashTable` of slot indices. The key is hashed exactly once per
//! check: disjoint bits of the one hash pick the shard and the table bucket.
//! Shards are padded to their own cache lines so neighbouring shards' locks
//! never share one.
//!
//! # Eviction — O(1) per insert
//!
//! Memory is bounded by eviction that is **lazy, per-shard, and bounded per
//! insert**. It runs only while inserting a new key, under the write lock
//! already held, and does a fixed amount of work:
//!
//! - **Capacity — CLOCK (second chance).** Every check sets the entry's
//!   *referenced* bit (a load, and a store only if it was clear); a new key
//!   starts with it clear. When a full shard needs room, a hand sweeps the
//!   slots, clearing referenced bits, and evicts the first entry whose bit was
//!   already clear — an approximation of least-recently-used in which a key
//!   seen only once (the signature of a unique-key flood) goes first and a key
//!   checked at least once per revolution of the hand is kept. The sweep examines at most [`CLOCK_SCAN`] slots; if
//!   every one was recently used it evicts the slot under the hand anyway, so
//!   one insert never costs more than that bound however large the shard is.
//! - **Idle TTL — incremental sweep.** Each insert also examines the next
//!   [`TTL_SWEEP`] slots from a second cursor and reclaims any idle past the
//!   TTL, so idle state drains continuously instead of in one stop-the-world
//!   pass. The CLOCK hand also prefers an expired entry when it meets one.
//!
//! Removal is a swap-remove from the slot vector plus two table updates, so
//! evicting is O(1).
//!
//! The one shard-wide pass that remains is amortized growth: the slot vector
//! and the index grow by doubling, and the index occasionally rehashes in place
//! to clear tombstones left by removals. Both run under the write lock and
//! touch every key in the shard, so the store keeps capped shards small —
//! the shard count is raised until each holds at most [`MAX_KEYS_PER_SHARD`]
//! keys — and the pass stays short.

use core::time::Duration;

use ahash::RandomState;
use clock_lib::Clock;
use hashbrown::HashTable;

use crate::algo::AlgoState;
use crate::decision::Decision;
use crate::eviction::Eviction;
use crate::key::Key;
#[cfg(feature = "metrics")]
use crate::metrics::{Counters, Metrics};
use crate::snapshot::Snapshot;
use crate::sync::{self, AtomicBool, AtomicU64, Ordering, RwLock};

/// The hash bits that pick a shard. `hashbrown` takes the bucket index from the
/// low bits and a 7-bit tag from the top 7 (57..64), so bits 40..56 (16 bits
/// cover the [`MAX_SHARDS`] maximum) never correlate with either: keys sharing
/// a shard still spread evenly across its table.
const SHARD_SHIFT: u32 = 40;

/// The most shards a store will create. A larger request (including
/// `usize::MAX`) is clamped to it rather than overflowing or exhausting
/// memory; `Builder::try_build` rejects it.
///
/// # Examples
///
/// ```
/// use rate_net::{MAX_SHARDS, RateLimiter};
///
/// assert_eq!(MAX_SHARDS, 65_536);
/// assert_eq!(RateLimiter::per_second(1).with_shards(usize::MAX).shards(), MAX_SHARDS);
/// ```
pub const MAX_SHARDS: usize = 1 << 16;

/// The most keys a capped shard may hold. A shard's index and slot vector
/// occasionally grow or rehash in one pass under its write lock, so the store
/// raises its shard count until every shard's share of the cap fits here:
/// that pass then touches at most this many keys (well under a millisecond).
pub(crate) const MAX_KEYS_PER_SHARD: usize = 1 << 14;

/// The most slots the CLOCK hand examines to evict one entry.
pub(crate) const CLOCK_SCAN: usize = 32;

/// The slots the idle sweep examines per insert.
pub(crate) const TTL_SWEEP: usize = 8;

/// The slots `retain_recent` examines per write-lock acquisition.
const RETAIN_CHUNK: usize = 1_024;

/// The most slots one shard can index (slot indices are `u32`).
const MAX_SLOTS: usize = u32::MAX as usize;

/// Per-key state: the algorithm's data plus the eviction marks.
struct Entry<C: Clock> {
    state: AlgoState<C>,
    /// CLOCK reference bit: set on every check, cleared by the sweeping hand.
    referenced: AtomicBool,
    /// Milliseconds since the limiter epoch at the last check. Maintained only
    /// when an idle TTL is configured, so the common path skips it.
    last_seen_ms: AtomicU64,
}

impl<C: Clock> Entry<C> {
    fn new(state: AlgoState<C>, now_ms: u64) -> Self {
        Self {
            state,
            // A new key starts unreferenced: a key seen once (a flood of
            // unique keys) is the first to go, while a key checked again
            // earns a second chance.
            referenced: AtomicBool::new(false),
            last_seen_ms: AtomicU64::new(now_ms),
        }
    }

    /// Marks the entry used. Both marks are written only when they change, so a
    /// hot key's cache line is not dirtied on every check.
    #[inline]
    fn touch(&self, now_ms: u64, track_idle: bool) {
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
        if track_idle && self.last_seen_ms.load(Ordering::Relaxed) < now_ms {
            self.last_seen_ms.store(now_ms, Ordering::Relaxed);
        }
    }

    /// Whether the entry has been idle for at least `ttl_ms`.
    fn is_expired(&self, now_ms: u64, ttl_ms: Option<u64>) -> bool {
        ttl_ms.is_some_and(|ttl| {
            now_ms.saturating_sub(self.last_seen_ms.load(Ordering::Relaxed)) >= ttl
        })
    }
}

/// One stored key: its hash (kept so a moved slot can be re-indexed without
/// rehashing), the key itself, and its entry.
struct Slot<C: Clock> {
    hash: u64,
    key: Key,
    entry: Entry<C>,
}

/// A shard's contents: dense slots, an index from key hash to slot, and the two
/// eviction cursors.
struct ShardMap<C: Clock> {
    index: HashTable<u32>,
    slots: Vec<Slot<C>>,
    /// CLOCK hand: the next slot the capacity sweep examines.
    hand: usize,
    /// The next slot the idle sweep examines.
    sweep: usize,
}

impl<C: Clock> ShardMap<C> {
    fn new() -> Self {
        Self {
            index: HashTable::new(),
            slots: Vec::new(),
            hand: 0,
            sweep: 0,
        }
    }

    /// The slot position holding `key`, if any.
    #[inline]
    fn position(&self, hash: u64, key: &Key) -> Option<usize> {
        let slots = &self.slots;
        self.index
            .find(hash, |&pos| {
                slots
                    .get(pos as usize)
                    .is_some_and(|slot| slot.hash == hash && slot.key == *key)
            })
            .map(|&pos| pos as usize)
    }

    /// The entry for `key`, if any.
    #[inline]
    fn get(&self, hash: u64, key: &Key) -> Option<&Entry<C>> {
        self.position(hash, key)
            .and_then(|pos| self.slots.get(pos))
            .map(|slot| &slot.entry)
    }

    /// Appends a new key. The caller has checked it is absent and made room.
    fn insert(&mut self, hash: u64, key: Key, entry: Entry<C>) {
        let Self { index, slots, .. } = self;
        // `make_room` keeps the slot count below `MAX_SLOTS`, so the position
        // always fits a `u32`.
        let pos = u32::try_from(slots.len()).unwrap_or(u32::MAX);
        slots.push(Slot { hash, key, entry });
        let _bucket = index.insert_unique(hash, pos, |&p| {
            slots.get(p as usize).map_or(0, |slot| slot.hash)
        });
    }

    /// Removes the slot at `pos` by swap-remove, re-indexing the slot that
    /// moves into its place. O(1).
    fn remove_at(&mut self, pos: usize) -> Option<Slot<C>> {
        let last = self.slots.len().checked_sub(1)?;
        let hash = self.slots.get(pos)?.hash;
        if let Ok(found) = self.index.find_entry(hash, |&p| p as usize == pos) {
            let (_pos, _vacant) = found.remove();
        }
        if pos != last {
            if let Some(moved_hash) = self.slots.get(last).map(|slot| slot.hash) {
                if let Some(target) = self.index.find_mut(moved_hash, |&p| p as usize == last) {
                    // `pos < last <= MAX_SLOTS`, so it fits.
                    *target = u32::try_from(pos).unwrap_or(u32::MAX);
                }
            }
        }
        Some(self.slots.swap_remove(pos))
    }

    /// Reclaims idle-expired entries among the next `budget` slots from the
    /// sweep cursor.
    fn sweep_idle(&mut self, now_ms: u64, ttl_ms: u64, budget: usize) -> usize {
        let mut reclaimed = 0;
        for _ in 0..budget {
            let len = self.slots.len();
            if len == 0 {
                break;
            }
            if self.sweep >= len {
                self.sweep = 0;
            }
            let expired = self
                .slots
                .get(self.sweep)
                .is_some_and(|slot| slot.entry.is_expired(now_ms, Some(ttl_ms)));
            if expired {
                // The swap moved an unexamined slot here; look at it next.
                let _reclaimed = self.remove_at(self.sweep);
                reclaimed += 1;
            } else {
                self.sweep += 1;
            }
        }
        reclaimed
    }

    /// Evicts one entry by CLOCK: the first idle-expired or unreferenced slot
    /// within [`CLOCK_SCAN`] of the hand, else the slot under the hand.
    fn evict_one(&mut self, now_ms: u64, ttl_ms: Option<u64>) -> bool {
        let len = self.slots.len();
        if len == 0 {
            return false;
        }
        let mut victim = None;
        for _ in 0..CLOCK_SCAN.min(len) {
            if self.hand >= len {
                self.hand = 0;
            }
            if let Some(slot) = self.slots.get(self.hand) {
                let entry = &slot.entry;
                if entry.is_expired(now_ms, ttl_ms) || !entry.referenced.load(Ordering::Relaxed) {
                    victim = Some(self.hand);
                    break;
                }
                // Second chance: clear the bit and move on.
                entry.referenced.store(false, Ordering::Relaxed);
            }
            self.hand += 1;
        }
        let pos = victim.unwrap_or(if self.hand >= len { 0 } else { self.hand });
        let evicted = self.remove_at(pos).is_some();
        // The swap moved the newest slot into `pos`; step past it so a key
        // that just arrived gets a full revolution before the hand returns.
        self.hand = pos + 1;
        evicted
    }
}

/// One shard: an independently locked slice of the key space, on its own cache
/// lines so neighbouring shards never falsely share.
#[repr(align(128))]
struct Shard<C: Clock> {
    map: RwLock<ShardMap<C>>,
    /// This shard's counters, beside the lock word every check already
    /// touches. Absent without the `metrics` feature.
    #[cfg(feature = "metrics")]
    counters: Counters,
}

// The metrics hooks. Without the `metrics` feature they are empty and inline
// away, so the counters cost nothing when off.
impl<C: Clock> Shard<C> {
    fn new() -> Self {
        Self {
            map: RwLock::new(ShardMap::new()),
            #[cfg(feature = "metrics")]
            counters: Counters::new(),
        }
    }

    #[inline]
    fn on_decision(&self, decision: Decision) -> Decision {
        #[cfg(feature = "metrics")]
        self.counters.decision(decision.is_allow());
        decision
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self))]
    #[inline]
    fn on_inserted(&self) {
        #[cfg(feature = "metrics")]
        self.counters.inserted();
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self, unused_variables))]
    #[inline]
    fn on_reclaimed(&self, evicted: usize, expired: usize) {
        #[cfg(feature = "metrics")]
        {
            self.counters.evicted(evicted);
            self.counters.expired(expired);
        }
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self, unused_variables))]
    #[inline]
    fn on_removed(&self, count: usize) {
        #[cfg(feature = "metrics")]
        self.counters.removed(count);
    }
}

/// The sharded per-key store.
pub(crate) struct Store<C: Clock> {
    shards: Box<[Shard<C>]>,
    /// `shard_count - 1`; `shard_count` is always a power of two so this masks
    /// the shard bits of a hash down to an index without a division.
    shard_mask: u64,
    /// One random-seeded hasher for the whole store (collision-attack
    /// resistant); its single hash drives both the shard and the table.
    hasher: RandomState,
    /// Maximum live keys per shard (at most [`MAX_SLOTS`]).
    per_shard_cap: usize,
    /// Effective idle expiry in milliseconds, or `None` to never expire.
    idle_ttl_ms: Option<u64>,
}

impl<C: Clock> Store<C> {
    /// Builds a store with `shards` shards (clamped to `1..=MAX_SHARDS`, raised
    /// so no capped shard holds more than `MAX_KEYS_PER_SHARD` keys, then
    /// rounded up to a power of two) and the given eviction policy. An idle
    /// TTL shorter than `min_idle` is raised to it.
    pub(crate) fn new(shards: usize, eviction: Eviction, min_idle: Duration) -> Self {
        let for_cap = eviction
            .max_keys()
            .map_or(1, |max| max.div_ceil(MAX_KEYS_PER_SHARD));
        // `MAX_SHARDS` is a power of two, so rounding a clamped value up never
        // passes it (and never overflows).
        let shard_count = shards.max(for_cap).clamp(1, MAX_SHARDS).next_power_of_two();
        // Spread a total cap evenly across shards; at least one key per shard so
        // a tiny cap with many shards still admits keys.
        let per_shard_cap = eviction.max_keys().map_or(MAX_SLOTS, |max| {
            max.div_ceil(shard_count).clamp(1, MAX_SLOTS)
        });
        // Idle time is measured between millisecond stamps, both floored, so
        // a measured gap can exceed the true one by up to a millisecond. The
        // TTL is rounded up and one millisecond added, so a key is only ever
        // reclaimed once it has truly been idle for the whole TTL.
        let idle_ttl_ms = eviction.idle_ttl().map(|ttl| {
            let nanos = ttl.max(min_idle).as_nanos().div_ceil(1_000_000);
            u64::try_from(nanos).unwrap_or(u64::MAX).saturating_add(1)
        });

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

    /// Whether checks must supply a real `now` for idle tracking.
    pub(crate) const fn tracks_idle(&self) -> bool {
        self.idle_ttl_ms.is_some()
    }

    /// Checks `n` units against `key` as of elapsed time `now`, creating the
    /// key's state from `make_state` if this is the first time the key is seen.
    #[inline]
    pub(crate) fn check(
        &self,
        key: Key,
        n: u32,
        now: Duration,
        make_state: impl FnOnce() -> AlgoState<C>,
    ) -> Decision {
        let hash = self.hasher.hash_one(&key);
        let shard = self.shard(hash);
        let track_idle = self.tracks_idle();
        let now_ms = if track_idle { millis(now) } else { 0 };

        // Fast path: a shared read lock is enough for an existing key. The
        // algorithm does its own accounting, so concurrent checks — of this key
        // or any other key in the shard — proceed without serialising.
        {
            let map = sync::read(&shard.map);
            if let Some(entry) = map.get(hash, &key) {
                entry.touch(now_ms, track_idle);
                return shard.on_decision(entry.state.acquire(n, now));
            }
        }

        // Slow path: first-seen key. Take the write lock, re-check (another
        // thread may have inserted it in the gap), make room, insert.
        let mut map = sync::write(&shard.map);
        if let Some(entry) = map.get(hash, &key) {
            entry.touch(now_ms, track_idle);
            return shard.on_decision(entry.state.acquire(n, now));
        }
        self.make_room(shard, &mut map, now_ms);
        let state = make_state();
        let outcome = state.acquire(n, now);
        map.insert(hash, key, Entry::new(state, now_ms));
        shard.on_inserted();
        shard.on_decision(outcome)
    }

    /// Makes room in a shard about to receive a new key: an incremental idle
    /// sweep, then a CLOCK eviction if the shard is full. Bounded work.
    fn make_room(&self, shard: &Shard<C>, map: &mut ShardMap<C>, now_ms: u64) {
        let expired = match self.idle_ttl_ms {
            Some(ttl_ms) => map.sweep_idle(now_ms, ttl_ms, TTL_SWEEP),
            None => 0,
        };
        let evicted = usize::from(
            map.slots.len() >= self.per_shard_cap && map.evict_one(now_ms, self.idle_ttl_ms),
        );
        shard.on_reclaimed(evicted, expired);
    }

    /// `key`'s remaining allowance as of `now`, or `None` if the key has no
    /// state. Takes only the shard read lock, consumes nothing, and leaves the
    /// key's eviction recency untouched.
    pub(crate) fn peek(&self, key: &Key, now: Duration) -> Option<Snapshot> {
        let hash = self.hasher.hash_one(key);
        let map = sync::read(&self.shard(hash).map);
        map.get(hash, key).map(|entry| entry.state.peek(now))
    }

    /// Restores `key`'s full allowance, keeping it tracked. Returns whether the
    /// key had state.
    pub(crate) fn reset(&self, key: &Key) -> bool {
        let hash = self.hasher.hash_one(key);
        let map = sync::read(&self.shard(hash).map);
        map.get(hash, key).is_some_and(|entry| {
            entry.state.reset();
            true
        })
    }

    /// Forgets `key` entirely. Returns whether the key had state.
    pub(crate) fn remove(&self, key: &Key) -> bool {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        let mut map = sync::write(&shard.map);
        let removed = map
            .position(hash, key)
            .and_then(|pos| map.remove_at(pos))
            .is_some();
        shard.on_removed(usize::from(removed));
        removed
    }

    /// Forgets every key and releases the shards' memory.
    pub(crate) fn clear(&self) {
        for shard in self.shards.iter() {
            let mut map = sync::write(&shard.map);
            shard.on_removed(map.slots.len());
            *map = ShardMap::new();
        }
    }

    /// Forgets every key whose state is indistinguishable from a fresh key's —
    /// its whole allowance is back — so no decision changes. Returns how many
    /// were removed.
    ///
    /// Each shard is walked in chunks of [`RETAIN_CHUNK`] slots, releasing the
    /// write lock between chunks, so concurrent checks never wait behind a
    /// whole-shard pass.
    pub(crate) fn retain_recent(&self, now: Duration) -> usize {
        let mut removed = 0;
        for shard in self.shards.iter() {
            // Walk downward: a swap-remove moves an already-examined slot into
            // the gap, so nothing is skipped.
            let mut cursor = usize::MAX;
            loop {
                let mut map = sync::write(&shard.map);
                cursor = cursor.min(map.slots.len());
                let stop = cursor.saturating_sub(RETAIN_CHUNK);
                let mut chunk_removed = 0;
                while cursor > stop {
                    cursor -= 1;
                    let full = map
                        .slots
                        .get(cursor)
                        .is_some_and(|slot| slot.entry.state.is_full(now));
                    if full && map.remove_at(cursor).is_some() {
                        chunk_removed += 1;
                    }
                }
                shard.on_removed(chunk_removed);
                removed += chunk_removed;
                if cursor == 0 {
                    break;
                }
            }
        }
        removed
    }

    /// Releases memory the shards hold beyond their current keys.
    pub(crate) fn shrink_to_fit(&self) {
        for shard in self.shards.iter() {
            let mut map = sync::write(&shard.map);
            let ShardMap { index, slots, .. } = &mut *map;
            slots.shrink_to_fit();
            index.shrink_to_fit(|&p| slots.get(p as usize).map_or(0, |slot| slot.hash));
        }
    }

    /// The number of keys with live state across all shards. A momentary,
    /// advisory snapshot.
    pub(crate) fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| sync::read(&shard.map).slots.len())
            .sum()
    }

    /// The counters summed across every shard.
    #[cfg(feature = "metrics")]
    pub(crate) fn metrics(&self) -> Metrics {
        let mut metrics = Metrics::default();
        for shard in self.shards.iter() {
            shard.counters.add_to(&mut metrics);
        }
        metrics
    }

    /// The number of shards (a power of two).
    pub(crate) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// The shard that owns `hash`.
    #[inline]
    fn shard(&self, hash: u64) -> &Shard<C> {
        let index = ((hash >> SHARD_SHIFT) & self.shard_mask) as usize;
        // `index <= shard_mask < shards.len()`.
        &self.shards[index]
    }
}

/// Whole milliseconds in `now`, saturating.
#[inline]
fn millis(now: Duration) -> u64 {
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use core::time::Duration;
    use std::sync::Arc;
    use std::time::Instant;

    use better_bucket::Bucket;
    use clock_lib::{ManualClock, SystemClock};

    use super::{CLOCK_SCAN, Store, TTL_SWEEP};
    use crate::algo::AlgoState;
    use crate::eviction::Eviction;
    use crate::key::Key;

    fn make_store(shards: usize, eviction: Eviction) -> Store<SystemClock> {
        Store::new(shards, eviction, Duration::ZERO)
    }

    fn token_state(rate: u32) -> impl Fn() -> AlgoState<SystemClock> {
        move || AlgoState::TokenBucket(Bucket::per_second(rate))
    }

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    impl<C: clock_lib::Clock> Store<C> {
        /// Every slot is indexed exactly once, at its own position.
        fn assert_consistent(&self) {
            for shard in self.shards.iter() {
                let map = super::sync::read(&shard.map);
                assert_eq!(map.index.len(), map.slots.len());
                for (pos, slot) in map.slots.iter().enumerate() {
                    assert_eq!(map.position(slot.hash, &slot.key), Some(pos));
                }
            }
        }
    }

    #[test]
    fn test_shard_count_rounds_up_to_power_of_two() {
        assert_eq!(make_store(0, Eviction::unbounded()).shard_count(), 1);
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
        store.assert_consistent();
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
        store.assert_consistent();
    }

    #[test]
    fn test_capacity_boundaries_per_shard() {
        // 0 is treated as 1 per shard; cap-1, cap, cap+1 on one shard.
        for (cap, expect) in [(0usize, 1usize), (1, 1), (4, 4), (5, 5)] {
            let store = make_store(1, Eviction::capacity(cap));
            let make = token_state(10);
            for k in 0..(cap as u64 + 10) {
                let _ = store.check(Key::from(k), 1, at(0), &make);
            }
            assert_eq!(store.len(), expect, "cap {cap}");
            store.assert_consistent();
        }
    }

    #[test]
    fn test_ttl_reclaims_idle_keys_on_later_insert() {
        let store = make_store(1, Eviction::new(1_000, Duration::from_millis(1000)));
        let make = token_state(10);

        let _ = store.check(Key::from("idle"), 1, at(0), &make);
        assert_eq!(store.len(), 1);

        let _ = store.check(Key::from("fresh"), 1, at(2_000), &make);
        assert_eq!(store.len(), 1, "the idle key should have been reclaimed");
        store.assert_consistent();
    }

    #[test]
    fn test_ttl_sweep_is_incremental_and_eventually_complete() {
        let store = make_store(1, Eviction::new(1_000, Duration::from_millis(1000)));
        let make = token_state(10);
        for k in 0..100u64 {
            let _ = store.check(Key::from(k), 1, at(0), &make);
        }
        // One late insert examines at most TTL_SWEEP slots, not all 100.
        let _ = store.check(Key::from("late-0"), 1, at(5_000), &make);
        assert_eq!(store.len(), 100 - TTL_SWEEP + 1);
        // Enough further inserts drain every idle key.
        for k in 1..=100u64 {
            let _ = store.check(Key::from(format!("late-{k}")), 1, at(5_000), &make);
        }
        assert_eq!(store.len(), 101);
        store.assert_consistent();
    }

    #[test]
    fn test_short_ttl_is_raised_to_min_idle() {
        let store: Store<SystemClock> = Store::new(
            1,
            Eviction::idle(Duration::from_millis(10)),
            Duration::from_secs(1),
        );
        // Rounded up and padded by a millisecond for the floored stamps.
        assert_eq!(store.idle_ttl_ms, Some(1_001));
    }

    #[test]
    fn test_idle_ttl_never_reclaims_a_key_before_the_full_ttl() {
        // The verifier's case: a key last seen at 0.999ms and idle for
        // 999.001ms has floored stamps 0 and 1000, a measured gap of the whole
        // 1s TTL, although it has not been idle that long.
        let store = make_store(1, Eviction::new(1_000, Duration::from_secs(1)));
        let make = token_state(1_000);
        let key = Key::from("k");
        let _ = store.check(key.clone(), 1_000, Duration::from_micros(999), &make);
        let _ = store.check(Key::from("other"), 1, Duration::from_millis(1_000), &make);
        assert!(
            store.peek(&key, Duration::ZERO).is_some(),
            "reclaimed early"
        );
        // Once truly idle for longer than the TTL, it goes.
        let _ = store.check(Key::from("later"), 1, Duration::from_millis(1_002), &make);
        assert!(store.peek(&key, Duration::ZERO).is_none());
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
        store.assert_consistent();
    }

    #[test]
    fn test_hot_key_state_survives_a_unique_key_flood() {
        // A drained hot key keeps its (empty) state through a flood, so the
        // flood cannot be used to reset it.
        let store = make_store(1, Eviction::capacity(64));
        // An hour-long period: no real-clock refill during the test.
        let make = || AlgoState::TokenBucket(Bucket::per_duration(5, Duration::from_secs(3_600)));
        let make = &make;
        for _ in 0..5 {
            assert!(store.check(Key::from("hot"), 1, at(0), make).is_allow());
        }
        for k in 0..10_000u64 {
            let _ = store.check(Key::from(k), 1, at(0), make);
            if k % 8 == 0 {
                assert!(store.check(Key::from("hot"), 1, at(0), make).is_deny());
            }
        }
        assert!(store.check(Key::from("hot"), 1, at(0), make).is_deny());
    }

    #[test]
    fn test_clock_scan_is_bounded_when_every_key_is_hot() {
        // Every slot referenced: one eviction still examines at most
        // CLOCK_SCAN slots and makes room.
        let store = make_store(1, Eviction::capacity(CLOCK_SCAN * 4));
        let make = token_state(10);
        for k in 0..(CLOCK_SCAN as u64 * 4) {
            let _ = store.check(Key::from(k), 1, at(0), &make);
        }
        // A second check of every key sets every referenced bit.
        for k in 0..(CLOCK_SCAN as u64 * 4) {
            let _ = store.check(Key::from(k), 1, at(0), &make);
        }
        let _ = store.check(Key::from("new"), 1, at(0), &make);
        assert_eq!(store.len(), CLOCK_SCAN * 4);
        store.assert_consistent();
    }

    #[test]
    #[cfg_attr(miri, ignore = "a timing bound is meaningless under Miri")]
    fn test_insert_at_capacity_is_constant_time() {
        // Compares inserts into a full shard (each one evicts) with inserts
        // into an empty one, on the same host at the same moment, so a loaded
        // machine slows both alike. A full-shard scan per eviction made the
        // first ~1000x the second; CLOCK keeps them within a small factor.
        // The 16,384-key shard is the largest a capped store creates.
        let full = make_store(1, Eviction::capacity(16_384));
        let empty = make_store(1, Eviction::unbounded());
        let make = token_state(10);
        for k in 0..16_384u64 {
            let _ = full.check(Key::from(k), 1, at(0), &make);
        }
        let time = |store: &Store<SystemClock>, from: u64| {
            let start = Instant::now();
            for k in from..from + 20_000 {
                let _ = store.check(Key::from(k), 1, at(0), &make);
            }
            start.elapsed()
        };
        let baseline = time(&empty, 0);
        let at_cap = time(&full, 16_384);
        assert!(
            at_cap < baseline * 20 + Duration::from_millis(200),
            "inserts at capacity took {at_cap:?}, empty-store inserts {baseline:?}"
        );
        assert_eq!(full.len(), 16_384);
    }

    #[test]
    fn test_large_caps_raise_the_shard_count() {
        use super::MAX_KEYS_PER_SHARD;
        // One shard requested, a million-key cap: shards are added until each
        // holds at most MAX_KEYS_PER_SHARD, so no shard-wide pass is large.
        let store = make_store(1, Eviction::capacity(1_000_000));
        assert_eq!(store.shard_count(), 64);
        assert!(store.per_shard_cap <= MAX_KEYS_PER_SHARD);
        // Small caps and unbounded stores keep the requested count.
        assert_eq!(make_store(2, Eviction::capacity(1_000)).shard_count(), 2);
        assert_eq!(make_store(2, Eviction::unbounded()).shard_count(), 2);
        // An absurd cap stops at the shard ceiling.
        assert_eq!(
            make_store(1, Eviction::capacity(usize::MAX)).shard_count(),
            super::MAX_SHARDS
        );
    }

    #[test]
    fn test_manual_clock_store_refills_across_window() {
        let clock = Arc::new(ManualClock::new());
        let store: Store<Arc<ManualClock>> = Store::new(4, Eviction::unbounded(), Duration::ZERO);
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

    #[test]
    fn test_peek_reset_and_remove_single_keys() {
        let store = make_store(2, Eviction::unbounded());
        let make = token_state(10);
        let key = Key::from("k");
        assert_eq!(store.peek(&key, at(0)), None);
        assert!(!store.reset(&key));
        assert!(!store.remove(&key));

        assert!(store.check(key.clone(), 4, at(0), &make).is_allow());
        assert_eq!(store.peek(&key, at(0)).map(|s| s.remaining()), Some(6));
        assert!(store.reset(&key));
        assert_eq!(store.peek(&key, at(0)).map(|s| s.remaining()), Some(10));
        assert!(store.remove(&key));
        assert_eq!(store.peek(&key, at(0)), None);
        assert_eq!(store.len(), 0);
        store.assert_consistent();
    }

    #[test]
    fn test_clear_and_shrink_forget_everything() {
        let store = make_store(4, Eviction::unbounded());
        let make = token_state(10);
        for k in 0..500u64 {
            let _ = store.check(Key::from(k), 1, at(0), &make);
        }
        assert_eq!(store.len(), 500);
        store.shrink_to_fit();
        store.assert_consistent();
        store.clear();
        assert_eq!(store.len(), 0);
        store.assert_consistent();
        assert!(store.check(Key::from(1u64), 10, at(0), &make).is_allow());
    }

    #[test]
    fn test_retain_recent_removes_only_full_keys() {
        // An hour-long period: nothing refills during the test.
        let make = || AlgoState::TokenBucket(Bucket::per_duration(5, Duration::from_secs(3_600)));
        let store = make_store(2, Eviction::unbounded());
        // 3_000 keys: spans several retain chunks per shard.
        for k in 0..3_000u64 {
            let n = if k % 3 == 0 { 0 } else { 1 };
            let _ = store.check(Key::from(k), n, at(0), make);
        }
        // Every third key spent nothing, so it is full and forgettable.
        assert_eq!(store.retain_recent(at(0)), 1_000);
        assert_eq!(store.len(), 2_000);
        store.assert_consistent();
        for k in 0..3_000u64 {
            let tracked = store.peek(&Key::from(k), at(0)).is_some();
            assert_eq!(tracked, k % 3 != 0, "key {k}");
        }
        assert_eq!(store.retain_recent(at(0)), 0);
    }

    proptest::proptest! {
        /// Under any interleaving of inserts, repeats, and idle gaps the index
        /// stays consistent with the slots and the cap holds.
        #[test]
        fn prop_store_index_stays_consistent(
            cap in 1usize..40,
            ttl in proptest::option::of(1u64..50),
            ops in proptest::collection::vec((0u64..80, 0u64..20), 0..300),
        ) {
            let eviction = match ttl {
                Some(ms) => Eviction::new(cap, Duration::from_millis(ms)),
                None => Eviction::capacity(cap),
            };
            let store = make_store(2, eviction);
            let make = token_state(3);
            let mut now = 0u64;
            for (key, advance) in ops {
                now += advance;
                let _ = store.check(Key::from(key), 1, at(now), &make);
                proptest::prop_assert!(store.len() <= cap.div_ceil(2).max(1) * 2);
            }
            store.assert_consistent();
        }
    }
}
