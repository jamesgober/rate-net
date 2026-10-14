//! The sharded, bounded-memory per-key state store.
//!
//! State for each key is its algorithm's [`KeyState`] plus two small eviction
//! marks; the store is generic over the state type, so a key holds exactly its
//! algorithm's state ([`Keyed`] picks the typed store once per limiter). Keys are spread across a fixed number of shards by
//! hash, each shard guarded by its own `RwLock`, so unrelated keys rarely
//! contend: an existing-key check takes only a shard *read* lock (many run
//! concurrently) and the algorithm's own accounting does the rest. Only
//! first-seeing a key takes the shard write lock, briefly.
//!
//! # Layout
//!
//! A shard keeps its entries densely in a `Vec` of slots and finds them through
//! a `hashbrown::HashTable` of slot indices. The key is hashed exactly once per
//! check, from a borrowed view of it — an owned [`Key`] is built only when a key
//! is first stored, so checking a long key never allocates. Disjoint bits of the
//! one hash pick the shard and the table bucket. Shards are padded to their own
//! cache lines so neighbouring shards' locks never share one.
//!
//! # Eviction — O(1) per insert, and never a reset
//!
//! Memory is bounded by eviction that is **lazy, per-shard, and bounded per
//! insert**. It runs only while inserting a new key, under the write lock
//! already held, and does a fixed amount of work:
//!
//! - **Capacity — CLOCK (second chance), recovered keys first.** Every check
//!   sets the entry's *referenced* bit (a load, and a store only if it was
//!   clear); a new key starts with it clear. When a full shard needs room, a
//!   hand sweeps at most [`CLOCK_SCAN`] slots, clearing referenced bits. The
//!   first unreferenced (or idle-expired) entry whose allowance has fully
//!   recovered is evicted — forgetting it loses nothing. Failing that, the first
//!   unreferenced entry is evicted, and failing that the entry under the hand,
//!   so one insert never costs more than the scan bound however large the
//!   shard is.
//! - **Idle TTL — incremental sweep.** Each insert also examines the next
//!   [`TTL_SWEEP`] slots from a second cursor and reclaims any idle past the
//!   TTL, so idle state drains continuously instead of in one stop-the-world
//!   pass.
//!
//! # The recovery table
//!
//! Evicting a key that has *not* recovered would hand it a fresh allowance when
//! it returns — a way to reset any key's limit by flooding the store with
//! others. So whenever an evicted key still owes part of its allowance, the
//! shard records what it owed: how many units were spent and when they would
//! all have been back (a 16-byte [`Record`], keyed by a 32-bit tag of the key's
//! hash, in a table of half the shard's capacity allocated on first use). When
//! a key is inserted the table is consulted, and a key that returns before its
//! debt is repaid resumes no more permissive than it left (see
//! [`KeyState::restore`]); its record is then cleared.
//!
//! Each key has two candidate records. If both are held by other keys' live
//! debts, the new debt is *merged* into one of them — the record keeps the
//! larger debt and the later deadline and loses its tag, so it then applies to
//! every new key that hashes to it until it expires. A merge therefore never
//! forgets a debt; it can only make an unrelated new key start drained (fail
//! closed). Merges happen only when more unrecovered keys are evicted within
//! one recovery period than the table holds — a flood far beyond the cap — and
//! are counted in `Metrics::records_merged`.
//!
//! Removal is a swap-remove from the slot vector plus two table updates, so
//! evicting is O(1). The one shard-wide pass that remains is amortized growth:
//! the slot vector and the index grow by doubling, and the index occasionally
//! rehashes in place to clear tombstones left by removals. Both run under the
//! write lock and touch every key in the shard, so the store keeps capped
//! shards small — the shard count is raised until each holds at most
//! [`MAX_KEYS_PER_SHARD`] keys — and the pass stays short.

use core::time::Duration;

use ahash::RandomState;
use better_bucket::BucketCore;
use hashbrown::HashTable;

use crate::algo::{Debt, KeyState, closed};
#[cfg(feature = "algorithms")]
use crate::algo::{FixedWindow, SlidingCounter, SlidingLog};
use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::eviction::Eviction;
#[cfg(feature = "algorithms")]
use crate::gcra::Gcra;
use crate::key::{Key, KeyRef};
#[cfg(feature = "metrics")]
use crate::metrics::{Counters, Metrics};
use crate::quota::Quota;
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

/// The fewest and most records a shard's recovery table holds.
const MIN_RECORDS: usize = 8;
const MAX_RECORDS: usize = 1 << 13;

/// The tag of a merged record, which applies to every key that maps to it.
const MERGED: u32 = 0;

/// An odd 64-bit constant (the golden ratio) used to remix a key's hash for the
/// recovery table, so the table's slot and tag bits are independent of the bits
/// that chose the shard and the index bucket.
const MIX: u64 = 0x9E37_79B9_7F4A_7C15;

/// One stored key: the algorithm's state, the eviction marks, the key's hash
/// (kept so a moved slot can be re-indexed without rehashing), and the key.
///
/// One flat struct, so the marks, hash, and key share the cache lines the
/// state's alignment already pads out (a token-bucket key is 128 bytes).
struct Slot<S> {
    state: S,
    /// CLOCK reference bit: set on every check, cleared by the sweeping hand.
    referenced: AtomicBool,
    /// Milliseconds since the limiter epoch at the last check. Maintained only
    /// when an idle TTL is configured, so the common path skips it.
    last_seen_ms: AtomicU64,
    hash: u64,
    key: Key,
}

impl<S: KeyState> Slot<S> {
    fn new(hash: u64, key: Key, state: S, now_ms: u64) -> Self {
        Self {
            state,
            // A new key starts unreferenced: a key seen once (a flood of
            // unique keys) is the first to go, while a key checked again
            // earns a second chance.
            referenced: AtomicBool::new(false),
            last_seen_ms: AtomicU64::new(now_ms),
            hash,
            key,
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

/// What an evicted, unrecovered key owed: see the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Record {
    /// A 32-bit tag of the key's hash, never [`MERGED`] for a single key.
    tag: u32,
    /// Units the key had spent and not yet recovered.
    spent: u32,
    /// When they would all have been back, in whole milliseconds since the
    /// limiter epoch (rounded up). A record is live while this is in the future.
    until_ms: u64,
}

impl Record {
    const fn is_live(&self, now_ms: u64) -> bool {
        self.until_ms > now_ms
    }

    const fn debt(&self) -> Debt {
        Debt {
            spent: self.spent,
            until: Duration::from_millis(self.until_ms),
        }
    }

    /// Folds `other` in, keeping the larger debt and the later deadline.
    fn absorb(&mut self, spent: u32, until_ms: u64) {
        self.spent = self.spent.max(spent);
        self.until_ms = self.until_ms.max(until_ms);
    }
}

/// Where a key's records live in a recovery table, and its tag.
#[derive(Clone, Copy)]
struct Probe {
    first: usize,
    second: usize,
    tag: u32,
}

impl Probe {
    /// The probe for `hash` in a table of `len` records (a power of two).
    fn new(hash: u64, len: usize) -> Self {
        let mixed = hash.wrapping_mul(MIX);
        let mask = len - 1;
        // The top bits of the product depend on every bit of the hash; take the
        // two slots from there and the tag from the hash's own low bits.
        let first = (mixed >> 40) as usize & mask;
        let mut second = (mixed >> 20) as usize & mask;
        if second == first {
            second = (first + 1) & mask;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the tag is deliberately the hash's low 32 bits"
        )]
        let low = hash as u32;
        let tag = match low {
            MERGED => 1,
            tag => tag,
        };
        Self { first, second, tag }
    }
}

/// Debts of keys evicted before they recovered. Empty until first needed.
struct Recovery {
    records: Box<[Record]>,
}

impl Recovery {
    fn new() -> Self {
        Self {
            records: Box::new([]),
        }
    }

    /// Records that the key with `hash` owes `debt`, allocating a `len`-record
    /// table on first use. Returns whether the record had to be merged with
    /// another key's.
    fn remember(&mut self, hash: u64, debt: Debt, now_ms: u64, len: usize) -> bool {
        if self.records.is_empty() {
            self.records = vec![Record::default(); len].into_boxed_slice();
        }
        let until_ms = ceil_millis(debt.until);
        let probe = Probe::new(hash, self.records.len());
        let (first, second) = (
            self.records.get(probe.first).copied().unwrap_or_default(),
            self.records.get(probe.second).copied().unwrap_or_default(),
        );
        let pick = |record: &Record| record.is_live(now_ms) && record.tag == probe.tag;
        let (target, merged) = if pick(&first) {
            (probe.first, false)
        } else if pick(&second) {
            (probe.second, false)
        } else if !first.is_live(now_ms) {
            (probe.first, false)
        } else if !second.is_live(now_ms) {
            (probe.second, false)
        } else if first.tag == MERGED {
            (probe.first, true)
        } else if second.tag == MERGED || second.until_ms < first.until_ms {
            (probe.second, true)
        } else {
            (probe.first, true)
        };
        if let Some(record) = self.records.get_mut(target) {
            if merged {
                record.tag = MERGED;
                record.absorb(debt.spent, until_ms);
            } else if record.is_live(now_ms) {
                record.absorb(debt.spent, until_ms);
            } else {
                *record = Record {
                    tag: probe.tag,
                    spent: debt.spent,
                    until_ms,
                };
            }
        }
        merged
    }

    /// The debt that applies to a new key with `hash`: its own record (cleared,
    /// since the key now carries the debt itself) combined with any merged
    /// record it maps to.
    fn take(&mut self, hash: u64, now_ms: u64) -> Option<Debt> {
        let owed = self.peek(hash, now_ms);
        if owed.is_some() {
            let _cleared = self.forget(hash);
        }
        owed
    }

    /// The debt that applies to the key with `hash`, without clearing it.
    fn peek(&self, hash: u64, now_ms: u64) -> Option<Debt> {
        let probe = self.probe(hash)?;
        let mut owed: Option<Record> = None;
        for pos in [probe.first, probe.second] {
            if let Some(record) = self.records.get(pos) {
                if record.is_live(now_ms) && (record.tag == probe.tag || record.tag == MERGED) {
                    owed.get_or_insert(Record::default())
                        .absorb(record.spent, record.until_ms);
                }
            }
        }
        owed.map(|record| record.debt())
    }

    /// Clears the key's own record (not a merged one). Returns whether there
    /// was a live one.
    fn forget(&mut self, hash: u64) -> bool {
        let Some(probe) = self.probe(hash) else {
            return false;
        };
        let mut found = false;
        for pos in [probe.first, probe.second] {
            if let Some(record) = self.records.get_mut(pos) {
                if record.tag == probe.tag && record.until_ms > 0 {
                    found = true;
                    *record = Record::default();
                }
            }
        }
        found
    }

    fn probe(&self, hash: u64) -> Option<Probe> {
        (!self.records.is_empty()).then(|| Probe::new(hash, self.records.len()))
    }

    /// Heap bytes the table holds.
    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        core::mem::size_of_val(&*self.records)
    }
}

/// A shard's contents: dense slots, an index from key hash to slot, the two
/// eviction cursors, and the recovery table.
struct ShardMap<S> {
    index: HashTable<u32>,
    slots: Vec<Slot<S>>,
    /// CLOCK hand: the next slot the capacity sweep examines.
    hand: usize,
    /// The next slot the idle sweep examines.
    sweep: usize,
    recovery: Recovery,
}

/// How many keys one eviction pass reclaimed, by kind, for the counters.
#[derive(Default)]
struct Reclaimed {
    evicted: usize,
    expired: usize,
    remembered: usize,
    merged: usize,
}

impl<S: KeyState> ShardMap<S> {
    fn new() -> Self {
        Self {
            index: HashTable::new(),
            slots: Vec::new(),
            hand: 0,
            sweep: 0,
            recovery: Recovery::new(),
        }
    }

    /// The slot position holding `key`, if any.
    #[inline]
    fn position(&self, hash: u64, key: KeyRef<'_>) -> Option<usize> {
        let slots = &self.slots;
        self.index
            .find(hash, |&pos| {
                slots
                    .get(pos as usize)
                    .is_some_and(|slot| slot.hash == hash && slot.key.as_key_ref() == key)
            })
            .map(|&pos| pos as usize)
    }

    /// The slot for `key`, if any.
    #[inline]
    fn get(&self, hash: u64, key: KeyRef<'_>) -> Option<&Slot<S>> {
        self.position(hash, key).and_then(|pos| self.slots.get(pos))
    }

    /// Appends a new key. The caller has checked it is absent and made room.
    fn insert(&mut self, slot: Slot<S>) {
        let Self { index, slots, .. } = self;
        let hash = slot.hash;
        // `make_room` keeps the slot count below `MAX_SLOTS`, so the position
        // always fits a `u32`.
        let pos = u32::try_from(slots.len()).unwrap_or(u32::MAX);
        slots.push(slot);
        let _bucket = index.insert_unique(hash, pos, |&p| {
            slots.get(p as usize).map_or(0, |slot| slot.hash)
        });
    }

    /// Removes the slot at `pos` by swap-remove, re-indexing the slot that
    /// moves into its place. O(1).
    fn remove_at(&mut self, pos: usize) -> Option<Slot<S>> {
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

    /// Evicts the slot at `pos`, first recording its debt if it has not
    /// recovered, so eviction never resets a limit.
    fn evict_at(
        &mut self,
        pos: usize,
        now: Duration,
        now_ms: u64,
        records: usize,
        out: &mut Reclaimed,
    ) {
        let Some(slot) = self.remove_at(pos) else {
            return;
        };
        if let Some(debt) = slot.state.debt(now) {
            out.remembered += 1;
            if self.recovery.remember(slot.hash, debt, now_ms, records) {
                out.merged += 1;
            }
        }
    }

    /// Reclaims idle-expired entries among the next `budget` slots from the
    /// sweep cursor.
    fn sweep_idle(
        &mut self,
        now: Duration,
        now_ms: u64,
        ttl_ms: u64,
        records: usize,
        out: &mut Reclaimed,
    ) {
        for _ in 0..TTL_SWEEP {
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
                .is_some_and(|slot| slot.is_expired(now_ms, Some(ttl_ms)));
            if expired {
                // The swap moved an unexamined slot here; look at it next.
                self.evict_at(self.sweep, now, now_ms, records, out);
                out.expired += 1;
            } else {
                self.sweep += 1;
            }
        }
    }

    /// Evicts one entry by CLOCK, preferring one that has recovered: see the
    /// module docs.
    fn evict_one(
        &mut self,
        now: Duration,
        now_ms: u64,
        ttl_ms: Option<u64>,
        records: usize,
        out: &mut Reclaimed,
    ) {
        let len = self.slots.len();
        if len == 0 {
            return;
        }
        let mut recovered = None;
        let mut unreferenced = None;
        for _ in 0..CLOCK_SCAN.min(len) {
            if self.hand >= len {
                self.hand = 0;
            }
            if let Some(slot) = self.slots.get(self.hand) {
                if slot.is_expired(now_ms, ttl_ms) || !slot.referenced.load(Ordering::Relaxed) {
                    if slot.state.is_full(now) {
                        recovered = Some(self.hand);
                        break;
                    }
                    let _first = unreferenced.get_or_insert(self.hand);
                } else {
                    // Second chance: clear the bit and move on.
                    slot.referenced.store(false, Ordering::Relaxed);
                }
            }
            self.hand += 1;
        }
        let under_hand = if self.hand >= len { 0 } else { self.hand };
        let pos = recovered.or(unreferenced).unwrap_or(under_hand);
        self.evict_at(pos, now, now_ms, records, out);
        out.evicted += 1;
        // The swap moved the newest slot into `pos`; step past it so a key
        // that just arrived gets a full revolution before the hand returns.
        self.hand = pos + 1;
    }
}

/// One shard: an independently locked slice of the key space, on its own cache
/// lines so neighbouring shards never falsely share.
#[repr(align(128))]
struct Shard<S> {
    map: RwLock<ShardMap<S>>,
    /// This shard's counters, beside the lock word every check already
    /// touches. Absent without the `metrics` feature.
    #[cfg(feature = "metrics")]
    counters: Counters,
}

// The metrics hooks. Without the `metrics` feature they are empty and inline
// away, so the counters cost nothing when off.
impl<S: KeyState> Shard<S> {
    fn new() -> Self {
        Self {
            map: RwLock::new(ShardMap::new()),
            #[cfg(feature = "metrics")]
            counters: Counters::new(),
        }
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self))]
    #[inline]
    fn on_decision(&self, decision: Decision) -> Decision {
        #[cfg(feature = "metrics")]
        self.counters.decision(decision.is_allow());
        decision
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self, unused_variables))]
    #[inline]
    fn on_inserted(&self, restored: bool) {
        #[cfg(feature = "metrics")]
        {
            self.counters.inserted();
            if restored {
                self.counters.restored();
            }
        }
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self, unused_variables))]
    #[inline]
    fn on_reclaimed(&self, reclaimed: &Reclaimed) {
        #[cfg(feature = "metrics")]
        {
            self.counters.evicted(reclaimed.evicted);
            self.counters.expired(reclaimed.expired);
            for _ in 0..reclaimed.remembered {
                self.counters.remembered();
            }
            for _ in 0..reclaimed.merged {
                self.counters.merged();
            }
        }
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self, unused_variables))]
    #[inline]
    fn on_removed(&self, count: usize) {
        #[cfg(feature = "metrics")]
        self.counters.removed(count);
    }
}

/// What the store knows about a key, for [`Store::peek`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Known {
    /// The key is tracked; here is its allowance.
    Tracked(Snapshot),
    /// The key is not tracked, but was evicted before it recovered and still
    /// owes this debt.
    Owed(Debt),
    /// The store has no state for the key: it has its full allowance.
    Unknown,
}

/// The sharded per-key store, holding one `S` per key.
pub(crate) struct Store<S> {
    shards: Box<[Shard<S>]>,
    /// `shard_count - 1`; `shard_count` is always a power of two so this masks
    /// the shard bits of a hash down to an index without a division.
    shard_mask: u64,
    /// One random-seeded hasher for the whole store (collision-attack
    /// resistant); its single hash drives the shard, the table, and the
    /// recovery records.
    hasher: RandomState,
    /// Maximum live keys per shard (at most [`MAX_SLOTS`]).
    per_shard_cap: usize,
    /// Effective idle expiry in milliseconds, or `None` to never expire.
    idle_ttl_ms: Option<u64>,
    /// Records in a shard's recovery table once it is allocated.
    records_per_shard: usize,
}

impl<S: KeyState> Store<S> {
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
        let idle_ttl_ms = eviction
            .idle_ttl()
            .map(|ttl| ceil_millis(ttl.max(min_idle)).saturating_add(1));
        let records_per_shard = per_shard_cap
            .div_ceil(2)
            .clamp(MIN_RECORDS, MAX_RECORDS)
            .next_power_of_two();

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
            records_per_shard,
        }
    }

    /// Checks `n` units against `key` as of elapsed time `now`, creating the
    /// key's state with `make_state` if this is the first time the key is seen.
    /// A key whose `make_state` gives `None` (a quota that admits nothing) is
    /// denied and never stored.
    #[inline]
    pub(crate) fn check(
        &self,
        key: KeyRef<'_>,
        n: u32,
        now: Duration,
        make_state: impl FnOnce() -> Option<S>,
    ) -> Decision {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        let track_idle = self.idle_ttl_ms.is_some();
        let now_ms = if track_idle { millis(now) } else { 0 };

        // Fast path: a shared read lock is enough for an existing key. The
        // algorithm does its own accounting, so concurrent checks — of this key
        // or any other key in the shard — proceed without serialising.
        {
            let map = sync::read(&shard.map);
            if let Some(slot) = map.get(hash, key) {
                slot.touch(now_ms, track_idle);
                return shard.on_decision(slot.state.acquire(n, now));
            }
        }

        // Slow path: first-seen key. Take the write lock, re-check (another
        // thread may have inserted it in the gap), make room, insert.
        let now_ms = millis(now);
        let mut map = sync::write(&shard.map);
        if let Some(slot) = map.get(hash, key) {
            slot.touch(now_ms, track_idle);
            return shard.on_decision(slot.state.acquire(n, now));
        }
        let Some(state) = make_state() else {
            return shard.on_decision(closed(n));
        };
        self.make_room(shard, &mut map, now, now_ms);
        // A key evicted before it recovered resumes no more permissive than it
        // left.
        let owed = map.recovery.take(hash, now_ms);
        if let Some(debt) = owed {
            state.restore(debt, now);
        }
        let outcome = state.acquire(n, now);
        map.insert(Slot::new(hash, key.to_key(), state, now_ms));
        shard.on_inserted(owed.is_some());
        shard.on_decision(outcome)
    }

    /// Makes room in a shard about to receive a new key: an incremental idle
    /// sweep, then a CLOCK eviction if the shard is full. Bounded work.
    fn make_room(&self, shard: &Shard<S>, map: &mut ShardMap<S>, now: Duration, now_ms: u64) {
        let mut reclaimed = Reclaimed::default();
        if let Some(ttl_ms) = self.idle_ttl_ms {
            map.sweep_idle(now, now_ms, ttl_ms, self.records_per_shard, &mut reclaimed);
        }
        if map.slots.len() >= self.per_shard_cap {
            map.evict_one(
                now,
                now_ms,
                self.idle_ttl_ms,
                self.records_per_shard,
                &mut reclaimed,
            );
        }
        shard.on_reclaimed(&reclaimed);
    }

    /// What the store knows about `key` as of `now`. Takes only the shard read
    /// lock, consumes nothing, and leaves the key's eviction recency untouched.
    pub(crate) fn peek(&self, key: KeyRef<'_>, now: Duration) -> Known {
        let hash = self.hasher.hash_one(key);
        let map = sync::read(&self.shard(hash).map);
        if let Some(slot) = map.get(hash, key) {
            return Known::Tracked(slot.state.peek(now));
        }
        map.recovery
            .peek(hash, millis(now))
            .map_or(Known::Unknown, Known::Owed)
    }

    /// Restores `key`'s full allowance as of `now`: a tracked key's state is
    /// reset, and any debt recorded for it is forgotten. Returns whether the
    /// key had either.
    pub(crate) fn reset(&self, key: KeyRef<'_>, now: Duration) -> bool {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        {
            let map = sync::read(&shard.map);
            if let Some(slot) = map.get(hash, key) {
                slot.state.reset(now);
                return true;
            }
        }
        sync::write(&shard.map).recovery.forget(hash)
    }

    /// Forgets `key` entirely, with any debt recorded for it. Returns whether
    /// the key had either.
    pub(crate) fn remove(&self, key: KeyRef<'_>) -> bool {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        let mut map = sync::write(&shard.map);
        let removed = map
            .position(hash, key)
            .and_then(|pos| map.remove_at(pos))
            .is_some();
        let forgotten = map.recovery.forget(hash);
        shard.on_removed(usize::from(removed));
        removed || forgotten
    }

    /// Forgets every key and every recorded debt, and releases the shards'
    /// memory.
    pub(crate) fn clear(&self) {
        for shard in &self.shards {
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
    /// whole-shard pass. A shard whose recovery table holds no live record any
    /// more releases it.
    pub(crate) fn retain_recent(&self, now: Duration) -> usize {
        let now_ms = millis(now);
        let mut removed = 0;
        for shard in &self.shards {
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
                        .is_some_and(|slot| slot.state.is_full(now));
                    if full && map.remove_at(cursor).is_some() {
                        chunk_removed += 1;
                    }
                }
                shard.on_removed(chunk_removed);
                removed += chunk_removed;
                if cursor == 0 {
                    if !map.recovery.records.iter().any(|r| r.is_live(now_ms)) {
                        map.recovery = Recovery::new();
                    }
                    break;
                }
            }
        }
        removed
    }

    /// Releases memory the shards hold beyond their current keys.
    pub(crate) fn shrink_to_fit(&self) {
        for shard in &self.shards {
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

    /// Heap bytes held by the recovery tables (zero until a shard first evicts
    /// an unrecovered key).
    #[cfg(test)]
    fn recovery_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| sync::read(&shard.map).recovery.heap_bytes())
            .sum()
    }

    /// The counters summed across every shard.
    #[cfg(feature = "metrics")]
    pub(crate) fn metrics(&self) -> Metrics {
        let mut metrics = Metrics::default();
        for shard in &self.shards {
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
    fn shard(&self, hash: u64) -> &Shard<S> {
        let index = ((hash >> SHARD_SHIFT) & self.shard_mask) as usize;
        // `index <= shard_mask < shards.len()`.
        &self.shards[index]
    }
}

/// The keyed store for whichever algorithm a limiter runs: one typed
/// [`Store`] per algorithm, chosen once at construction, so the per-key state
/// carries no enum tag and the dispatch is one predictable branch per call.
pub(crate) enum Keyed {
    TokenBucket(Store<BucketCore>),
    #[cfg(feature = "algorithms")]
    LeakyBucket(Store<Gcra>),
    #[cfg(feature = "algorithms")]
    FixedWindow(Store<FixedWindow>),
    #[cfg(feature = "algorithms")]
    SlidingLog(Store<SlidingLog>),
    #[cfg(feature = "algorithms")]
    SlidingCounter(Store<SlidingCounter>),
}

/// Runs `$body` with `$store` bound to the typed store inside a [`Keyed`], and
/// `$state` naming its per-key state type.
macro_rules! keyed {
    ($keyed:expr, $store:ident: $state:ident => $body:expr) => {
        match $keyed {
            Keyed::TokenBucket($store) => {
                #[allow(dead_code)]
                type $state = BucketCore;
                $body
            }
            #[cfg(feature = "algorithms")]
            Keyed::LeakyBucket($store) => {
                #[allow(dead_code)]
                type $state = Gcra;
                $body
            }
            #[cfg(feature = "algorithms")]
            Keyed::FixedWindow($store) => {
                #[allow(dead_code)]
                type $state = FixedWindow;
                $body
            }
            #[cfg(feature = "algorithms")]
            Keyed::SlidingLog($store) => {
                #[allow(dead_code)]
                type $state = SlidingLog;
                $body
            }
            #[cfg(feature = "algorithms")]
            Keyed::SlidingCounter($store) => {
                #[allow(dead_code)]
                type $state = SlidingCounter;
                $body
            }
        }
    };
}

impl Keyed {
    /// A store for `algorithm`; see [`Store::new`].
    pub(crate) fn new(
        algorithm: Algorithm,
        shards: usize,
        eviction: Eviction,
        min_idle: Duration,
    ) -> Self {
        match algorithm {
            Algorithm::TokenBucket => Self::TokenBucket(Store::new(shards, eviction, min_idle)),
            #[cfg(feature = "algorithms")]
            Algorithm::LeakyBucket => Self::LeakyBucket(Store::new(shards, eviction, min_idle)),
            #[cfg(feature = "algorithms")]
            Algorithm::FixedWindow => Self::FixedWindow(Store::new(shards, eviction, min_idle)),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowLog => Self::SlidingLog(Store::new(shards, eviction, min_idle)),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowCounter => {
                Self::SlidingCounter(Store::new(shards, eviction, min_idle))
            }
        }
    }

    /// See [`Store::check`]; a new key's state is created under `quota()`.
    #[inline]
    pub(crate) fn check(
        &self,
        key: KeyRef<'_>,
        n: u32,
        now: Duration,
        quota: impl FnOnce() -> Quota,
    ) -> Decision {
        keyed!(self, store: State => store.check(key, n, now, || State::create(&quota(), now)))
    }

    /// See [`Store::peek`].
    pub(crate) fn peek(&self, key: KeyRef<'_>, now: Duration) -> Known {
        keyed!(self, store: State => store.peek(key, now))
    }

    /// The allowance of an untracked key that owes `debt`: a fresh state under
    /// `quota` with the debt restored.
    pub(crate) fn owed_snapshot(&self, debt: Debt, quota: &Quota, now: Duration) -> Snapshot {
        keyed!(self, _store: State => State::create(quota, now).map_or_else(Snapshot::closed, |state| {
            state.restore(debt, now);
            state.peek(now)
        }))
    }

    /// See [`Store::reset`].
    pub(crate) fn reset(&self, key: KeyRef<'_>, now: Duration) -> bool {
        keyed!(self, store: State => store.reset(key, now))
    }

    /// See [`Store::remove`].
    pub(crate) fn remove(&self, key: KeyRef<'_>) -> bool {
        keyed!(self, store: State => store.remove(key))
    }

    /// See [`Store::clear`].
    pub(crate) fn clear(&self) {
        keyed!(self, store: State => store.clear());
    }

    /// See [`Store::retain_recent`].
    pub(crate) fn retain_recent(&self, now: Duration) -> usize {
        keyed!(self, store: State => store.retain_recent(now))
    }

    /// See [`Store::shrink_to_fit`].
    pub(crate) fn shrink_to_fit(&self) {
        keyed!(self, store: State => store.shrink_to_fit());
    }

    /// See [`Store::len`].
    pub(crate) fn len(&self) -> usize {
        keyed!(self, store: State => store.len())
    }

    /// See [`Store::metrics`].
    #[cfg(feature = "metrics")]
    pub(crate) fn metrics(&self) -> Metrics {
        keyed!(self, store: State => store.metrics())
    }

    /// See [`Store::shard_count`].
    pub(crate) fn shard_count(&self) -> usize {
        keyed!(self, store: State => store.shard_count())
    }
}

/// Whole milliseconds in `now`, saturating.
#[inline]
fn millis(now: Duration) -> u64 {
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

/// Milliseconds in `duration`, rounded up and saturating.
fn ceil_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos().div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use core::time::Duration;
    use std::time::Instant;

    use better_bucket::BucketCore;

    use super::{CLOCK_SCAN, Known, MAX_RECORDS, MIN_RECORDS, Store, TTL_SWEEP};
    use crate::algo::KeyState;
    use crate::eviction::Eviction;
    use crate::key::Key;
    use crate::quota::Quota;

    fn make_store(shards: usize, eviction: Eviction) -> Store<BucketCore> {
        Store::new(shards, eviction, Duration::ZERO)
    }

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// One check of `key` for `n` units at `ms`, creating token-bucket state
    /// under `quota` if the key is new.
    fn check(
        store: &Store<BucketCore>,
        key: impl Into<Key>,
        n: u32,
        ms: u64,
        quota: Quota,
    ) -> bool {
        let key = key.into();
        store
            .check(key.as_key_ref(), n, at(ms), || {
                BucketCore::create(&quota, at(ms))
            })
            .is_allow()
    }

    fn peek(store: &Store<BucketCore>, key: impl Into<Key>, ms: u64) -> Known {
        store.peek(key.into().as_key_ref(), at(ms))
    }

    fn tracked(store: &Store<BucketCore>, key: impl Into<Key>) -> bool {
        matches!(peek(store, key, 0), Known::Tracked(_))
    }

    /// A rate so slow nothing refills during a test.
    fn slow(limit: u32) -> Quota {
        Quota::from_parts(limit, Duration::from_secs(3_600), limit)
    }

    impl<S: KeyState> Store<S> {
        /// Every slot is indexed exactly once, at its own position.
        fn assert_consistent(&self) {
            for shard in &self.shards {
                let map = super::sync::read(&shard.map);
                assert_eq!(map.index.len(), map.slots.len());
                for (pos, slot) in map.slots.iter().enumerate() {
                    assert_eq!(map.position(slot.hash, slot.key.as_key_ref()), Some(pos));
                }
            }
        }

        fn merged_records(&self, now_ms: u64) -> usize {
            self.shards
                .iter()
                .map(|shard| {
                    let map = super::sync::read(&shard.map);
                    map.recovery
                        .records
                        .iter()
                        .filter(|r| r.tag == super::MERGED && r.is_live(now_ms))
                        .count()
                })
                .sum()
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
        assert!(check(&store, "a", 1, 0, Quota::per_second(10)));
        assert_eq!(store.len(), 1);
        assert!(check(&store, "a", 1, 0, Quota::per_second(10)));
        assert_eq!(store.len(), 1);
        store.assert_consistent();
    }

    #[test]
    fn test_long_and_short_keys_find_the_same_state() {
        let store = make_store(2, Eviction::unbounded());
        let long = "an-api-token-much-longer-than-the-inline-capacity";
        assert!(check(&store, long, 3, 0, Quota::per_second(3)));
        // A borrowed lookup by the same bytes finds the stored key.
        let decision = store.check(crate::key::KeyRef::raw(long.as_bytes()), 1, at(0), || None);
        assert!(decision.is_deny());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn test_a_key_that_admits_nothing_is_never_stored() {
        let store = make_store(1, Eviction::unbounded());
        for k in 0..100u64 {
            assert!(!check(&store, k, 1, 0, Quota::per_second(0)));
            assert!(check(&store, k, 0, 0, Quota::per_second(0)));
        }
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn test_a_token_bucket_key_costs_two_cache_lines() {
        assert_eq!(core::mem::size_of::<super::Slot<BucketCore>>(), 128);
    }

    #[test]
    fn test_capacity_bounds_total_keys_under_unique_flood() {
        let shards = 8;
        let cap = 100usize;
        let store = make_store(shards, Eviction::capacity(cap));
        for k in 0..10_000u64 {
            let _ = check(&store, k, 1, k, Quota::per_second(10));
        }
        let bound = cap.div_ceil(shards).max(1) * shards;
        assert!(store.len() <= bound, "flood grew to {}", store.len());
        store.assert_consistent();
    }

    #[test]
    fn test_capacity_boundaries_per_shard() {
        // 0 is treated as 1 per shard; cap-1, cap, cap+1 on one shard.
        for (cap, expect) in [(0usize, 1usize), (1, 1), (4, 4), (5, 5)] {
            let store = make_store(1, Eviction::capacity(cap));
            for k in 0..(cap as u64 + 10) {
                let _ = check(&store, k, 1, 0, Quota::per_second(10));
            }
            assert_eq!(store.len(), expect, "cap {cap}");
            store.assert_consistent();
        }
    }

    #[test]
    fn test_ttl_reclaims_idle_keys_on_later_insert() {
        let store = make_store(1, Eviction::new(1_000, Duration::from_millis(1000)));
        let _ = check(&store, "idle", 1, 0, Quota::per_second(10));
        assert_eq!(store.len(), 1);
        let _ = check(&store, "fresh", 1, 2_000, Quota::per_second(10));
        assert_eq!(store.len(), 1, "the idle key should have been reclaimed");
        store.assert_consistent();
    }

    #[test]
    fn test_ttl_sweep_is_incremental_and_eventually_complete() {
        let store = make_store(1, Eviction::new(1_000, Duration::from_millis(1000)));
        for k in 0..100u64 {
            let _ = check(&store, k, 1, 0, Quota::per_second(10));
        }
        // One late insert examines at most TTL_SWEEP slots, not all 100.
        let _ = check(&store, "late-0", 1, 5_000, Quota::per_second(10));
        assert_eq!(store.len(), 100 - TTL_SWEEP + 1);
        // Enough further inserts drain every idle key.
        for k in 1..=100u64 {
            let _ = check(&store, format!("late-{k}"), 1, 5_000, Quota::per_second(10));
        }
        assert_eq!(store.len(), 101);
        store.assert_consistent();
    }

    #[test]
    fn test_short_ttl_is_raised_to_min_idle() {
        let store: Store<BucketCore> = Store::new(
            1,
            Eviction::idle(Duration::from_millis(10)),
            Duration::from_secs(1),
        );
        // Rounded up and padded by a millisecond for the floored stamps.
        assert_eq!(store.idle_ttl_ms, Some(1_001));
    }

    #[test]
    fn test_idle_ttl_never_reclaims_a_key_before_the_full_ttl() {
        // A key last seen at 0.999ms and idle for 999.001ms has floored stamps
        // 0 and 1000, a measured gap of the whole 1s TTL, although it has not
        // been idle that long.
        let store = make_store(1, Eviction::new(1_000, Duration::from_secs(1)));
        let quota = Quota::per_second(1_000);
        let key = Key::from("k");
        let _ = store.check(key.as_key_ref(), 1_000, Duration::from_micros(999), || {
            BucketCore::create(&quota, Duration::from_micros(999))
        });
        let _ = check(&store, "other", 1, 1_000, quota);
        assert!(tracked(&store, key.clone()), "reclaimed early");
        // Once truly idle for longer than the TTL, it goes.
        let _ = check(&store, "later", 1, 1_002, quota);
        assert!(!tracked(&store, key));
    }

    #[test]
    fn test_recently_seen_key_survives_eviction_pressure() {
        let store = make_store(1, Eviction::capacity(4));
        let quota = Quota::per_second(1_000);
        let mut now = 0u64;
        for round in 0..50u64 {
            now += 1;
            assert!(check(&store, "hot", 1, now, quota));
            now += 1;
            let _ = check(&store, round, 1, now - 1, quota);
        }
        now += 10_000;
        assert!(check(&store, "hot", 1, now, quota));
        store.assert_consistent();
    }

    #[test]
    fn test_hot_key_state_survives_a_unique_key_flood() {
        let store = make_store(1, Eviction::capacity(64));
        for _ in 0..5 {
            assert!(check(&store, "hot", 1, 0, slow(5)));
        }
        for k in 0..10_000u64 {
            let _ = check(&store, k, 1, 0, slow(5));
            if k % 8 == 0 {
                assert!(!check(&store, "hot", 1, 0, slow(5)));
            }
        }
        assert!(!check(&store, "hot", 1, 0, slow(5)));
    }

    #[test]
    fn test_drained_key_is_not_reset_by_evicting_it() {
        // RN-005: a drained key that is *not* checked during a flood is
        // evicted, and in 1.1 it came back with a fresh allowance. It must come
        // back exactly as drained.
        let store = make_store(1, Eviction::capacity(64));
        for _ in 0..5 {
            assert!(check(&store, "victim", 1, 0, slow(5)));
        }
        assert!(!check(&store, "victim", 1, 0, slow(5)));
        for k in 0..10_000u64 {
            let _ = check(&store, k, 1, 0, slow(5));
        }
        assert!(
            !tracked(&store, "victim"),
            "the flood should have evicted it"
        );
        assert!(matches!(peek(&store, "victim", 0), Known::Owed(_)));
        assert!(
            !check(&store, "victim", 1, 0, slow(5)),
            "eviction reset the limit"
        );
        store.assert_consistent();
    }

    #[test]
    fn test_evicted_key_resumes_where_it_was() {
        // 10/s: two units spent at t=0, evicted, back at t=50ms: it holds
        // eight (the half token accrued does not round up into a ninth).
        let store = make_store(1, Eviction::capacity(1));
        let quota = Quota::per_second(10);
        assert!(check(&store, "a", 2, 0, quota));
        assert!(check(&store, "b", 1, 10, quota)); // evicts "a", owing 2
        assert!(check(&store, "a", 8, 50, quota)); // evicts "b"; "a" resumes
        assert!(!check(&store, "a", 1, 50, quota));
        // After its debt is repaid a returning key is simply fresh.
        assert!(check(&store, "b", 1, 1_000, quota));
        assert!(check(&store, "a", 10, 5_000, quota));
    }

    #[test]
    fn test_recovered_keys_are_evicted_before_limited_ones() {
        let store = make_store(1, Eviction::capacity(8));
        let quota = Quota::per_second(1);
        // Seven drained keys and one that spent nothing (recovered).
        for k in 0..7u64 {
            assert!(check(&store, k, 1, 0, quota));
        }
        assert!(check(&store, "idle", 0, 0, quota));
        // A newcomer evicts the recovered key, so nothing is owed.
        assert!(check(&store, "new", 1, 0, quota));
        assert!(!tracked(&store, "idle"));
        assert!(matches!(peek(&store, "idle", 0), Known::Unknown));
        assert_eq!(store.recovery_bytes(), 0, "no debt had to be recorded");
        for k in 0..7u64 {
            assert!(tracked(&store, k));
        }
    }

    #[test]
    fn test_recovery_table_is_lazy_and_bounded() {
        let store = make_store(1, Eviction::capacity(100));
        assert_eq!(store.recovery_bytes(), 0);
        for k in 0..10_000u64 {
            let _ = check(&store, k, 1, 0, slow(5));
        }
        let records = 64usize; // half of 100, rounded up to a power of two
        assert_eq!(store.records_per_shard, records);
        assert_eq!(store.recovery_bytes(), records * 16);
        assert_eq!(
            make_store(1, Eviction::capacity(1)).records_per_shard,
            MIN_RECORDS
        );
        assert_eq!(
            make_store(1, Eviction::unbounded()).records_per_shard,
            MAX_RECORDS
        );
    }

    #[test]
    fn test_an_overloaded_recovery_table_fails_closed() {
        // A flood of drained keys far beyond what the table holds: records
        // merge, and no key that left drained ever returns with credit.
        let store = make_store(1, Eviction::capacity(16));
        let quota = slow(3);
        // Once records merge, some newcomers start drained (fail closed), so
        // not every flood key is admitted; count the ones that were.
        let drained: Vec<u64> = (0..2_000u64)
            .filter(|&k| check(&store, k, 3, 0, quota))
            .collect();
        assert!(drained.len() >= 16, "only {} admitted", drained.len());
        assert!(
            store.merged_records(0) > 0,
            "the table should have overflowed"
        );
        for &k in &drained {
            assert!(!check(&store, k, 1, 0, quota), "key {k} was reset");
        }
        // Merged records expire with the debts they hold.
        assert!(check(&store, "after", 1, 3_600_000, quota));
        assert_eq!(store.merged_records(3_600_000), 0);
    }

    #[test]
    fn test_remove_reset_and_clear_forget_recorded_debts() {
        let store = make_store(1, Eviction::capacity(1));
        let quota = slow(2);
        assert!(check(&store, "a", 2, 0, quota));
        assert!(check(&store, "b", 2, 0, quota)); // "a" evicted, owing 2
        assert!(matches!(peek(&store, "a", 0), Known::Owed(_)));
        assert!(store.reset(Key::from("a").as_key_ref(), at(0)));
        assert!(matches!(peek(&store, "a", 0), Known::Unknown));
        assert!(!store.reset(Key::from("a").as_key_ref(), at(0)));

        assert!(check(&store, "c", 2, 0, quota)); // "b" evicted, owing 2
        assert!(store.remove(Key::from("b").as_key_ref()));
        assert!(matches!(peek(&store, "b", 0), Known::Unknown));

        assert!(check(&store, "d", 2, 0, quota)); // "c" evicted, owing 2
        store.clear();
        assert!(matches!(peek(&store, "c", 0), Known::Unknown));
        assert_eq!(store.recovery_bytes(), 0);
        assert!(check(&store, "c", 2, 0, quota));
    }

    #[test]
    fn test_clock_scan_is_bounded_when_every_key_is_hot() {
        // Every slot referenced: one eviction still examines at most
        // CLOCK_SCAN slots and makes room.
        let store = make_store(1, Eviction::capacity(CLOCK_SCAN * 4));
        for _ in 0..2 {
            for k in 0..(CLOCK_SCAN as u64 * 4) {
                let _ = check(&store, k, 1, 0, Quota::per_second(10));
            }
        }
        let _ = check(&store, "new", 1, 0, Quota::per_second(10));
        assert_eq!(store.len(), CLOCK_SCAN * 4);
        store.assert_consistent();
    }

    #[test]
    #[cfg_attr(miri, ignore = "a timing bound is meaningless under Miri")]
    fn test_insert_at_capacity_is_constant_time() {
        // Compares inserts into a full shard (each one evicts an unrecovered
        // key and records its debt) with inserts into an empty one, on the
        // same host at the same moment, so a loaded machine slows both alike.
        // A full-shard scan per eviction made the first ~1000x the second.
        let full = make_store(1, Eviction::capacity(16_384));
        let empty = make_store(1, Eviction::unbounded());
        let quota = Quota::per_second(10);
        for k in 0..16_384u64 {
            let _ = check(&full, k, 1, 0, quota);
        }
        let time = |store: &Store<BucketCore>, from: u64| {
            let start = Instant::now();
            for k in from..from + 20_000 {
                let _ = check(store, k, 1, 0, quota);
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
        let store = make_store(1, Eviction::capacity(1_000_000));
        assert_eq!(store.shard_count(), 64);
        assert!(store.per_shard_cap <= MAX_KEYS_PER_SHARD);
        assert_eq!(make_store(2, Eviction::capacity(1_000)).shard_count(), 2);
        assert_eq!(make_store(2, Eviction::unbounded()).shard_count(), 2);
        assert_eq!(
            make_store(1, Eviction::capacity(usize::MAX)).shard_count(),
            super::MAX_SHARDS
        );
    }

    #[test]
    fn test_store_refills_across_time() {
        let store = make_store(4, Eviction::unbounded());
        let quota = Quota::per_second(3);
        for _ in 0..3 {
            assert!(check(&store, "k", 1, 0, quota));
        }
        assert!(!check(&store, "k", 1, 0, quota));
        assert!(check(&store, "k", 1, 1_000, quota));
    }

    #[test]
    fn test_peek_reset_and_remove_single_keys() {
        let store = make_store(2, Eviction::unbounded());
        let quota = Quota::per_second(10);
        let key = Key::from("k");
        assert_eq!(store.peek(key.as_key_ref(), at(0)), Known::Unknown);
        assert!(!store.reset(key.as_key_ref(), at(0)));
        assert!(!store.remove(key.as_key_ref()));

        assert!(check(&store, key.clone(), 4, 0, quota));
        let remaining = |store: &Store<BucketCore>| match store.peek(key.as_key_ref(), at(0)) {
            Known::Tracked(snapshot) => Some(snapshot.remaining()),
            _ => None,
        };
        assert_eq!(remaining(&store), Some(6));
        assert!(store.reset(key.as_key_ref(), at(0)));
        assert_eq!(remaining(&store), Some(10));
        assert!(store.remove(key.as_key_ref()));
        assert_eq!(remaining(&store), None);
        assert_eq!(store.len(), 0);
        store.assert_consistent();
    }

    #[test]
    fn test_clear_and_shrink_forget_everything() {
        let store = make_store(4, Eviction::unbounded());
        for k in 0..500u64 {
            let _ = check(&store, k, 1, 0, Quota::per_second(10));
        }
        assert_eq!(store.len(), 500);
        store.shrink_to_fit();
        store.assert_consistent();
        store.clear();
        assert_eq!(store.len(), 0);
        store.assert_consistent();
        assert!(check(&store, 1u64, 10, 0, Quota::per_second(10)));
    }

    #[test]
    fn test_retain_recent_removes_only_full_keys() {
        let store = make_store(2, Eviction::unbounded());
        // 3_000 keys: spans several retain chunks per shard.
        for k in 0..3_000u64 {
            let n = u32::from(k % 3 != 0);
            let _ = check(&store, k, n, 0, slow(5));
        }
        // Every third key spent nothing, so it is full and forgettable.
        assert_eq!(store.retain_recent(at(0)), 1_000);
        assert_eq!(store.len(), 2_000);
        store.assert_consistent();
        for k in 0..3_000u64 {
            assert_eq!(tracked(&store, k), k % 3 != 0, "key {k}");
        }
        assert_eq!(store.retain_recent(at(0)), 0);
    }

    #[test]
    fn test_retain_recent_releases_a_spent_recovery_table() {
        let store = make_store(1, Eviction::capacity(4));
        for k in 0..100u64 {
            let _ = check(&store, k, 1, 0, Quota::per_second(10));
        }
        assert!(store.recovery_bytes() > 0);
        // A second later every debt is repaid: the table is dropped.
        let _ = store.retain_recent(at(1_000));
        assert_eq!(store.recovery_bytes(), 0);
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
            let mut now = 0u64;
            for (key, advance) in ops {
                now += advance;
                let _ = check(&store, key, 1, now, Quota::per_second(3));
                proptest::prop_assert!(store.len() <= cap.div_ceil(2).max(1) * 2);
            }
            store.assert_consistent();
        }

        /// Eviction never hands a key more allowance than its quota: under
        /// any amount of eviction pressure, the units admitted for one key in
        /// any interval `[a, b]` stay within `burst + rate * (b - a)` — the
        /// token bucket's own bound, which an eviction that reset a key would
        /// break.
        #[test]
        fn prop_eviction_never_resets_a_limit(
            cap in 1usize..12,
            ops in proptest::collection::vec((0u64..40, 0u64..120, 1u32..4), 1..400),
        ) {
            let store = make_store(1, Eviction::capacity(cap));
            // 5 per second, burst 4.
            let quota = Quota::per_second(5).with_burst(4);
            let mut admitted: std::collections::HashMap<u64, Vec<(u64, u64)>> =
                std::collections::HashMap::new();
            let mut now = 0u64;
            for (key, advance, n) in ops {
                now += advance;
                if check(&store, key, n, now, quota) {
                    admitted.entry(key).or_default().push((now, u64::from(n)));
                }
            }
            for (key, events) in &admitted {
                for (i, &(a, _)) in events.iter().enumerate() {
                    let mut units = 0u64;
                    for &(b, n) in &events[i..] {
                        units += n;
                        // units <= 4 + 5 * (b - a) / 1000, scaled by 1000.
                        proptest::prop_assert!(
                            units * 1_000 <= 4_000 + 5 * (b - a),
                            "key {} admitted {} units in [{}ms, {}ms]", key, units, a, b
                        );
                    }
                }
            }
        }
    }
}
