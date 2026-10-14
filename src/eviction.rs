//! How the per-key store bounds its memory.

use core::time::Duration;

/// The default key-capacity cap, applied unless overridden.
///
/// Roughly a million keys. Generous for legitimate traffic, but a hard ceiling
/// so a flood of unique keys can never grow the store without limit. At this
/// cap a default token-bucket limiter holds about 138 MiB of per-key state
/// (measured: `tests/memory.rs`), plus up to 8 MiB of recovery records under
/// eviction pressure, plus the bytes of any key longer than 22 bytes. Pick a
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
/// # Eviction never resets a limit
///
/// Evicting a key that is still being limited — one that has spent part of its
/// allowance and not yet recovered it — must not hand it a fresh allowance when
/// it returns, or anyone could reset a target's limit (login attempts for one
/// username, say) by flooding the limiter with throwaway keys. So when such a
/// key is evicted, its shard records what it owes: the units spent and when
/// they would all be back. A key inserted before that time resumes **no more
/// permissive than it left**:
///
/// - token and leaky buckets resume where they were, to within one unit
///   (rounded against the key);
/// - window algorithms resume with the spent units charged at the return, so
///   they may be held back for up to one window longer than the original.
///
/// The records live in a per-shard table of half the shard's capacity (16
/// bytes each, at most 8,192 per shard, allocated the first time a shard evicts
/// an unrecovered key; `retain_recent` releases a table once its records have
/// all expired). Each key has two candidate records. When more unrecovered keys
/// are evicted within one recovery period than the table can hold — a flood far
/// beyond the cap — a new record is **merged** into an existing one, which
/// then keeps the larger debt and the later deadline and applies to *every*
/// key that maps to it until it expires. Merging never forgets a debt; it can
/// only make an unrelated newcomer start partly or wholly drained. Under
/// extreme pressure the limiter therefore **fails closed** for some new keys
/// instead of failing open for evicted ones. `Metrics::records_merged`
/// (feature `metrics`) counts merges; any sustained rate means the cap is far
/// too small for the traffic.
///
/// `remove`, `reset`, and `clear` forget a key's record along with its state;
/// they are explicit resets.
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
    use super::{DEFAULT_MAX_KEYS, Eviction};
    use core::time::Duration;

    #[test]
    fn test_default_is_bounded() {
        let policy = Eviction::default();
        assert_eq!(policy.max_keys(), Some(DEFAULT_MAX_KEYS));
        assert_eq!(policy.idle_ttl(), None);
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
