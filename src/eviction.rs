//! How the per-key store bounds its memory.

use core::time::Duration;

/// The default key-capacity cap, applied unless overridden.
///
/// Roughly a million keys. Generous for legitimate traffic, but a hard ceiling
/// so a flood of unique keys can never grow the store without limit. At this
/// cap a default token-bucket limiter holds about 138 MiB of per-key state
/// (measured: `tests/memory.rs`), plus the recovery tables under eviction
/// pressure — 64 MiB with the default [`Overflow::FailClosed`], 32 MiB with
/// [`Overflow::Forget`] (about 202 or 170 MiB in all) — plus the bytes of
/// any key longer than 22 bytes. Pick a
/// value that matches your real key cardinality with [`Eviction::capacity`].
pub const DEFAULT_MAX_KEYS: usize = 1 << 20;

/// How the limiter bounds the memory its per-key state can occupy.
///
/// A rate limiter that tracks state per key is a denial-of-service vector
/// against itself if that state can grow without limit: a flood of unique keys
/// would exhaust memory. `Eviction` is the defense. Two independent bounds
/// compose:
///
/// - **Capacity** — a hard ceiling on the number of live keys. When inserting a
///   new key would exceed it, one key is evicted by CLOCK (second chance),
///   preferring a key whose allowance has **recovered** (is whole again —
///   forgetting it loses nothing), and otherwise the first key not checked
///   since the sweeping hand last passed it, so a key seen once goes before a
///   key in active use. This is what caps a unique-key flood.
/// - **Idle TTL** — keys not seen for longer than the TTL are reclaimed, so
///   long-idle state is not held forever. The effective TTL is never shorter
///   than the quota's *recovery time* (the time an idle key needs to regain its
///   full allowance), so an expired key has normally recovered. `try_build`
///   rejects a shorter TTL; the other constructors raise it.
///
/// Eviction is **lazy and bounded**: it happens only while inserting a new key,
/// touches only the one shard being written, and does a fixed amount of work —
/// a CLOCK step of at most 32 slots and an idle sweep of 8 — however large the
/// shard. It never sweeps the whole store. The capacity is divided evenly over
/// the shards (rounded up, at least one key per shard), so the live-key count
/// stays at or below the cap plus one key per shard.
///
/// # Eviction does not reset a limit
///
/// Evicting a key that is still being limited — one that has spent part of its
/// allowance and not yet recovered it — must not hand it a fresh allowance when
/// it returns, or anyone could reset a target's limit (login attempts for one
/// username, say) by flooding the limiter with throwaway keys. So when such a
/// key is evicted, its shard records what it owes: the units spent and when
/// they would all be back. A key inserted before that time resumes **no more
/// permissive than it left**:
///
/// - token and leaky buckets resume exactly where they were (the deadline is
///   rounded up to the millisecond, against the key);
/// - window algorithms resume with the spent units charged at the return, so
///   they may be held back for up to one window longer than the original.
///
/// The debts live in a per-shard recovery table, allocated the first time a
/// shard evicts an unrecovered key (`retain_recent` releases it once every
/// debt has expired): an **exact** cuckoo-hashed table of 16-byte records, two
/// per key of capacity, in which a key reads back only its own record. While
/// a shard's still-limited evicted keys fit — about twice its share of the
/// cap at once; what counts is the rate at which still-limited keys are
/// evicted times their recovery time, not the size of the population — the
/// guarantee is unconditional and costs nothing in liveness: no evicted key
/// is reset, no key that never owed anything is charged, and nothing is
/// refused. Idle-TTL expiry records debts the same way (a key with a
/// per-key quota that recovers more slowly than the TTL is judged by its
/// current state when it expires).
///
/// Past that bound the [`Overflow`] policy decides
/// ([`with_overflow`](Self::with_overflow)):
///
/// - [`Overflow::FailClosed`] (the default) keeps every debt, spilling those the exact table
///   cannot place into a shared **overflow sketch** of four 8-byte cells per
///   key of capacity. A key being stored is charged the smaller of its two
///   cells while both are live — never less than its own spilled debt — so a
///   key that never owed anything can start partly or wholly drained. A key
///   charged from the sketch never writes that charge back, so it cannot
///   spread or ratchet, and a full shard that can neither evict nor record
///   **refuses** the newcomer with a retry time. The sketch keeps 24 bits of a
///   spent count; a larger count is charged as everything;
/// - [`Overflow::Forget`] (opt-out) drops the debts that do not fit: those
///   keys may come back with a fresh allowance, as every evicted key did in
///   1.x, and no other key is ever affected. Under a sustained flood that
///   evicts far more still-limited keys than the table holds, a targeted key
///   can be reset repeatedly (`Metrics::debts_forgotten` counts the drops).
///
/// Measured (`docs/BENCHMARKS.md`; 128 shards, cap 100,000 for the floods and
/// 10,000 for the populations): with `FailClosed`, newcomers after a flood of
/// unique keys that each drain their quota are never denied up to a flood of
/// 3x the cap, about 3 % at 4x, 38 % at 6x, 86 % at 9x; a well-behaved
/// population of 10x the cap sees about 4 % false denials at 5,000 visits a
/// second and **72 % at 20,000** — a unique-key flood can lock legitimate
/// newcomers out. With `Forget` none of these keys is ever denied, but in a
/// real-clock race of four drained victims against unique-key flood threads
/// (cap 32) the victims were reset over and over. `FailClosed` is the default
/// and the right choice for limiters keyed by identities an attacker can
/// target (usernames, API keys, account ids); opt into `Forget` only where
/// keeping newcomers admitted under a flood matters more than never resetting
/// a key (load shedding for anonymous traffic).
///
/// The exact table costs 32 bytes per key of capacity, and `FailClosed`'s
/// sketch another 32 (64 or 32 MiB at the default cap; up to twice that when
/// a shard's share of the cap is just past a power of two).
/// `Metrics::debts_forgotten`, `debts_overflowed`, and `keys_refused`
/// (feature `metrics`) count drops, spills, and refusals: any sustained rate
/// means the cap is too small for the traffic.
///
/// # Sizing the cap
///
/// The overflow policy only matters once more still-limited keys are evicted
/// within one recovery period than the exact table holds. Size the cap so
/// that never happens: `cap >= peak distinct keys active within the longest
/// recovery window × headroom` (1.5–2x), where the recovery window is
/// `burst / limit` periods for the buckets, one period for the fixed window
/// and sliding-window log, and two for the sliding-window counter. Budget
/// about 202 bytes per key of capacity under eviction pressure with
/// `FailClosed` (138 of token-bucket state and 64 of recovery tables), 170
/// with `Forget`. Then export
/// [`RateLimiter::recovery_load`](crate::RateLimiter::recovery_load)
/// (feature `metrics`) and alert at about 0.7 utilization, alert on any
/// `Metrics::keys_refused`, and enable the `tracing` feature for rate-limited
/// warnings when the store does run out of room.
///
/// `remove`, `reset`, and `clear` forget a key's own record along with its
/// state; they are explicit resets. A sketch charge is shared and cannot be
/// cleared for one key (it expires within one recovery period); `clear`
/// drops the whole table.
///
/// Size the cap above your real key cardinality so only flood keys are ever
/// evicted. A tracked token-bucket key costs about 138 bytes of heap at the
/// default cap (up to ~210 bytes just after a shard's tables have grown), plus
/// its bytes beyond 22; see
/// [`Builder::max_key_len`](crate::Builder::max_key_len) and
/// [`Builder::digest_keys_longer_than`](crate::Builder::digest_keys_longer_than)
/// to bound key bytes.
///
/// The [`Default`] is safe: a [`DEFAULT_MAX_KEYS`] capacity cap and no TTL —
/// memory is bounded out of the box. Opt into unbounded growth explicitly with
/// [`unbounded`](Self::unbounded), understanding the risk.
///
/// # Examples
///
/// ```
/// use rate_net::Eviction;
/// use std::time::Duration;
///
/// // Cap at 100k keys and reclaim anything idle for five minutes.
/// let policy = Eviction::capacity(100_000).with_idle(Duration::from_secs(300));
/// assert_eq!(policy.max_keys(), Some(100_000));
/// assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(300)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eviction {
    max_keys: Option<usize>,
    idle_ttl: Option<Duration>,
    overflow: Overflow,
}

impl Eviction {
    /// A hard cap of `max_keys` live keys, with no idle expiry.
    ///
    /// A `max_keys` of `0` is treated as `1` (the store always holds at least
    /// one key per shard).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    ///
    /// let policy = Eviction::capacity(50_000);
    /// assert_eq!(policy.max_keys(), Some(50_000));
    /// assert_eq!(policy.idle_ttl(), None);
    /// ```
    #[must_use]
    pub const fn capacity(max_keys: usize) -> Self {
        Self {
            max_keys: Some(max_keys),
            idle_ttl: None,
            overflow: DEFAULT_OVERFLOW,
        }
    }

    /// Reclaim keys idle for longer than `ttl`, keeping the default capacity cap.
    ///
    /// Idle expiry alone does not bound a unique-key flood — flooded keys are not
    /// idle — so this keeps the [`DEFAULT_MAX_KEYS`] cap as the flood defense and
    /// layers the TTL on top. Use [`capacity`](Self::capacity) plus
    /// [`with_idle`](Self::with_idle) to choose both bounds explicitly.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Eviction, DEFAULT_MAX_KEYS};
    /// use std::time::Duration;
    ///
    /// let policy = Eviction::idle(Duration::from_secs(300));
    /// assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(300)));
    /// assert_eq!(policy.max_keys(), Some(DEFAULT_MAX_KEYS));
    /// ```
    #[must_use]
    pub const fn idle(ttl: Duration) -> Self {
        Self {
            max_keys: Some(DEFAULT_MAX_KEYS),
            idle_ttl: Some(ttl),
            overflow: DEFAULT_OVERFLOW,
        }
    }

    /// Both bounds at once: a `max_keys` cap and an idle `ttl`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    /// use std::time::Duration;
    ///
    /// let policy = Eviction::new(10_000, Duration::from_secs(60));
    /// assert_eq!(policy.max_keys(), Some(10_000));
    /// assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub const fn new(max_keys: usize, ttl: Duration) -> Self {
        Self {
            max_keys: Some(max_keys),
            idle_ttl: Some(ttl),
            overflow: DEFAULT_OVERFLOW,
        }
    }

    /// No bounds at all — the store grows without limit.
    ///
    /// Only safe when the key space is intrinsically bounded (a fixed set of
    /// tenants, say). Against untrusted keys this is a self-inflicted
    /// denial-of-service: prefer [`capacity`](Self::capacity).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    ///
    /// let policy = Eviction::unbounded();
    /// assert_eq!(policy.max_keys(), None);
    /// assert_eq!(policy.idle_ttl(), None);
    /// ```
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_keys: None,
            idle_ttl: None,
            overflow: DEFAULT_OVERFLOW,
        }
    }

    /// Returns a copy with the capacity cap set to `max_keys`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    /// use std::time::Duration;
    ///
    /// let policy = Eviction::idle(Duration::from_secs(30)).with_capacity(1_000);
    /// assert_eq!(policy.max_keys(), Some(1_000));
    /// ```
    #[must_use]
    pub const fn with_capacity(mut self, max_keys: usize) -> Self {
        self.max_keys = Some(max_keys);
        self
    }

    /// Returns a copy with idle expiry set to `ttl`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    /// use std::time::Duration;
    ///
    /// let policy = Eviction::capacity(1_000).with_idle(Duration::from_secs(30));
    /// assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(30)));
    /// ```
    #[must_use]
    pub const fn with_idle(mut self, ttl: Duration) -> Self {
        self.idle_ttl = Some(ttl);
        self
    }

    /// Returns a copy with the capacity cap removed (unbounded key count).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    ///
    /// let policy = Eviction::default().without_capacity();
    /// assert_eq!(policy.max_keys(), None);
    /// ```
    #[must_use]
    pub const fn without_capacity(mut self) -> Self {
        self.max_keys = None;
        self
    }

    /// The configured capacity cap, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    ///
    /// assert_eq!(Eviction::capacity(10).max_keys(), Some(10));
    /// assert_eq!(Eviction::unbounded().max_keys(), None);
    /// ```
    #[must_use]
    pub const fn max_keys(&self) -> Option<usize> {
        self.max_keys
    }

    /// The configured idle TTL, if any — as given, before it is raised to the
    /// quota's recovery time.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Eviction;
    /// use std::time::Duration;
    ///
    /// assert_eq!(Eviction::idle(Duration::from_secs(9)).idle_ttl(), Some(Duration::from_secs(9)));
    /// assert_eq!(Eviction::capacity(10).idle_ttl(), None);
    /// ```
    #[must_use]
    pub const fn idle_ttl(&self) -> Option<Duration> {
        self.idle_ttl
    }

    /// Chooses what happens when still-limited keys are evicted faster than
    /// the store can keep their debts exactly (see [`Overflow`]). The default
    /// is [`Overflow::FailClosed`]: no flood ever resets a key's limit.
    ///
    /// Opt into [`Overflow::Forget`] only for availability-first limiters —
    /// load shedding keyed by client address, say — where keeping newcomers
    /// admitted under a flood matters more than never resetting a key.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Eviction, Overflow};
    ///
    /// let shedding = Eviction::capacity(100_000).with_overflow(Overflow::Forget);
    /// assert_eq!(shedding.overflow(), Overflow::Forget);
    /// assert_eq!(Eviction::capacity(100_000).overflow(), Overflow::FailClosed);
    /// ```
    #[must_use]
    pub const fn with_overflow(mut self, overflow: Overflow) -> Self {
        self.overflow = overflow;
        self
    }

    /// The overflow policy (see [`with_overflow`](Self::with_overflow)).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Eviction, Overflow};
    ///
    /// assert_eq!(Eviction::default().overflow(), Overflow::FailClosed);
    /// ```
    #[must_use]
    pub const fn overflow(&self) -> Overflow {
        self.overflow
    }
}

/// What the store does with the debts of still-limited keys it evicts faster
/// than it can keep them exactly — [`Eviction::with_overflow`].
///
/// Each shard keeps the debt of every key it evicts before the key has fully
/// recovered, so a key that returns resumes no more permissive than it left.
/// Debts are kept **exactly** — each key reads back only its own — for about
/// twice the shard's share of the cap at once (the count that matters is the
/// still-limited keys evicted within one recovery period: the eviction rate
/// times the recovery time, not the size of the population). Within that
/// bound both policies behave identically: no evicted key is reset, and no
/// other key is affected. They differ only past it:
///
/// | | `FailClosed` (default) | `Forget` |
/// |---|---|---|
/// | A debt that does not fit | is kept in a shared sketch | is dropped |
/// | Evicted, still-limited keys past the bound | never come back looser than they left | may come back with a fresh allowance (1.x behaviour) |
/// | Keys that never owed anything | may start partly or wholly drained, or be refused with a retry time | never charged, never refused |
///
/// Measured (`docs/BENCHMARKS.md`, cap 100,000 for floods, 10,000 for
/// populations, 128 shards): with `FailClosed`, newcomers after a flood of
/// unique keys that each drain their quota are never denied up to a flood of
/// 3x the cap, about 3 % at 4x, 39 % at 6x; a well-behaved population of 10x
/// the cap sees about 4 % false denials at 5,000 visits a second but **72 %
/// at 20,000** — a unique-key flood can lock legitimate newcomers out. With
/// `Forget` none of them is ever denied, but a flood that evicts more than
/// about twice the cap's worth of still-limited keys within one recovery
/// period can reset some of them.
///
/// **`FailClosed` is the default** — secure by default, and the right choice
/// for limiters keyed by identities an attacker can target (usernames, API
/// keys, account ids — login attempts, password resets, paid quotas), where a
/// reset is the attack. Opt into `Forget` for limiters that shed load from
/// anonymous traffic (per-IP limits in front of a service), where keeping
/// newcomers admitted under a flood matters more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Overflow {
    /// Drop debts the exact table cannot hold (1.x behaviour past the bound;
    /// never denies a key that owed nothing, but may reset evicted keys).
    Forget,
    /// Keep every debt, spilling those the exact table cannot hold into a
    /// shared sketch that may charge or refuse keys that owed nothing; an
    /// evicted key never comes back looser than it left. The default.
    FailClosed,
}

/// The overflow policy every [`Eviction`] constructor and
/// [`Overflow::default`] start from: the one place the default is decided.
const DEFAULT_OVERFLOW: Overflow = Overflow::FailClosed;

impl Default for Overflow {
    /// [`Overflow::FailClosed`].
    fn default() -> Self {
        DEFAULT_OVERFLOW
    }
}

impl Default for Eviction {
    /// A [`DEFAULT_MAX_KEYS`] capacity cap and no idle TTL — bounded memory out
    /// of the box.
    fn default() -> Self {
        Self::capacity(DEFAULT_MAX_KEYS)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MAX_KEYS, Eviction, Overflow};
    use core::time::Duration;

    #[test]
    fn test_default_is_bounded() {
        let policy = Eviction::default();
        assert_eq!(policy.max_keys(), Some(DEFAULT_MAX_KEYS));
        assert_eq!(policy.idle_ttl(), None);
        assert_eq!(policy.overflow(), Overflow::FailClosed);
    }

    #[test]
    fn test_overflow_policy_composes() {
        for policy in [
            Eviction::capacity(10),
            Eviction::idle(Duration::from_secs(1)),
            Eviction::new(10, Duration::from_secs(1)),
            Eviction::unbounded(),
        ] {
            assert_eq!(policy.overflow(), Overflow::FailClosed);
            let open = policy.with_overflow(Overflow::Forget);
            assert_eq!(open.overflow(), Overflow::Forget);
            assert_eq!(open.max_keys(), policy.max_keys());
            assert_eq!(
                open.with_idle(Duration::from_secs(2)).overflow(),
                Overflow::Forget
            );
        }
        assert_eq!(Overflow::default(), Overflow::FailClosed);
    }

    #[test]
    fn test_every_constructor_starts_from_the_one_default() {
        let all = [
            Eviction::default(),
            Eviction::capacity(10),
            Eviction::idle(Duration::from_secs(1)),
            Eviction::new(10, Duration::from_secs(1)),
            Eviction::unbounded(),
            Eviction::unbounded()
                .with_capacity(5)
                .with_idle(Duration::from_secs(1)),
            Eviction::capacity(10).without_capacity(),
        ];
        for policy in all {
            assert_eq!(policy.overflow(), super::DEFAULT_OVERFLOW);
            assert_eq!(policy.overflow(), Overflow::default());
        }
        assert_eq!(super::DEFAULT_OVERFLOW, Overflow::FailClosed);
    }

    #[test]
    fn test_idle_keeps_default_cap() {
        let policy = Eviction::idle(Duration::from_secs(5));
        assert_eq!(policy.max_keys(), Some(DEFAULT_MAX_KEYS));
        assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_unbounded_has_no_bounds() {
        let policy = Eviction::unbounded();
        assert_eq!(policy.max_keys(), None);
        assert_eq!(policy.idle_ttl(), None);
    }

    #[test]
    fn test_builders_compose() {
        let policy = Eviction::capacity(10).with_idle(Duration::from_secs(1));
        assert_eq!(policy.max_keys(), Some(10));
        assert_eq!(policy.idle_ttl(), Some(Duration::from_secs(1)));

        let dropped = policy.without_capacity();
        assert_eq!(dropped.max_keys(), None);
        assert_eq!(dropped.idle_ttl(), Some(Duration::from_secs(1)));
    }
}
