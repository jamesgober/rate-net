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
//! all have been back. When a key is inserted the table is consulted, and a key
//! that returns before its debt is repaid resumes no more permissive than it
//! left (see [`KeyState::restore`]); its own record is then cleared. Records
//! expire at their deadline, and a shard's table is allocated on first use.
//!
//! - **Exact records.** Each debt is a 16-byte [`Record`] tagged with 32 bits
//!   of its key's hash, in a cuckoo-hashed table of 64-byte buckets holding
//!   [`EXACT_PER_KEY`] records per key of shard capacity. A key has two
//!   candidate buckets; when both are full, records are moved to their other
//!   bucket (at most [`MAX_KICKS`] moves), which fills the table past 90 %
//!   before anything spills; after a walk fails, the next [`WALK_BACKOFF`]
//!   debts spill without one, so a sustained flood stays cheap. A key reads
//!   back only its own record.
//! - **Past the table: [`Overflow`].** Under [`Overflow::Forget`] (the
//!   default) a debt the table cannot place is dropped — its key may return
//!   with a fresh allowance — and nothing below applies: the sketch is never
//!   written. Under [`Overflow::FailClosed`]:
//! - **Overflow sketch.** A debt the table cannot place goes into two cells of
//!   a shared sketch ([`CELLS_PER_KEY`] cells per key), each keeping the larger
//!   deadline and spend. A key being stored is charged the *smaller* of its two
//!   cells, and only while both are live: never less than its own overflowed
//!   debt, but an unrelated key whose two cells were both written by others is
//!   charged too (it starts partly or wholly drained — fail closed).
//! - **No spreading.** A key charged from the sketch carries debt that may be
//!   others'. When such a key is evicted its debt goes only into the exact
//!   table, marked so that it is never moved back into the sketch: a sketch
//!   charge cannot be written back, spread to further keys, and ratchet up.
//!   If the key's buckets hold nothing but such marked debts, the key is not
//!   evicted, and if a full shard has nothing else to evict the newcomer is
//!   refused with a retry time (fail closed, `Metrics::keys_refused`).
//!
//! So the guarantee is exact — no reset, no false denials, no saturation —
//! while the unrecovered evicted keys of a shard fit its exact table (about
//! twice the cap). Past that, `Forget` resets the keys whose debts it drops,
//! and `FailClosed` never gives an evicted key back more than it had but can
//! charge keys that never owed anything, at rates `docs/BENCHMARKS.md`
//! measures.
//!
//! The eviction sweep judges a candidate by a cached "recovers at" hint
//! ([`Slot::recovers_at_ms`]) only while the slot's reference bit is clear:
//! a set bit means a check may have spent since, and the state is read again
//! — including for idle-expired slots, which are taken whatever their bit
//! says.
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
use crate::eviction::{Eviction, Overflow};
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

/// Records per bucket of the exact table: one 64-byte cache line.
const BUCKET_RECORDS: usize = 4;

/// Exact records per key of shard capacity. Debts are 16 bytes against a key's
/// ~128, so the table holds twice as many evicted debts as the shard holds
/// keys (`tests/eviction_liveness.rs` measures what that buys).
const EXACT_PER_KEY: usize = 2;

/// The most records one insertion into the exact table may relocate (cuckoo
/// moves) before the last displaced debt goes to the overflow sketch. With two
/// four-record buckets per key this fills the table past 90 % before anything
/// overflows.
const MAX_KICKS: usize = 16;

/// After a cuckoo walk fails — the table is full of live debts — this many
/// following debts skip the walk and spill to the overflow sketch at once. A
/// walk through a full table would almost surely fail again, and costs a cache
/// miss per move under the shard write lock; backing off keeps a sustained
/// flood's evictions cheap, while a table with room is walked as usual.
const WALK_BACKOFF: u8 = 32;

/// The bit of [`Record::until_ms`] that marks a debt charged (in part) from
/// the shared overflow sketch: see [`Recovery::record`].
const INHERITED: u64 = 1 << 63;

/// The fewest and most buckets a shard's exact table holds (4 records each).
const MIN_BUCKETS: usize = 2;
const MAX_BUCKETS: usize = 1 << 13;

/// Overflow-sketch cells per key of shard capacity. A newcomer is charged from
/// the sketch only when both of its cells are live, so the sparser the sketch
/// the fewer keys that never owed anything start short; at four 8-byte cells
/// per key, a sketch holding a cap's worth of live overflowed debts charges
/// about one newcomer in six, and one holding a quarter of that about one in
/// seventy (`(1 - e^(-2n/m))^2` for `n` debts in `m` cells).
const CELLS_PER_KEY: usize = 4;

/// The fewest and most cells a shard's overflow sketch holds.
const MIN_CELLS: usize = 8;
const MAX_CELLS: usize = 1 << 16;

/// Bits of a sketch cell holding the spent units; the rest hold the deadline.
const SPENT_BITS: u32 = 24;
const SPENT_MAX: u64 = (1 << SPENT_BITS) - 1;
const UNTIL_MAX: u64 = u64::MAX >> SPENT_BITS;

/// Odd 64-bit constants (the golden ratio, and a second from `splitmix64`)
/// used to remix a key's hash for the recovery table, so its bucket, cell, and
/// tag bits are independent of the bits that chose the shard and the index
/// bucket, and of each other.
const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX2: u64 = 0xBF58_476D_1CE4_E5B9;

/// One stored key: the algorithm's state, the eviction marks, the key's hash
/// (kept so a moved slot can be re-indexed without rehashing), and the key.
///
/// One flat struct, so the marks, hash, and key share the cache lines the
/// state's alignment already pads out (a token-bucket key is 128 bytes).
struct Slot<S> {
    state: S,
    /// CLOCK reference bit: set on every check, cleared by the sweeping hand.
    referenced: AtomicBool,
    /// Whether the key's state was charged a debt from the shared overflow
    /// sketch when it was stored. Such a key is never itself written back to
    /// the sketch (see the module docs), so set and read only under the shard
    /// write lock.
    seeded: bool,
    /// Milliseconds since the limiter epoch at the last check. Maintained only
    /// when an idle TTL is configured, so the common path skips it.
    last_seen_ms: AtomicU64,
    /// When the key's allowance is whole again — milliseconds since the epoch,
    /// rounded up — as of the last time the store read its state, or
    /// [`UNKNOWN`] once a check may have changed it since. The eviction sweep
    /// reads this word, beside the reference bit, instead of the state's own
    /// cache line. It is computed when the key is stored, invalidated by the
    /// hand whenever it clears the reference bit (every later check sets that
    /// bit again), and refreshed when the hand next has to read the state.
    /// Only ever later than the truth, never earlier — a key that is reset
    /// recovers sooner than its hint says — so trusting it never evicts a key
    /// that still owes. Read and written only under the shard write lock.
    recovers_at_ms: u64,
    hash: u64,
    key: Key,
}

/// A [`Slot::recovers_at_ms`] that must be recomputed from the state.
const UNKNOWN: u64 = u64::MAX;

impl<S: KeyState> Slot<S> {
    fn new(hash: u64, key: Key, state: S, seeded: bool, now: Duration, now_ms: u64) -> Self {
        let recovers_at_ms = recovers_at(&state, now);
        Self {
            state,
            // A new key starts unreferenced: a key seen once (a flood of
            // unique keys) is the first to go, while a key checked again
            // earns a second chance.
            referenced: AtomicBool::new(false),
            seeded,
            last_seen_ms: AtomicU64::new(now_ms),
            recovers_at_ms,
            hash,
            key,
        }
    }

    /// When the key's allowance is whole again: the hint if it is still valid,
    /// otherwise read from the state (and remembered).
    ///
    /// The hint is valid only while the reference bit is clear: every check
    /// sets the bit before it touches the state, so a set bit means the key
    /// may have spent since the hint was computed. The CLOCK hand clears the
    /// bit (and the hint) on its way past, but an idle-expired slot is taken
    /// whatever its bit says — so a set bit always forces a fresh read here.
    fn recovers_at(&mut self, now: Duration) -> u64 {
        if self.recovers_at_ms == UNKNOWN || self.referenced.load(Ordering::Relaxed) {
            self.recovers_at_ms = recovers_at(&self.state, now);
        }
        self.recovers_at_ms
    }

    /// Marks the entry used for CLOCK. Written only when it changes, so a hot
    /// key's cache line is not dirtied on every check.
    #[inline]
    fn touch(&self) {
        if !self.referenced.load(Ordering::Relaxed) {
            self.referenced.store(true, Ordering::Relaxed);
        }
    }

    /// Records a check at `now_ms` for the idle TTL, only when it moves the
    /// stamp forward.
    #[inline]
    fn touch_idle(&self, now_ms: u64) {
        if self.last_seen_ms.load(Ordering::Relaxed) < now_ms {
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

/// When `state` has its whole allowance again, in milliseconds since the epoch
/// rounded up; `0` if it already has.
fn recovers_at<S: KeyState>(state: &S, now: Duration) -> u64 {
    state.recovers_at(now).map_or(0, ceil_millis)
}

/// What one evicted, unrecovered key owed, in the exact table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Record {
    /// A 32-bit tag of the key's hash; `0` marks an empty record.
    tag: u32,
    /// Units the key had spent and not yet recovered.
    spent: u32,
    /// When they would all have been back, in whole milliseconds since the
    /// limiter epoch (rounded up), with the [`INHERITED`] bit on top. A record
    /// is live while the deadline is in the future.
    until_ms: u64,
}

impl Record {
    const fn until(&self) -> u64 {
        self.until_ms & !INHERITED
    }

    const fn is_live(&self, now_ms: u64) -> bool {
        self.tag != 0 && self.until() > now_ms
    }

    /// Whether the debt was charged (in part) from the overflow sketch, so it
    /// must never be moved back into it.
    const fn is_inherited(&self) -> bool {
        self.until_ms & INHERITED != 0
    }

    const fn debt(&self) -> Debt {
        Debt {
            spent: self.spent,
            until: Duration::from_millis(self.until()),
        }
    }
}

/// Four records: one cache line, so a key's two candidate buckets cost two.
#[derive(Debug, Clone, Copy, Default)]
#[repr(align(64))]
struct Bucket([Record; BUCKET_RECORDS]);

/// A cell of the overflow sketch: a deadline (milliseconds since the epoch,
/// rounded up) and a spent count, packed into one word; `0` is empty.
fn pack(until_ms: u64, spent: u32) -> u64 {
    (until_ms.min(UNTIL_MAX) << SPENT_BITS) | u64::from(spent).min(SPENT_MAX)
}

/// The deadline and spent count of a sketch cell. A spent count that filled
/// the field ([`SPENT_MAX`] or more units) reads back as `u32::MAX` — all of
/// the key's allowance, clamped to its limit when restored — so a limit
/// larger than the field is never under-charged.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the spent field is masked to 24 bits"
)]
const fn unpack(cell: u64) -> (u64, u32) {
    let spent = cell & SPENT_MAX;
    let spent = if spent == SPENT_MAX {
        u32::MAX
    } else {
        spent as u32
    };
    (cell >> SPENT_BITS, spent)
}

/// Where a key's records and cells live in a recovery table, and its tag.
#[derive(Clone, Copy)]
struct Probe {
    buckets: [usize; 2],
    cells: [usize; 2],
    tag: u32,
}

/// `len - 1` bits of `word` from bit `shift` up (`len` a power of two).
fn pick(word: u64, shift: u32, len: usize) -> usize {
    let mask = u64::try_from(len - 1).unwrap_or(u64::MAX);
    usize::try_from((word >> shift) & mask).unwrap_or(0)
}

/// A record's other candidate bucket, from its bucket and tag alone, so a
/// record can be moved there without its key's hash (partial-key cuckoo
/// hashing). An involution — `alt(alt(b)) == b` — and never `b` itself.
fn alt_bucket(bucket: usize, tag: u32, buckets: usize) -> usize {
    let offset = pick(u64::from(tag).wrapping_mul(MIX2), 32, buckets).max(1);
    bucket ^ offset
}

/// A key's two overflow cells, from its tag and the lower of its two buckets —
/// both recoverable from a record — so a record displaced from the exact
/// table lands in the same cells its key reads.
fn cells_of(tag: u32, low_bucket: usize, cells: usize) -> [usize; 2] {
    let low = u64::try_from(low_bucket).unwrap_or(u64::MAX);
    let word = ((low << 32) | u64::from(tag)).wrapping_mul(MIX);
    let word = (word ^ (word >> 31)).wrapping_mul(MIX2);
    let first = pick(word, 40, cells);
    let mut second = pick(word, 16, cells);
    if second == first {
        second = (first + 1) & (cells - 1);
    }
    [first, second]
}

impl Probe {
    /// The probe for `hash` in a table of `buckets` buckets and `cells` cells
    /// (both powers of two, `buckets >= 2`).
    fn new(hash: u64, buckets: usize, cells: usize) -> Self {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the tag is deliberately the hash's low 32 bits"
        )]
        let tag = match hash as u32 {
            0 => 1,
            tag => tag,
        };
        // The top bits of the product depend on every bit of the hash.
        let first = pick(hash.wrapping_mul(MIX), 40, buckets);
        let second = alt_bucket(first, tag, buckets);
        Self {
            buckets: [first, second],
            cells: cells_of(tag, first.min(second), cells),
            tag,
        }
    }
}

/// The sizes of a shard's recovery table once allocated, and what happens to
/// a debt the exact table cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TableSize {
    buckets: usize,
    cells: usize,
    /// [`Overflow::Forget`]: such a debt is dropped, and the sketch is never
    /// written (so it is kept at its minimum size).
    forget: bool,
}

impl TableSize {
    /// Records for [`EXACT_PER_KEY`] evicted debts per key of capacity, and
    /// [`CELLS_PER_KEY`] overflow cells per key ([`Overflow::FailClosed`]).
    fn for_cap(per_shard_cap: usize) -> Self {
        Self {
            buckets: per_shard_cap
                .saturating_mul(EXACT_PER_KEY)
                .div_ceil(BUCKET_RECORDS)
                .clamp(MIN_BUCKETS, MAX_BUCKETS)
                .next_power_of_two(),
            cells: per_shard_cap
                .saturating_mul(CELLS_PER_KEY)
                .clamp(MIN_CELLS, MAX_CELLS)
                .next_power_of_two(),
            forget: false,
        }
    }

    /// The table for `overflow`: under [`Overflow::Forget`] the sketch is
    /// never written, so it keeps only its minimum size.
    fn with_overflow(self, overflow: Overflow) -> Self {
        match overflow {
            Overflow::FailClosed => self,
            _ => Self {
                cells: MIN_CELLS,
                forget: true,
                ..self
            },
        }
    }
}

/// A debt read back for a key being stored: its own exact record and/or the
/// shared overflow cells' lower bound, combined (the larger of each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Charge {
    debt: Debt,
    /// Whether any of it came from the shared overflow sketch.
    shared: bool,
}

/// How [`Recovery::record`] stored a debt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    /// In the key's own record of the exact table.
    Exact,
    /// In the shared overflow sketch (the exact table had no room).
    Overflowed,
    /// Under [`Overflow::Forget`], the exact table had no room and a debt —
    /// this one, or one it displaced — was dropped.
    Forgotten,
}

/// Debts of keys evicted before they recovered: an exact table, and an
/// overflow sketch for when it is full. Empty until first needed.
struct Recovery {
    buckets: Box<[Bucket]>,
    cells: Box<[u64]>,
    /// Rotates which record of a full bucket a cuckoo walk moves first.
    turn: usize,
    /// Debts still to spill without a walk after the last failed one (see
    /// [`WALK_BACKOFF`]).
    backoff: u8,
}

impl Recovery {
    fn new() -> Self {
        Self {
            buckets: Box::new([]),
            cells: Box::new([]),
            turn: 0,
            backoff: 0,
        }
    }

    fn probe(&self, hash: u64) -> Option<Probe> {
        (!self.buckets.is_empty()).then(|| Probe::new(hash, self.buckets.len(), self.cells.len()))
    }

    /// The position (bucket, record) of the key's own live record, if any.
    fn own(&self, probe: &Probe, now_ms: u64) -> Option<(usize, usize)> {
        probe.buckets.iter().find_map(|&b| {
            let bucket = self.buckets.get(b)?;
            let r = bucket
                .0
                .iter()
                .position(|record| record.tag == probe.tag && record.is_live(now_ms))?;
            Some((b, r))
        })
    }

    /// Whether the exact table can take an inherited debt for `hash`: its own
    /// record, or a record in either candidate bucket that is free, expired,
    /// or holds a debt that may be moved on (one that is not inherited).
    fn has_exact_room(&self, hash: u64, now_ms: u64) -> bool {
        let Some(probe) = self.probe(hash) else {
            return true; // allocated on first record, empty
        };
        self.own(&probe, now_ms).is_some()
            || probe.buckets.iter().any(|&b| {
                self.buckets.get(b).is_some_and(|bucket| {
                    bucket
                        .0
                        .iter()
                        .any(|r| !r.is_live(now_ms) || !r.is_inherited())
                })
            })
    }

    /// A free or expired record of bucket `b`.
    fn free_in(&self, b: usize, now_ms: u64) -> Option<usize> {
        self.buckets
            .get(b)?
            .0
            .iter()
            .position(|record| !record.is_live(now_ms))
    }

    /// A record of bucket `b` that may be moved to its other bucket: live and
    /// not inherited. Starts at a rotating offset so repeated walks through a
    /// bucket do not always move the same record.
    fn movable_in(&mut self, b: usize, now_ms: u64) -> Option<usize> {
        let start = self.turn;
        self.turn = self.turn.wrapping_add(1);
        let bucket = self.buckets.get(b)?;
        (0..BUCKET_RECORDS)
            .map(|i| (start + i) % BUCKET_RECORDS)
            .find(|&r| {
                bucket
                    .0
                    .get(r)
                    .is_some_and(|record| record.is_live(now_ms) && !record.is_inherited())
            })
    }

    /// Writes `record`, found homeless at bucket `bucket`, into its key's
    /// overflow cells (each keeps the larger deadline and spend).
    fn overflow(&mut self, record: Record, bucket: usize, now_ms: u64) {
        let low = bucket.min(alt_bucket(bucket, record.tag, self.buckets.len()));
        let until_ms = record.until();
        for c in cells_of(record.tag, low, self.cells.len()) {
            if let Some(cell) = self.cells.get_mut(c) {
                let (old_until, old_spent) = unpack(*cell);
                *cell = if old_until > now_ms {
                    pack(old_until.max(until_ms), old_spent.max(record.spent))
                } else {
                    pack(until_ms, record.spent)
                };
            }
        }
    }

    /// Records that the key with `hash` owes `debt`, allocating the table on
    /// first use.
    ///
    /// The debt goes into the exact table: a free or expired record of either
    /// of the key's buckets, or else — cuckoo hashing — the place of another
    /// record, which moves to its own other bucket, displacing another in
    /// turn, for at most [`MAX_KICKS`] moves. If the walk ends with a debt
    /// still homeless, that debt goes into its key's overflow cells: nothing is
    /// ever dropped.
    ///
    /// An `inherited` debt — one a key was charged (in part) from the overflow
    /// sketch — is never written to the sketch, by this call or by a later
    /// walk: the sketch is shared, and writing back a debt read from it would
    /// let it spread and ratchet across keys that never owed it. Returns
    /// `None`, recording nothing, when an inherited debt finds no place (see
    /// [`has_exact_room`](Self::has_exact_room)).
    fn record(
        &mut self,
        hash: u64,
        debt: Debt,
        now_ms: u64,
        size: TableSize,
        inherited: bool,
    ) -> Option<Recorded> {
        if self.buckets.is_empty() {
            self.buckets = vec![Bucket::default(); size.buckets].into_boxed_slice();
            self.cells = vec![0u64; size.cells].into_boxed_slice();
        }
        let probe = Probe::new(hash, self.buckets.len(), self.cells.len());
        let flag = if inherited { INHERITED } else { 0 };
        let until_ms = ceil_millis(debt.until).min(!INHERITED);
        if let Some((b, r)) = self.own(&probe, now_ms) {
            if let Some(record) = self
                .buckets
                .get_mut(b)
                .and_then(|bucket| bucket.0.get_mut(r))
            {
                record.spent = record.spent.max(debt.spent);
                let kept = record.until_ms & INHERITED;
                record.until_ms = record.until().max(until_ms) | kept | flag;
            }
            return Some(Recorded::Exact);
        }
        let new = Record {
            tag: probe.tag,
            spent: debt.spent,
            until_ms: until_ms | flag,
        };
        for b in probe.buckets {
            if let Some(r) = self.free_in(b, now_ms) {
                let _empty = self.put(b, r, new);
                return Some(Recorded::Exact);
            }
        }
        // Both buckets are full of live debts: walk, starting from a bucket
        // with a record that may move (an inherited debt must displace one).
        // Right after a failed walk, skip it: a plain debt spills at once, an
        // inherited one displaces a single record, which spills.
        let budget = if self.backoff > 0 {
            self.backoff -= 1;
            if !inherited {
                return Some(self.spill(new, probe.buckets[0], now_ms, size));
            }
            1
        } else {
            MAX_KICKS
        };
        let mut start = None;
        for b in probe.buckets {
            if let Some(r) = self.movable_in(b, now_ms) {
                start = Some((b, r));
                break;
            }
        }
        let Some((mut b, mut r)) = start else {
            if inherited {
                return None;
            }
            return Some(self.spill(new, probe.buckets[0], now_ms, size));
        };
        let mut homeless = new;
        for kick in 1..=budget {
            homeless = self.put(b, r, homeless);
            b = alt_bucket(b, homeless.tag, self.buckets.len());
            if let Some(free) = self.free_in(b, now_ms) {
                let _empty = self.put(b, free, homeless);
                return Some(Recorded::Exact);
            }
            if kick == budget {
                break;
            }
            match self.movable_in(b, now_ms) {
                Some(next) => r = next,
                None => break,
            }
        }
        // Only a debt that was moved can be left homeless here (the loop
        // runs at least once), and only a debt that is not inherited is ever
        // moved.
        self.backoff = WALK_BACKOFF;
        Some(self.spill(homeless, b, now_ms, size))
    }

    /// Disposes of a debt the exact table could not hold, found homeless at
    /// `bucket`: into the overflow sketch, or — under [`Overflow::Forget`] —
    /// nowhere.
    fn spill(&mut self, record: Record, bucket: usize, now_ms: u64, size: TableSize) -> Recorded {
        if size.forget {
            Recorded::Forgotten
        } else {
            self.overflow(record, bucket, now_ms);
            Recorded::Overflowed
        }
    }

    /// Puts `record` at `(b, r)`, returning the record it replaces.
    fn put(&mut self, b: usize, r: usize, record: Record) -> Record {
        self.buckets
            .get_mut(b)
            .and_then(|bucket| bucket.0.get_mut(r))
            .map_or(Record::default(), |slot| core::mem::replace(slot, record))
    }

    /// The debt that applies to a key with `hash` at `now_ms`: its own exact
    /// record, combined with the overflow cells' lower bound — the smaller of
    /// the key's two cells, which is at least anything recorded for the key
    /// itself there.
    fn peek(&self, hash: u64, now_ms: u64) -> Option<Charge> {
        let probe = self.probe(hash)?;
        let own = self
            .own(&probe, now_ms)
            .and_then(|(b, r)| self.buckets.get(b)?.0.get(r))
            .map(Record::debt);
        let shared = self.shared(&probe, now_ms);
        match (own, shared) {
            (None, None) => None,
            (Some(debt), None) => Some(Charge {
                debt,
                shared: false,
            }),
            (None, Some(debt)) => Some(Charge { debt, shared: true }),
            (Some(own), Some(shared)) => Some(Charge {
                debt: Debt {
                    spent: own.spent.max(shared.spent),
                    until: own.until.max(shared.until),
                },
                shared: true,
            }),
        }
    }

    /// As [`peek`](Self::peek), for a key being stored: its own record is
    /// cleared, since the key now carries the debt itself.
    fn take(&mut self, hash: u64, now_ms: u64) -> Option<Charge> {
        let charge = self.peek(hash, now_ms)?;
        let _cleared = self.forget(hash, now_ms);
        Some(charge)
    }

    /// The overflow sketch's lower bound for a key: only when both of its cells
    /// are live, the smaller deadline and the smaller spend.
    fn shared(&self, probe: &Probe, now_ms: u64) -> Option<Debt> {
        let [a, b] = probe
            .cells
            .map(|c| unpack(self.cells.get(c).copied().unwrap_or(0)));
        (a.0 > now_ms && b.0 > now_ms).then(|| Debt {
            spent: a.1.min(b.1),
            until: Duration::from_millis(a.0.min(b.0)),
        })
    }

    /// Clears the key's own record. Returns whether there was a live one.
    /// Overflow cells are shared and cannot be cleared for one key.
    fn forget(&mut self, hash: u64, now_ms: u64) -> bool {
        let Some(probe) = self.probe(hash) else {
            return false;
        };
        let Some((b, r)) = self.own(&probe, now_ms) else {
            return false;
        };
        if let Some(record) = self
            .buckets
            .get_mut(b)
            .and_then(|bucket| bucket.0.get_mut(r))
        {
            *record = Record::default();
        }
        true
    }

    /// Whether every record and cell has expired, so the table can be released.
    fn is_spent(&self, now_ms: u64) -> bool {
        self.buckets
            .iter()
            .all(|bucket| bucket.0.iter().all(|record| !record.is_live(now_ms)))
            && self.cells.iter().all(|&cell| unpack(cell).0 <= now_ms)
    }

    /// Heap bytes the table holds.
    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        core::mem::size_of_val(&*self.buckets) + core::mem::size_of_val(&*self.cells)
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
    overflowed: usize,
    forgotten: usize,
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

    /// Whether the slot at `pos` may be evicted at `now`, and if so whether it
    /// has recovered (evicting it loses nothing). A key that still owes may be
    /// evicted if its debt can be recorded: always for a key whose own state
    /// it is, but a key whose state carries a debt charged from the shared
    /// overflow only into the exact table — never back into the overflow,
    /// which would spread debt it may not owe to other keys.
    fn evictable(&mut self, pos: usize, now: Duration, now_ms: u64) -> Evictable {
        let Self {
            slots, recovery, ..
        } = self;
        let Some(slot) = slots.get_mut(pos) else {
            return Evictable::No(UNKNOWN);
        };
        let at = slot.recovers_at(now);
        if at <= now_ms {
            Evictable::Recovered
        } else if !slot.seeded || recovery.has_exact_room(slot.hash, now_ms) {
            Evictable::Owing
        } else {
            Evictable::No(at)
        }
    }

    /// Evicts the slot at `pos`, first recording its debt if it still owes, so
    /// eviction does not reset a limit (unless, under [`Overflow::Forget`], the
    /// full exact table drops a debt).
    fn evict_at(
        &mut self,
        pos: usize,
        recovered: bool,
        now: Duration,
        now_ms: u64,
        size: TableSize,
        out: &mut Reclaimed,
    ) {
        if !recovered {
            let Self {
                slots, recovery, ..
            } = self;
            if let Some(slot) = slots.get(pos) {
                if let Some(debt) = slot.state.debt(now) {
                    // `evictable` checked that a seeded key fits the exact
                    // table, so recording cannot fail.
                    match recovery.record(slot.hash, debt, now_ms, size, slot.seeded) {
                        Some(Recorded::Overflowed) => {
                            out.remembered += 1;
                            out.overflowed += 1;
                        }
                        Some(Recorded::Exact) => out.remembered += 1,
                        Some(Recorded::Forgotten) => out.forgotten += 1,
                        None => return,
                    }
                }
            }
        }
        let _slot = self.remove_at(pos);
    }

    /// Reclaims idle-expired entries among the next [`TTL_SWEEP`] slots from
    /// the sweep cursor. An expired key that still owes and whose debt cannot
    /// be recorded stays (see [`evictable`](Self::evictable)).
    fn sweep_idle(
        &mut self,
        now: Duration,
        now_ms: u64,
        ttl_ms: u64,
        size: TableSize,
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
            let evict = if expired {
                match self.evictable(self.sweep, now, now_ms) {
                    Evictable::Recovered => Some(true),
                    Evictable::Owing => Some(false),
                    Evictable::No(_) => None,
                }
            } else {
                None
            };
            if let Some(recovered) = evict {
                // The swap moved an unexamined slot here; look at it next.
                self.evict_at(self.sweep, recovered, now, now_ms, size, out);
                out.expired += 1;
            } else {
                self.sweep += 1;
            }
        }
    }

    /// Evicts one entry by CLOCK, preferring one that has recovered: see the
    /// module docs. Examines at most [`CLOCK_SCAN`] slots (twice in the rare
    /// case none of them is a candidate). Returns `Err` with the earliest time
    /// a blocking key recovers when no examined slot can be evicted without
    /// losing a debt — the shard is saturated with keys that are still limited.
    fn evict_one(
        &mut self,
        now: Duration,
        now_ms: u64,
        ttl_ms: Option<u64>,
        size: TableSize,
        out: &mut Reclaimed,
    ) -> Result<(), u64> {
        let len = self.slots.len();
        if len == 0 {
            return Ok(());
        }
        let start = if self.hand >= len { 0 } else { self.hand };
        let scan = CLOCK_SCAN.min(len);
        let mut owing = None;
        let mut earliest = UNKNOWN;
        for step in 0..scan {
            let pos = (start + step) % len;
            let Some(slot) = self.slots.get_mut(pos) else {
                continue;
            };
            if !slot.is_expired(now_ms, ttl_ms) && slot.referenced.load(Ordering::Relaxed) {
                // Second chance: clear the bit and move on. Its checks may
                // have changed the state, so the recovery hint is stale.
                slot.referenced.store(false, Ordering::Relaxed);
                slot.recovers_at_ms = UNKNOWN;
                continue;
            }
            match self.evictable(pos, now, now_ms) {
                Evictable::Recovered => {
                    self.finish_eviction(pos, true, now, now_ms, size, out);
                    return Ok(());
                }
                Evictable::Owing => {
                    let _first = owing.get_or_insert(pos);
                }
                Evictable::No(at) => earliest = earliest.min(at),
            }
        }
        if let Some(pos) = owing {
            self.finish_eviction(pos, false, now, now_ms, size, out);
            return Ok(());
        }
        // Every examined slot was referenced (now cleared) or blocked: take
        // the first of them that can go, in the same window.
        for step in 0..scan {
            let pos = (start + step) % len;
            match self.evictable(pos, now, now_ms) {
                Evictable::Recovered => {
                    self.finish_eviction(pos, true, now, now_ms, size, out);
                    return Ok(());
                }
                Evictable::Owing => {
                    self.finish_eviction(pos, false, now, now_ms, size, out);
                    return Ok(());
                }
                Evictable::No(at) => earliest = earliest.min(at),
            }
        }
        Err(earliest)
    }

    /// Evicts `pos` and steps the hand past it: the swap moved the newest slot
    /// into `pos`, and a key that just arrived gets a full revolution before
    /// the hand returns.
    fn finish_eviction(
        &mut self,
        pos: usize,
        recovered: bool,
        now: Duration,
        now_ms: u64,
        size: TableSize,
        out: &mut Reclaimed,
    ) {
        self.evict_at(pos, recovered, now, now_ms, size, out);
        out.evicted += 1;
        self.hand = pos + 1;
    }
}

/// Whether a slot can be evicted: see [`ShardMap::evictable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Evictable {
    /// Its allowance is whole: evicting it loses nothing.
    Recovered,
    /// It still owes, and its debt can be recorded.
    Owing,
    /// It still owes and its debt cannot be recorded; it recovers at this time.
    No(u64),
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
            for _ in 0..reclaimed.overflowed {
                self.counters.overflowed();
            }
            for _ in 0..reclaimed.forgotten {
                self.counters.forgotten();
            }
        }
    }

    #[cfg_attr(not(feature = "metrics"), allow(clippy::unused_self))]
    #[inline]
    fn on_refused(&self) {
        #[cfg(feature = "metrics")]
        self.counters.refused();
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
    /// A shard's recovery table size once it is allocated.
    table: TableSize,
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
        let table = TableSize::for_cap(per_shard_cap).with_overflow(eviction.overflow());

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
            table,
        }
    }

    /// Checks `n` units against `key` as of the elapsed time `clock()`,
    /// creating the key's state with `make_state` if this is the first time the
    /// key is seen. A key whose `make_state` gives `None` (a quota that admits
    /// nothing) is denied and never stored.
    ///
    /// The clock is read only once the key's state is in hand, immediately
    /// before its accounting: the shorter the gap between reading the time and
    /// the compare-and-swap that acts on it, the less often a contended CAS
    /// fails and retries.
    #[inline]
    pub(crate) fn check(
        &self,
        key: KeyRef<'_>,
        n: u32,
        clock: impl Fn() -> Duration,
        make_state: impl FnOnce(Duration) -> Option<S>,
    ) -> Decision {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        let track_idle = self.idle_ttl_ms.is_some();

        // Fast path: a shared read lock is enough for an existing key. The
        // algorithm does its own accounting, so concurrent checks — of this key
        // or any other key in the shard — proceed without serialising.
        {
            let map = sync::read(&shard.map);
            if let Some(slot) = map.get(hash, key) {
                slot.touch();
                let now = clock();
                if track_idle {
                    slot.touch_idle(millis(now));
                }
                return shard.on_decision(slot.state.acquire(n, now));
            }
        }

        // Slow path: first-seen key. Build its state *before* taking the
        // write lock — `make_state` may run a caller's per-key quota resolver,
        // which must never block the shard or call back into it under the
        // lock — then lock, re-check (another thread may have inserted it in
        // the gap; the fresh state is then simply dropped), make room, insert.
        let now = clock();
        // A quota that admits nothing is never stored: deny it here, before
        // the write lock, so a blocked key hammering the limiter never
        // serialises its shard.
        let Some(mut state) = make_state(now) else {
            return shard.on_decision(closed(n));
        };
        let mut map = sync::write(&shard.map);
        let now_ms = millis(now);
        if let Some(slot) = map.get(hash, key) {
            if track_idle {
                slot.touch_idle(now_ms);
            }
            slot.touch();
            return shard.on_decision(slot.state.acquire(n, now));
        }
        if let Err(recovers_at_ms) = self.make_room(shard, &mut map, now, now_ms) {
            // Saturated: every examined key is still limited and its debt
            // cannot be recorded. Refuse the newcomer (fail closed) rather than
            // forget a debt or grow past the cap; retry once one recovers.
            shard.on_refused();
            let wait_ms = recovers_at_ms.saturating_sub(now_ms).max(1);
            return shard.on_decision(Decision::Deny {
                retry_after: Duration::from_millis(wait_ms),
            });
        }
        // A key evicted before it recovered resumes no more permissive than it
        // left.
        let charge = map.recovery.take(hash, now_ms);
        if let Some(charge) = charge {
            state.restore(charge.debt, now);
        }
        let seeded = charge.is_some_and(|charge| charge.shared);
        let outcome = state.acquire(n, now);
        map.insert(Slot::new(hash, key.to_key(), state, seeded, now, now_ms));
        shard.on_inserted(charge.is_some());
        shard.on_decision(outcome)
    }

    /// Makes room in a shard about to receive a new key: an incremental idle
    /// sweep, then a CLOCK eviction if the shard is full. Bounded work.
    /// `Err` (the earliest time a blocking key recovers) when the shard is full
    /// and nothing can be evicted without losing a debt.
    fn make_room(
        &self,
        shard: &Shard<S>,
        map: &mut ShardMap<S>,
        now: Duration,
        now_ms: u64,
    ) -> Result<(), u64> {
        let mut reclaimed = Reclaimed::default();
        if let Some(ttl_ms) = self.idle_ttl_ms {
            map.sweep_idle(now, now_ms, ttl_ms, self.table, &mut reclaimed);
        }
        let room = if map.slots.len() >= self.per_shard_cap {
            map.evict_one(now, now_ms, self.idle_ttl_ms, self.table, &mut reclaimed)
        } else {
            Ok(())
        };
        shard.on_reclaimed(&reclaimed);
        room
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
            .map_or(Known::Unknown, |charge| Known::Owed(charge.debt))
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
        sync::write(&shard.map).recovery.forget(hash, millis(now))
    }

    /// Forgets `key` entirely, with any debt recorded for it. Returns whether
    /// the key had either.
    pub(crate) fn remove(&self, key: KeyRef<'_>, now: Duration) -> bool {
        let hash = self.hasher.hash_one(key);
        let shard = self.shard(hash);
        let mut map = sync::write(&shard.map);
        let removed = map
            .position(hash, key)
            .and_then(|pos| map.remove_at(pos))
            .is_some();
        let forgotten = map.recovery.forget(hash, millis(now));
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
                    if map.recovery.is_spent(now_ms) {
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
        clock: impl Fn() -> Duration,
        quota: impl FnOnce() -> Quota,
    ) -> Decision {
        keyed!(self, store: State => store.check(key, n, clock, |now| State::create(&quota(), now)))
    }

    /// See [`Store::peek`].
    pub(crate) fn peek(&self, key: KeyRef<'_>, now: Duration) -> Known {
        keyed!(self, store: State => store.peek(key, now))
    }

    /// The allowance of an untracked key that owes `debt`: a fresh state under
    /// `quota` with the debt restored.
    pub(crate) fn owed_snapshot(&self, debt: Debt, quota: &Quota, now: Duration) -> Snapshot {
        keyed!(self, _store: State => State::create(quota, now).map_or_else(Snapshot::closed, |mut state| {
            state.restore(debt, now);
            state.peek(now)
        }))
    }

    /// See [`Store::reset`].
    pub(crate) fn reset(&self, key: KeyRef<'_>, now: Duration) -> bool {
        keyed!(self, store: State => store.reset(key, now))
    }

    /// See [`Store::remove`].
    pub(crate) fn remove(&self, key: KeyRef<'_>, now: Duration) -> bool {
        keyed!(self, store: State => store.remove(key, now))
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

    use super::{CLOCK_SCAN, Known, Store, TTL_SWEEP, TableSize};
    use crate::algo::KeyState;
    use crate::eviction::{Eviction, Overflow};
    use crate::key::Key;
    use crate::quota::Quota;

    /// A store under [`Overflow::FailClosed`], whose sketch most of these
    /// tests exercise; [`Overflow::Forget`] tests ask for it explicitly.
    fn make_store(shards: usize, eviction: Eviction) -> Store<BucketCore> {
        Store::new(
            shards,
            eviction.with_overflow(Overflow::FailClosed),
            Duration::ZERO,
        )
    }

    fn make_forgetting_store(shards: usize, max_keys: usize) -> Store<BucketCore> {
        Store::new(shards, Eviction::capacity(max_keys), Duration::ZERO)
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
            .check(
                key.as_key_ref(),
                n,
                || at(ms),
                |now| BucketCore::create(&quota, now),
            )
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

        /// Live cells of the shared overflow sketch, across shards.
        fn overflow_cells(&self, now_ms: u64) -> usize {
            self.shards
                .iter()
                .map(|shard| {
                    let map = super::sync::read(&shard.map);
                    map.recovery
                        .cells
                        .iter()
                        .filter(|&&cell| super::unpack(cell).0 > now_ms)
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
        let decision = store.check(
            crate::key::KeyRef::raw(long.as_bytes()),
            1,
            || at(0),
            |_| None,
        );
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
        let _ = store.check(
            key.as_key_ref(),
            1_000,
            || Duration::from_micros(999),
            |now| BucketCore::create(&quota, now),
        );
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
    fn test_an_idle_expired_key_is_judged_by_its_current_state() {
        // RN3-001: the recovery hint computed when the key was stored is stale
        // once the key spends again. A key whose own quota recovers far slower
        // than the idle TTL (a per-key override) must be recorded, not
        // forgotten, when the idle sweep takes it.
        let store: Store<BucketCore> = Store::new(
            1,
            Eviction::new(1_000, Duration::from_secs(1)),
            Duration::ZERO,
        );
        let pro = Quota::per_minute(10);
        assert!(check(&store, "pro", 1, 0, pro)); // hint: whole again at 6 s
        assert!(check(&store, "pro", 9, 500, pro)); // drained: whole at 60.5 s
        // Ten idle seconds later, enough inserts for the sweep to take it.
        for k in 0..64u64 {
            let _ = check(&store, k, 1, 10_000, pro);
        }
        assert!(!tracked(&store, "pro"), "the idle sweep took the key");
        // It comes back with only what refilled since (about one unit), not
        // a fresh allowance.
        assert!(!check(&store, "pro", 3, 10_001, pro));
        assert!(check(&store, "pro", 1, 10_001, pro));
    }

    #[test]
    fn test_a_spent_count_past_the_sketch_field_charges_everything() {
        // RN3-002: the sketch keeps 24 bits of spent units; a count that
        // fills them reads back as all of the key's allowance.
        use super::{Recorded, Recovery, SPENT_MAX, TableSize, pack, unpack};
        use crate::algo::Debt;

        let below = u32::try_from(SPENT_MAX - 1).unwrap();
        assert_eq!(unpack(pack(7, below)), (7, below));
        for spent in [SPENT_MAX, SPENT_MAX + 1, 100_000_000, u64::from(u32::MAX)] {
            let spent = u32::try_from(spent).unwrap();
            assert_eq!(unpack(pack(7, spent)), (7, u32::MAX), "{spent}");
        }
        assert_eq!(unpack(0), (0, 0));

        // Through the table: nine keys that each spent 100 M units fill the
        // eight exact records of a two-bucket table and spill one into the
        // sketch; every key reads back at least what it spent.
        let size = TableSize {
            buckets: 2,
            cells: 8,
            forget: false,
        };
        let debt = Debt {
            spent: 100_000_000,
            until: Duration::from_secs(60),
        };
        let mut table = Recovery::new();
        let hashes: Vec<u64> = (1..=9u64)
            .map(|k| k.wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .collect();
        let spilled = hashes
            .iter()
            .filter(|&&hash| table.record(hash, debt, 0, size, false) == Some(Recorded::Overflowed))
            .count();
        assert_eq!(spilled, 1);
        for &hash in &hashes {
            let charge = table.peek(hash, 0).unwrap();
            assert!(charge.debt.spent >= debt.spent, "{charge:?}");
        }
        // A window key restored from such a charge has nothing left.
        #[cfg(feature = "algorithms")]
        {
            use crate::algo::FixedWindow;
            let quota = Quota::from_parts(100_000_000, Duration::from_secs(60), 100_000_000);
            let mut state = FixedWindow::create(&quota, Duration::ZERO).unwrap();
            state.restore(
                Debt {
                    spent: u32::MAX,
                    until: Duration::from_secs(60),
                },
                Duration::ZERO,
            );
            assert_eq!(state.peek(Duration::ZERO).remaining(), 0);
        }
    }

    #[test]
    fn test_a_key_that_admits_nothing_never_takes_the_write_lock() {
        // RN3-003: hold the shard's read lock; a closed key must still be
        // answered (a write lock would wait for the guard forever).
        let store = make_store(1, Eviction::capacity(8));
        let closed = Quota::from_parts(0, Duration::from_secs(1), 0);
        let guard = super::sync::read(&store.shards[0].map);
        let (done, answered) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let _handle = scope.spawn(|| {
                let denied = !check(&store, "blocked", 1, 0, closed);
                let _sent = done.send(denied);
            });
            let denied = answered.recv_timeout(Duration::from_secs(10));
            drop(guard);
            assert_eq!(denied, Ok(true), "a closed key waited for the write lock");
        });
        assert!(!tracked(&store, "blocked"));
    }

    #[test]
    fn test_forget_drops_debts_past_the_exact_table_and_never_charges_others() {
        // Overflow::Forget: debts that fit are kept exactly (an evicted key
        // never comes back looser), debts that do not are dropped, the sketch
        // is never written, and no key that owed nothing is charged or
        // refused.
        let store = make_forgetting_store(1, 16);
        assert!(store.table.forget);
        assert_eq!(store.table.cells, super::MIN_CELLS);
        // Within the bound: a drained key evicted by a small flood comes back
        // drained.
        assert!(check(&store, "victim", 1, 0, slow(1)));
        for k in 0..20u64 {
            assert!(check(&store, k, 1, 0, slow(1)));
        }
        assert!(!tracked(&store, "victim"));
        assert!(!check(&store, "victim", 1, 0, slow(1)));
        // Far past it: every newcomer still has its whole allowance, and the
        // sketch stayed empty.
        for k in 100..10_000u64 {
            assert!(
                check(&store, k, 1, 0, slow(1)),
                "key {k} refused or charged"
            );
        }
        assert_eq!(store.overflow_cells(0), 0);
        for k in 0..100u64 {
            assert!(check(&store, format!("new-{k}"), 1, 0, slow(1)));
        }
    }

    #[test]
    fn test_recovery_table_is_lazy_and_bounded() {
        let store = make_store(1, Eviction::capacity(100));
        assert_eq!(store.recovery_bytes(), 0);
        // Far past what the table holds either way; smaller under miri, which
        // runs this test too.
        let flood = if cfg!(miri) { 1_000 } else { 10_000u64 };
        for k in 0..flood {
            let _ = check(&store, k, 1, 0, slow(5));
        }
        // Two records per key of capacity (50 buckets of 4, rounded up to 64)
        // and four overflow cells per key (512): 8 KiB, whatever the flood.
        assert_eq!(
            store.table,
            TableSize {
                buckets: 64,
                cells: 512,
                forget: false,
            }
        );
        assert_eq!(store.recovery_bytes(), 64 * 64 + 512 * 8);
        assert_eq!(
            TableSize::for_cap(1),
            TableSize {
                buckets: 2,
                cells: 8,
                forget: false,
            }
        );
        assert_eq!(
            TableSize::for_cap(usize::MAX),
            TableSize {
                buckets: 8_192,
                cells: 65_536,
                forget: false,
            }
        );
    }

    #[test]
    fn test_an_overloaded_recovery_table_fails_closed() {
        // A flood of drained keys far beyond what the table holds: debts
        // overflow into the shared sketch, and no key that left drained ever
        // returns with credit.
        let store = make_store(1, Eviction::capacity(16));
        let quota = slow(3);
        // New keys whose two overflow cells are both live start drained, and
        // a saturated shard refuses newcomers, so not every flood key is
        // admitted; count the ones that were.
        let drained: Vec<u64> = (0..2_000u64)
            .filter(|&k| check(&store, k, 3, 0, quota))
            .collect();
        assert!(drained.len() >= 16, "only {} admitted", drained.len());
        assert!(
            store.overflow_cells(0) > 0,
            "the table should have overflowed"
        );
        for &k in &drained {
            assert!(!check(&store, k, 1, 0, quota), "key {k} was reset");
        }
        // Records and cells expire with the debts they hold.
        assert!(check(&store, "after", 1, 3_600_000, quota));
        assert_eq!(store.overflow_cells(3_600_000), 0);
    }

    #[test]
    fn test_newcomers_are_charged_only_by_two_live_overflow_cells() {
        // Liveness: a key that never owed anything starts fresh unless both of
        // its overflow cells are live, and is never charged more than the
        // smaller of them — here, one unit of a key that spent one.
        let store = make_store(1, Eviction::capacity(64));
        let quota = slow(10);
        // 64 tracked, 128 exact records, then about 40 debts overflow into the
        // 64 cells. Each flood key spends one unit and is evicted owing it.
        for k in 0..232u64 {
            let _ = check(&store, k, 1, 0, quota);
        }
        assert!(store.overflow_cells(0) > 0, "some debts should overflow");
        let mut charged = 0;
        for k in 0..1_000u64 {
            let key = format!("new-{k}");
            if let Known::Owed(debt) = peek(&store, key.as_str(), 0) {
                assert!(debt.spent <= 1, "{key} would be charged {}", debt.spent);
                charged += 1;
            }
        }
        // Both cells live is the minority case at this load.
        assert!(charged < 700, "{charged} of 1000 newcomers charged");
        for k in 0..10u64 {
            assert!(check(&store, format!("new-{k}"), 9, 0, quota));
        }
    }

    #[test]
    fn test_seeded_keys_never_spread_their_debt_into_the_overflow() {
        // The ratchet the overflow must not have: a key charged from shared
        // cells is never written back into them, so cells hold only what keys
        // themselves spent, and a fresh key's charge stays bounded by that.
        let store = make_store(1, Eviction::capacity(32));
        let quota = slow(10);
        for round in 0..20u64 {
            for k in 0..2_000u64 {
                let _ = check(&store, round * 1_000_000 + k, 1, 0, quota);
            }
        }
        for k in 0..500u64 {
            if let Known::Owed(debt) = peek(&store, format!("probe-{k}").as_str(), 0) {
                assert!(debt.spent <= 1, "charged {} units", debt.spent);
            }
        }
    }

    #[test]
    fn test_a_saturated_shard_refuses_newcomers_until_a_key_recovers() {
        // Fill a tiny shard and its exact table with keys seeded from the
        // overflow and still limited: nothing can be evicted without losing a
        // debt, so a newcomer is refused with a wait — and admitted once one
        // of them recovers.
        let store = make_store(1, Eviction::capacity(2));
        let quota = Quota::from_parts(1, Duration::from_secs(60), 1);
        let mut refused = None;
        for k in 0..100_000u64 {
            let key = Key::from(k);
            let decision = store.check(
                key.as_key_ref(),
                1,
                || at(0),
                |now| BucketCore::create(&quota, now),
            );
            if let crate::decision::Decision::Deny { retry_after } = decision {
                if !tracked(&store, key.clone()) {
                    refused = Some(retry_after);
                    break;
                }
            }
        }
        let wait = refused.unwrap(); // a tiny shard under a drain flood saturates
        assert!(wait > Duration::ZERO && wait <= Duration::from_secs(60));
        assert!(check(&store, "late", 1, 60_001, quota));
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
        assert!(store.remove(Key::from("b").as_key_ref(), at(0)));
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
        assert!(!store.remove(key.as_key_ref(), at(0)));

        assert!(check(&store, key.clone(), 4, 0, quota));
        let remaining = |store: &Store<BucketCore>| match store.peek(key.as_key_ref(), at(0)) {
            Known::Tracked(snapshot) => Some(snapshot.remaining()),
            _ => None,
        };
        assert_eq!(remaining(&store), Some(6));
        assert!(store.reset(key.as_key_ref(), at(0)));
        assert_eq!(remaining(&store), Some(10));
        assert!(store.remove(key.as_key_ref(), at(0)));
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

    #[test]
    fn test_recovery_records_absorb_overflow_and_forget() {
        use super::{Charge, Recorded, Recovery, TableSize};
        use crate::algo::Debt;

        let debt = |spent, ms| Debt {
            spent,
            until: Duration::from_millis(ms),
        };
        let own = |spent, ms| Charge {
            debt: debt(spent, ms),
            shared: false,
        };
        let size = TableSize {
            buckets: 2,
            cells: 8,
            forget: false,
        };
        let mut table = Recovery::new();
        assert_eq!(table.peek(7, 0), None);
        assert!(!table.forget(7, 0));
        assert!(table.has_exact_room(7, 0));

        // The same key remembered twice keeps the larger debt and later end.
        assert_eq!(
            table.record(7, debt(2, 100), 0, size, false),
            Some(Recorded::Exact)
        );
        assert_eq!(
            table.record(7, debt(1, 300), 0, size, false),
            Some(Recorded::Exact)
        );
        assert_eq!(table.peek(7, 0), Some(own(2, 300)));
        // Its record lives only until the deadline.
        assert_eq!(table.peek(7, 300), None);
        assert!(table.is_spent(300));

        // Two buckets: every key's pair is the whole table, so cuckoo moves
        // fill all 8 records; the ninth debt overflows into the sketch.
        let hashes = (100..200u64).map(|hash| hash.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut table = Recovery::new();
        for (i, hash) in hashes.clone().take(9).enumerate() {
            let expect = if i < 8 {
                Recorded::Exact
            } else {
                Recorded::Overflowed
            };
            assert_eq!(
                table.record(hash, debt(5, 1_000), 0, size, false),
                Some(expect)
            );
        }
        // Every key reads back at least its own debt: the one whose record
        // was left homeless (whichever the walk displaced) from the sketch.
        let mut shared = 0;
        for hash in hashes.clone().take(9) {
            let charge = table.peek(hash, 0).unwrap();
            assert!(charge.debt.spent >= 5);
            shared += usize::from(charge.shared);
        }
        assert!(shared >= 1);
        assert!(!table.is_spent(0));
        assert!(table.is_spent(1_000));

        // An inherited debt displaces a movable one (which overflows), and is
        // itself never moved: once all 8 records are inherited, the next
        // inherited debt is refused and a plain one overflows.
        let mut table = Recovery::new();
        let mut inherited = hashes.clone().skip(10);
        for _ in 0..8 {
            let hash = inherited.next().unwrap();
            assert!(table.has_exact_room(hash, 0));
            assert_eq!(
                table.record(hash, debt(7, 2_000), 0, size, true),
                Some(Recorded::Exact)
            );
        }
        let refused = inherited.next().unwrap();
        assert!(!table.has_exact_room(refused, 0));
        assert_eq!(table.record(refused, debt(7, 2_000), 0, size, true), None);
        assert_eq!(
            table.record(1, debt(1, 500), 0, size, false),
            Some(Recorded::Overflowed)
        );
        for hash in hashes.skip(10).take(8) {
            let charge = table.peek(hash, 0).unwrap();
            assert_eq!(charge.debt, debt(7, 2_000));
        }
        // An inherited debt never reaches the sketch: only the plain one did.
        assert_eq!(
            table.cells.iter().filter(|&&cell| cell != 0).count(),
            2,
            "only the plain debt's two cells"
        );

        // A key's own record is cleared by `forget` and by `take`.
        let mut table = Recovery::new();
        let _ = table.record(9, debt(3, 50), 0, size, false);
        assert!(table.forget(9, 0));
        assert_eq!(table.peek(9, 0), None);
        let _ = table.record(9, debt(3, 50), 0, size, false);
        assert_eq!(table.take(9, 0), Some(own(3, 50)));
        assert_eq!(table.peek(9, 0), None);
        assert!(super::pack(u64::MAX, u32::MAX) > 0);
        assert_eq!(super::unpack(super::pack(5, 7)), (5, 7));
    }

    proptest::proptest! {
        /// Whatever the order of debts recorded, taken, and moved by cuckoo
        /// walks, every key reads back at least the debt it still owes.
        #[test]
        fn prop_recovery_never_loses_a_debt(
            buckets_log in 1u32..4,
            ops in proptest::collection::vec((0u64..48, 1u32..20, 1u64..400, 0u64..3, proptest::bool::ANY), 1..400),
        ) {
            use super::{Recovery, TableSize};
            use crate::algo::Debt;

            let size = TableSize { buckets: 1 << buckets_log, cells: 8, forget: false };
            let mut table = Recovery::new();
            // The debts recorded for each key since it last returned:
            // (spent, until_ms). Each must be covered while it is live.
            let mut owed: std::collections::HashMap<u64, Vec<(u32, u64)>> = std::collections::HashMap::new();
            let mut now = 0u64;
            for (key, spent, ttl, op, inherited) in ops {
                let hash = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (key << 7);
                match op {
                    0 => {
                        now += ttl / 8;
                    }
                    1 => {
                        // The key returns: its charge covers what it owed.
                        let charge = table.take(hash, now);
                        for &(s, u) in owed.get(&hash).into_iter().flatten() {
                            if u > now {
                                let charge = charge.unwrap();
                                proptest::prop_assert!(charge.debt.spent >= s);
                                proptest::prop_assert!(charge.debt.until >= Duration::from_millis(u));
                            }
                        }
                        let _ = owed.remove(&hash);
                    }
                    _ => {
                        let debt = Debt { spent, until: Duration::from_millis(now + ttl) };
                        if table.record(hash, debt, now, size, inherited).is_some() {
                            owed.entry(hash).or_default().push((spent, now + ttl));
                        }
                    }
                }
                for (&hash, &(s, u)) in owed.iter().flat_map(|(hash, debts)| debts.iter().map(move |debt| (hash, debt))) {
                    if u > now {
                        let charge = table.peek(hash, now);
                        proptest::prop_assert!(charge.is_some(), "a debt was lost");
                        let charge = charge.unwrap();
                        proptest::prop_assert!(charge.debt.spent >= s && charge.debt.until >= Duration::from_millis(u));
                    }
                }
            }
        }

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

        /// The same with an idle TTL and keys under a slower quota than the
        /// default the TTL was raised for (RN3-001): idle expiry never resets
        /// a key either, whichever overflow policy, while the exact table
        /// holds the debts (eight keys: at most seven evicted, against at
        /// least eight exact records).
        #[test]
        fn prop_idle_expiry_never_resets_a_slow_key(
            cap in 1usize..12,
            fail_closed in proptest::bool::ANY,
            ops in proptest::collection::vec((0u64..8, 0u64..3_000, 1u32..4), 1..400),
        ) {
            let fast = Quota::per_second(5).with_burst(4);
            let slow = Quota::rate(5, Duration::from_secs(10)).unwrap();
            let eviction = Eviction::new(cap, Duration::from_millis(1))
                .with_overflow(if fail_closed { Overflow::FailClosed } else { Overflow::Forget });
            // The TTL is raised to the default quota's recovery, 0.8 s.
            let store: Store<BucketCore> = Store::new(1, eviction, Duration::from_millis(800));
            let mut admitted: std::collections::HashMap<u64, Vec<(u64, u64)>> =
                std::collections::HashMap::new();
            let mut now = 0u64;
            for (key, advance, n) in ops {
                now += advance;
                let quota = if key >= 4 { slow } else { fast };
                if check(&store, key, n, now, quota) {
                    admitted.entry(key).or_default().push((now, u64::from(n)));
                }
            }
            for (key, events) in &admitted {
                // burst + limit * elapsed / period, scaled by the period.
                let (burst, limit, period) = if *key >= 4 { (5, 5, 10_000) } else { (4, 5, 1_000) };
                for (i, &(a, _)) in events.iter().enumerate() {
                    let mut units = 0u64;
                    for &(b, n) in &events[i..] {
                        units += n;
                        proptest::prop_assert!(
                            units * period <= burst * period + limit * (b - a),
                            "key {} admitted {} units in [{}ms, {}ms]", key, units, a, b
                        );
                    }
                }
            }
        }
    }
}
