//! The Generic Cell Rate Algorithm: the clock-free single-key core.
//!
//! [`Gcra`] is the leaky bucket: where a token bucket lets a full burst through
//! and then sustains the rate, the GCRA *spaces* admitted units one emission
//! interval apart, smoothing traffic toward a steady output rate while still
//! tolerating a burst up to the quota's `burst`. The whole state is one
//! atomic "theoretical arrival time" advanced by compare-and-swap, so a check is
//! lock-free.
//!
//! It is also the crate's `no_std` mode: the type needs nothing but `core` and
//! 64-bit atomics. The caller supplies the time: any monotonic reading
//! expressed as a [`Duration`] since a fixed origin of the caller's choosing (a
//! hardware timer, an RTOS tick count, `Instant::elapsed` from a start point).
//! The keyed [`RateLimiter`](crate::RateLimiter) runs the same engine per key
//! when it uses the leaky-bucket algorithm.
//!
//! The arithmetic lives in [`GcraParams`], which holds a quota's parameters
//! once and runs against any theoretical arrival time the caller passes in.
//! [`Gcra`] is one such time plus its parameters; a caller with its own keyed
//! table shares one `GcraParams` across all of its keys and stores only the
//! 8-byte word per key.
//!
//! # Exact units
//!
//! Time is accounted in fixed fractions of an *emission interval* rather than
//! in nanoseconds: one unit of the theoretical arrival time is exactly
//! `period / (limit * 2^k)`, where `2^k` (at most `2^30`) is the largest power
//! of two that keeps that unit at least one nanosecond. An interval in whole
//! nanoseconds would truncate (`period / limit` rounds down), which over-admits
//! (600 M/s became 1 G/s, a 1.67x overshoot) and collapses to zero once the
//! rate passes one per nanosecond. In these units every admitted unit costs
//! exactly `2^k`, so there is no drift at any rate. The current time is
//! converted with a precomputed 64.64 fixed-point reciprocal (two multiplies,
//! no division on the check path) and corrected to the exact floor.
//!
//! The fractions are what keep the spacing honest. With whole intervals, the
//! part of an interval that had already passed was credited for free, so a
//! burst-1 limit at one per second admitted units at 1.999 s and 2.000 s. Now
//! two admitted units of a burst-1 limit are at least one interval apart, less
//! at most one unit of the time line: under 2 ns, or `1/2^30` of the interval
//! for intervals longer than about one second.

use core::fmt;
use core::sync::atomic::AtomicU64;
use core::time::Duration;

use crate::decision::Decision;
use crate::error::RateLimiterError;
use crate::quota::Quota;
use crate::snapshot::Snapshot;
// `core`'s ordering, re-exported by loom too, so one type in every build.
use crate::sync::Ordering;

/// Most time-line units per emission interval, as a power of two: `2^30`. Keeps
/// `(burst - 1) * 2^k` within `u64` for any `u32` burst.
const MAX_SHIFT: u32 = 30;

/// Where [`GcraParams::shift`] sits in `per_ns`: the reciprocal is at most
/// `2^64`, so bits 96 and up are free.
const SHIFT_BITS_AT: u32 = 96;
/// The reciprocal bits of `per_ns`.
const RECIPROCAL_MASK: u128 = (1 << 65) - 1;

/// Clamps a duration-since-epoch to nanoseconds, saturating rather than wrapping
/// past the ~584-year `u64` ceiling.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// The atomic word a theoretical arrival time lives in.
///
/// The public [`GcraParams`] methods take `core`'s [`AtomicU64`]. [`Gcra`]
/// keeps its own word in `crate::sync`'s, which is loom's under `cfg(loom)`,
/// so the loom suite model-checks the keyed limiter's real leaky-bucket state
/// through this same code. Outside `cfg(loom)` the two are one type and this
/// trait has a single implementation.
trait Word {
    fn load(&self, order: Ordering) -> u64;
    fn compare_exchange_weak(
        &self,
        current: u64,
        new: u64,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u64, u64>;
    #[cfg(all(feature = "std", feature = "algorithms"))]
    fn fetch_max(&self, value: u64, order: Ordering) -> u64;
}

macro_rules! impl_word {
    ($atomic:ty) => {
        impl Word for $atomic {
            #[inline]
            fn load(&self, order: Ordering) -> u64 {
                <$atomic>::load(self, order)
            }

            #[inline]
            fn compare_exchange_weak(
                &self,
                current: u64,
                new: u64,
                success: Ordering,
                failure: Ordering,
            ) -> Result<u64, u64> {
                <$atomic>::compare_exchange_weak(self, current, new, success, failure)
            }

            #[cfg(all(feature = "std", feature = "algorithms"))]
            #[inline]
            fn fetch_max(&self, value: u64, order: Ordering) -> u64 {
                <$atomic>::fetch_max(self, value, order)
            }
        }
    };
}

impl_word!(AtomicU64);
#[cfg(loom)]
impl_word!(crate::sync::AtomicU64);

/// A GCRA quota's parameters, computed once and run against theoretical
/// arrival times the caller stores.
///
/// A [`Gcra`] is one theoretical arrival time (TAT) plus these parameters,
/// about 80 bytes. A keyed limiter that tracks many streams under one quota
/// needs the parameters only once: each stream is then a single 8-byte
/// [`AtomicU64`], which can sit beside its key in the caller's own table, so a
/// check touches one cache line instead of two. The methods here are the
/// arithmetic [`Gcra`] itself runs; `Gcra` is built on them, so the two decide
/// identically for the same quota, word, and times.
///
/// # The TAT word
///
/// A word holding `0` is a full bucket: time 0 is at or before any `now`.
/// So `AtomicU64::new(0)` starts a stream with its whole burst, and storing
/// `0` (or calling [`reset`](Self::reset)) restores it. Any other value is the
/// theoretical arrival time in *this* quota's fixed fractions of an emission
/// interval (`2^k` of them per interval, `k <= 30`); treat it as opaque.
/// Use a word with one set of parameters only; read under a different quota,
/// its value means something else. Since 2.2.0 the fractions are finer than
/// the whole intervals 2.1 used, so a word saved by 2.1 reads as an earlier
/// time (a fuller bucket) under 2.2. The methods load and update the word atomically, so
/// any number of threads may check one word at once.
///
/// # Time and quota semantics
///
/// As for [`Gcra`]: times are [`Duration`]s since one fixed origin from one
/// monotonic source, and a reading earlier than one already used can only
/// make a decision stricter. `limit` units per `period`, with bursts of up to
/// `burst` units at once. A rate finer than one unit per nanosecond is clamped
/// to it, and a zero limit, period, or burst admits nothing; use
/// [`try_new`](Self::try_new) to reject those instead.
///
/// Available on every target with 64-bit atomics, `no_std` included.
///
/// # Examples
///
/// ```
/// use core::sync::atomic::AtomicU64;
/// use core::time::Duration;
/// use rate_net::{GcraParams, Quota};
///
/// // One set of parameters, shared by every stream under the same quota.
/// let params = GcraParams::new(Quota::per_second(10));
/// let alice = AtomicU64::new(0);
/// let bob = AtomicU64::new(0);
/// let t0 = Duration::ZERO;
///
/// for _ in 0..10 {
///     assert!(params.check_at(&alice, t0).is_allow());
/// }
/// assert!(params.check_at(&alice, t0).is_deny());
/// // Bob's word is untouched by Alice's traffic.
/// assert!(params.check_at(&bob, t0).is_allow());
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GcraParams {
    /// Time-line units per period: the effective `limit`, clamped to at most
    /// one unit per nanosecond, times `2^k` sub-intervals per emission
    /// interval (see [`shift`](Self::shift)). Never more than `period_ns`.
    limit: u64,
    /// Period in nanoseconds.
    period_ns: u64,
    /// `limit * 2^64 / period_ns`, the ns→interval reciprocal, in the low 65
    /// bits, and the [`shift`](Self::shift) from bit 96 up. The reciprocal is
    /// at most `2^64` (exactly `2^64` when one unit of the time line is one
    /// nanosecond). The reciprocal is zero exactly when
    /// the quota admits nothing (`limit == 0`, `burst == 0`, or a zero period):
    /// with `1 <= limit <= period_ns < 2^64` the quotient is at least one, so
    /// zero is free to mark the closed quota without a separate flag (which
    /// would round the struct up past one cache line).
    per_ns: u128,
    /// Delay tolerance in time-line units: `(burst - 1) * 2^k`, the slack a
    /// burst rides on.
    tolerance: u64,
    /// The quota as configured, for introspection.
    quota: Quota,
}

impl GcraParams {
    /// The parameters for `quota`. Infallible and fail-closed: a zero limit,
    /// period, or burst admits nothing, and a rate finer than one unit per
    /// nanosecond is clamped to it.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::sync::atomic::AtomicU64;
    /// use core::time::Duration;
    /// use rate_net::{GcraParams, Quota};
    ///
    /// let closed = GcraParams::new(Quota::per_second(0));
    /// assert!(closed.check_at(&AtomicU64::new(0), Duration::ZERO).is_deny());
    /// ```
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        let period_ns = nanos(quota.period());
        let burst = quota.burst();
        // Fail closed on a rate finer than one unit per nanosecond: clamp it to
        // exactly one per nanosecond (admit less, never more).
        let limit = u64::from(quota.limit()).min(period_ns);
        // Split each emission interval into `2^shift` units of the time line,
        // as many as keep a unit at least one nanosecond (and at most 2^30, so
        // the tolerance below always fits).
        let shift = period_ns
            .checked_div(limit)
            .map_or(0, |per_unit| (63 - per_unit.leading_zeros()).min(MAX_SHIFT));
        let limit = limit << shift;
        let reciprocal = if limit == 0 || burst == 0 {
            0
        } else {
            (u128::from(limit) << 64) / u128::from(period_ns)
        };
        let per_ns = reciprocal | (u128::from(shift) << SHIFT_BITS_AT);
        Self {
            limit,
            period_ns,
            per_ns,
            tolerance: u64::from(burst.saturating_sub(1)) << shift,
            quota,
        }
    }

    /// The parameters for `quota`, validated.
    ///
    /// # Errors
    ///
    /// - [`RateLimiterError::ZeroQuota`]: the limit is `0`.
    /// - [`RateLimiterError::ZeroPeriod`]: the period is zero.
    /// - [`RateLimiterError::ZeroBurst`]: the burst is `0`.
    /// - [`RateLimiterError::RateTooHigh`]: more than one unit per nanosecond.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{GcraParams, Quota, RateLimiterError};
    ///
    /// assert!(GcraParams::try_new(Quota::per_second(100)).is_ok());
    /// assert_eq!(
    ///     GcraParams::try_new(Quota::per_second(100).with_burst(0)),
    ///     Err(RateLimiterError::ZeroBurst),
    /// );
    /// ```
    pub fn try_new(quota: Quota) -> Result<Self, RateLimiterError> {
        validate(&quota)?;
        Ok(Self::new(quota))
    }

    /// The quota these parameters were built from, as configured.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{GcraParams, Quota};
    ///
    /// assert_eq!(GcraParams::new(Quota::per_second(7)).quota().limit(), 7);
    /// ```
    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.quota
    }

    /// Whether the quota admits nothing.
    #[inline]
    const fn closed(&self) -> bool {
        self.per_ns & RECIPROCAL_MASK == 0
    }

    /// `k`, where one emission interval is `2^k` units of the time line. Kept
    /// in the otherwise unused top bits of `per_ns`, so the struct stays one
    /// cache line.
    #[inline]
    #[allow(clippy::cast_possible_truncation, reason = "the shift is at most 30")]
    const fn shift(&self) -> u32 {
        (self.per_ns >> SHIFT_BITS_AT) as u32
    }

    /// `units` admitted units in time-line units. `units` is at most a `u32`
    /// burst and the shift at most 30, so this never overflows.
    #[inline]
    const fn cost(&self, units: u64) -> u64 {
        units << self.shift()
    }

    /// `floor(now_ns * limit / period_ns)`: the current time in time-line
    /// units.
    ///
    /// The reciprocal estimate is low by at most one (`per_ns` is rounded down,
    /// and the error term `now_ns / 2^64` is below one), so one exact multiply
    /// comparison corrects it. With `limit <= period_ns` the result is at most
    /// `now_ns`, so it always fits.
    fn intervals_at(&self, now_ns: u64) -> u64 {
        let wide = u128::from(now_ns);
        let estimate = ((wide * (self.per_ns & RECIPROCAL_MASK)) >> 64) as u64;
        let next = u128::from(estimate) + 1;
        if next * u128::from(self.period_ns) <= wide * u128::from(self.limit) {
            estimate + 1
        } else {
            estimate
        }
    }

    /// The first nanosecond at which time-line unit `interval` has begun:
    /// `ceil(interval * period_ns / limit)`. Off the hot path (denials only).
    fn nanos_at(&self, interval: u64) -> u64 {
        let scaled = u128::from(interval) * u128::from(self.period_ns);
        let ns = scaled.div_ceil(u128::from(self.limit));
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// Checks a single unit against `tat` at time `now`. Equivalent to
    /// `check_n_at(tat, 1, now)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::sync::atomic::AtomicU64;
    /// use core::time::Duration;
    /// use rate_net::{GcraParams, Quota};
    ///
    /// let params = GcraParams::new(Quota::per_second(1));
    /// let tat = AtomicU64::new(0);
    /// assert!(params.check_at(&tat, Duration::ZERO).is_allow());
    /// assert!(params.check_at(&tat, Duration::ZERO).is_deny());
    /// assert!(params.check_at(&tat, Duration::from_secs(1)).is_allow());
    /// ```
    #[inline]
    pub fn check_at(&self, tat: &AtomicU64, now: Duration) -> Decision {
        self.check_n_in(tat, 1, now)
    }

    /// Checks `n` units against `tat` at time `now`, all or nothing. On
    /// admission the word is advanced by `n` intervals; on denial it is left
    /// as it was.
    ///
    /// Units admitted one after another are spaced one emission interval
    /// apart once the burst is used, to within one unit of the time line:
    /// under 2 ns, or `1/2^30` of the interval for intervals longer than
    /// about one second.
    ///
    /// Requesting `0` always succeeds; requesting more than the burst can never
    /// succeed, and the denial's `retry_after` is [`Duration::MAX`]. A denial
    /// otherwise carries the exact wait until the request conforms.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::sync::atomic::AtomicU64;
    /// use core::time::Duration;
    /// use rate_net::{GcraParams, Quota};
    ///
    /// let params = GcraParams::new(Quota::per_second(10));
    /// let tat = AtomicU64::new(0);
    /// assert!(params.check_n_at(&tat, 10, Duration::ZERO).is_allow());
    /// assert_eq!(
    ///     params.check_n_at(&tat, 11, Duration::ZERO).retry_after(),
    ///     Some(Duration::MAX),
    /// );
    /// // The next unit conforms one emission interval (100 ms) later.
    /// assert_eq!(
    ///     params.check_n_at(&tat, 1, Duration::ZERO).retry_after(),
    ///     Some(Duration::from_millis(100)),
    /// );
    /// ```
    #[inline]
    pub fn check_n_at(&self, tat: &AtomicU64, n: u32, now: Duration) -> Decision {
        self.check_n_in(tat, n, now)
    }

    /// Reads the allowance in `tat` at time `now` without consuming any: the
    /// burst, the units that would conform right now, and the wait until the
    /// bucket has drained back to its full burst.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::sync::atomic::AtomicU64;
    /// use core::time::Duration;
    /// use rate_net::{GcraParams, Quota};
    ///
    /// let params = GcraParams::new(Quota::per_second(10));
    /// let tat = AtomicU64::new(0);
    /// assert!(params.check_n_at(&tat, 4, Duration::ZERO).is_allow());
    /// let snapshot = params.peek_at(&tat, Duration::ZERO);
    /// assert_eq!(snapshot.remaining(), 6);
    /// assert_eq!(snapshot.reset_after(), Duration::from_millis(400));
    /// ```
    #[must_use]
    pub fn peek_at(&self, tat: &AtomicU64, now: Duration) -> Snapshot {
        self.peek_in(tat, now)
    }

    /// Restores the full burst in `tat` immediately, by storing `0`.
    ///
    /// The same as `tat.store(0, Ordering::Release)`; it is here so a reset
    /// reads as part of the limiter rather than as a magic number.
    ///
    /// # Examples
    ///
    /// ```
    /// use core::sync::atomic::AtomicU64;
    /// use core::time::Duration;
    /// use rate_net::{GcraParams, Quota};
    ///
    /// let params = GcraParams::new(Quota::per_second(2));
    /// let tat = AtomicU64::new(0);
    /// assert!(params.check_n_at(&tat, 2, Duration::ZERO).is_allow());
    /// params.reset(&tat);
    /// assert!(params.check_n_at(&tat, 2, Duration::ZERO).is_allow());
    /// ```
    // A method rather than an associated function so a reset is spelled on
    // the parameters like every other operation on the word.
    #[allow(clippy::unused_self)]
    pub fn reset(&self, tat: &AtomicU64) {
        tat.store(0, Ordering::Release);
    }

    /// [`check_n_at`](Self::check_n_at) over any [`Word`].
    #[inline]
    fn check_n_in<W: Word>(&self, tat: &W, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        // A request wider than the burst can never conform, even from an
        // empty bucket.
        if self.closed() || n > self.quota.burst() {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }
        let span = self.cost(u64::from(n - 1));

        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let ceiling = now_iv.saturating_add(self.tolerance);
        loop {
            let current = tat.load(Ordering::Acquire);
            // A late thread (stale `now`) only ever sees a later TAT here, which
            // makes it stricter, never looser.
            let effective = current.max(now_iv);
            // Conforms iff `effective + span <= now + tolerance`.
            if effective.saturating_add(span) <= ceiling {
                let new_tat = effective.saturating_add(self.cost(u64::from(n)));
                if tat
                    .compare_exchange_weak(current, new_tat, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Decision::Allow;
                }
                core::hint::spin_loop();
                continue;
            }
            // Denied: the request conforms once `now` reaches this interval.
            let ready_iv = effective.saturating_add(span) - self.tolerance;
            let ready_ns = self.nanos_at(ready_iv);
            return Decision::Deny {
                retry_after: Duration::from_nanos(ready_ns.saturating_sub(now_ns).max(1)),
            };
        }
    }

    /// [`peek_at`](Self::peek_at) over any [`Word`].
    fn peek_in<W: Word>(&self, tat: &W, now: Duration) -> Snapshot {
        if self.closed() {
            return Snapshot::closed();
        }
        let burst = self.quota.burst();
        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let tat = tat.load(Ordering::Acquire).max(now_iv);
        // `k` units conform iff `tat + (k - 1) * unit <= now + tolerance`.
        let unit = self.cost(1);
        let remaining = now_iv
            .saturating_add(self.tolerance)
            .saturating_add(unit)
            .saturating_sub(tat)
            >> self.shift();
        let reset_after = if tat == now_iv {
            Duration::ZERO
        } else {
            Duration::from_nanos(self.nanos_at(tat).saturating_sub(now_ns))
        };
        Snapshot::new(
            burst,
            u32::try_from(remaining).unwrap_or(u32::MAX).min(burst),
            reset_after,
        )
    }

    /// Whether `tat` holds the whole burst at `now`: the theoretical arrival
    /// time is not past the current interval. The same answer as a
    /// [`peek_at`](Self::peek_at) with nothing spent, without its division.
    #[cfg(all(feature = "std", feature = "algorithms"))]
    fn is_full_in<W: Word>(&self, tat: &W, now: Duration) -> bool {
        self.closed() || tat.load(Ordering::Acquire) <= self.intervals_at(nanos(now))
    }

    /// Makes `tat` no fuller than a bucket that is whole again exactly at
    /// `until` (a duration on the caller's time line): the theoretical arrival
    /// time is raised to the first time-line unit that begins at or after it.
    /// Exact, with no rounding of fractional units, so a key restored
    /// repeatedly loses nothing.
    ///
    /// Never past a bucket drained empty at `now`: a debt that would take
    /// longer than this quota's own full refill (one recorded under another,
    /// slower quota) is cut to that refill, so a restore holds a key back for
    /// at most its own recovery time.
    #[cfg(all(feature = "std", feature = "algorithms"))]
    fn restore_until_in<W: Word>(&self, tat: &W, until: Duration, now: Duration) {
        if self.closed() {
            return;
        }
        let until_ns = nanos(until);
        let floor = self.intervals_at(until_ns);
        let interval = if self.nanos_at(floor) >= until_ns {
            floor
        } else {
            floor.saturating_add(1)
        };
        let drained = self
            .intervals_at(nanos(now))
            .saturating_add(self.tolerance)
            .saturating_add(self.cost(1));
        let _previous = tat.fetch_max(interval.min(drained), Ordering::AcqRel);
    }
}

impl fmt::Debug for GcraParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcraParams")
            .field("quota", &self.quota)
            .finish_non_exhaustive()
    }
}

/// A lock-free, clock-free, single-key leaky bucket (GCRA), usable in `no_std`.
///
/// One `Gcra` limits one stream: there is no key, no map, no hash, and no lock;
/// a check is a few multiplies and one compare-and-swap. Time is passed in by
/// the caller on every call as a [`Duration`] since any fixed origin, so the
/// type needs no clock and no standard library: it is what the crate offers
/// with the `std` feature off. With `std` on, prefer
/// `DirectLimiter`, which reads a clock for you and can run any algorithm.
///
/// A `Gcra` is one theoretical arrival time plus its [`GcraParams`]. To limit
/// many streams under one quota from your own table, share one `GcraParams`
/// and keep only an [`AtomicU64`] per stream.
///
/// Times must come from one monotonic source. A reading earlier than one
/// already used (a late thread, or a source that steps back) is handled
/// conservatively: it can only make a decision stricter, never admit more.
///
/// Available on every target with 64-bit atomics.
///
/// # Quota semantics
///
/// `limit` units per `period`, with bursts of up to `burst` units at once. The
/// bucket starts full. A rate finer than one unit per nanosecond is clamped to
/// it, and a zero limit, period, or burst admits nothing; use
/// [`try_new`](Self::try_new) to reject those instead.
///
/// # Examples
///
/// ```
/// use rate_net::{Decision, Gcra, Quota};
/// use core::time::Duration;
///
/// // 10 per second, bursts of 10.
/// let gcra = Gcra::new(Quota::per_second(10));
/// let t0 = Duration::ZERO;
///
/// for _ in 0..10 {
///     assert!(gcra.check_at(t0).is_allow());
/// }
/// // The 11th unit conforms one emission interval (100 ms) later.
/// assert_eq!(gcra.check_at(t0).retry_after(), Some(Duration::from_millis(100)));
/// assert_eq!(gcra.check_at(Duration::from_millis(100)), Decision::Allow);
/// ```
pub struct Gcra {
    /// Theoretical arrival time of the next conforming unit, in time-line
    /// units since the limiter epoch. Loom's atomic under `cfg(loom)`.
    tat: crate::sync::AtomicU64,
    /// The quota's parameters; all of the arithmetic lives there.
    params: GcraParams,
}

impl Gcra {
    /// A GCRA limiter for `quota`, starting full. Infallible and fail-closed: a
    /// zero limit, period, or burst admits nothing, and a rate finer than one
    /// unit per nanosecond is clamped to it.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    /// use core::time::Duration;
    ///
    /// let gcra = Gcra::new(Quota::per_second(0));
    /// assert!(gcra.check_at(Duration::ZERO).is_deny());
    /// ```
    #[must_use]
    pub fn new(quota: Quota) -> Self {
        Self {
            // Interval 0 is at or before any `now`, so a fresh key is full.
            tat: crate::sync::AtomicU64::new(0),
            params: GcraParams::new(quota),
        }
    }

    /// A GCRA limiter for `quota`, validated.
    ///
    /// # Errors
    ///
    /// - [`RateLimiterError::ZeroQuota`]: the limit is `0`.
    /// - [`RateLimiterError::ZeroPeriod`]: the period is zero.
    /// - [`RateLimiterError::ZeroBurst`]: the burst is `0`.
    /// - [`RateLimiterError::RateTooHigh`]: more than one unit per nanosecond.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota, RateLimiterError};
    ///
    /// assert!(Gcra::try_new(Quota::per_second(100)).is_ok());
    /// assert_eq!(
    ///     Gcra::try_new(Quota::per_second(100).with_burst(0)).unwrap_err(),
    ///     RateLimiterError::ZeroBurst,
    /// );
    /// ```
    pub fn try_new(quota: Quota) -> Result<Self, RateLimiterError> {
        validate(&quota)?;
        Ok(Self::new(quota))
    }

    /// The quota this limiter was built from, as configured.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    ///
    /// assert_eq!(Gcra::new(Quota::per_second(7)).quota().limit(), 7);
    /// ```
    #[must_use]
    pub const fn quota(&self) -> Quota {
        self.params.quota
    }

    /// The parameters this limiter runs on: the same [`GcraParams`] that
    /// [`GcraParams::new`] builds from [`quota`](Self::quota).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, GcraParams, Quota};
    ///
    /// let quota = Quota::per_second(7);
    /// assert_eq!(*Gcra::new(quota).params(), GcraParams::new(quota));
    /// ```
    #[must_use]
    pub const fn params(&self) -> &GcraParams {
        &self.params
    }

    /// Checks a single unit at time `now`. Equivalent to
    /// `check_n_at(1, now)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    /// use core::time::Duration;
    ///
    /// let gcra = Gcra::new(Quota::per_second(1));
    /// assert!(gcra.check_at(Duration::ZERO).is_allow());
    /// assert!(gcra.check_at(Duration::ZERO).is_deny());
    /// assert!(gcra.check_at(Duration::from_secs(1)).is_allow());
    /// ```
    #[inline]
    pub fn check_at(&self, now: Duration) -> Decision {
        self.check_n_at(1, now)
    }

    /// Checks `n` units at time `now`, all or nothing.
    ///
    /// Requesting `0` always succeeds; requesting more than the burst can never
    /// succeed, and the denial's `retry_after` is [`Duration::MAX`]. A denial
    /// otherwise carries the exact wait until the request conforms.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    /// use core::time::Duration;
    ///
    /// let gcra = Gcra::new(Quota::per_second(10));
    /// assert!(gcra.check_n_at(10, Duration::ZERO).is_allow());
    /// assert_eq!(
    ///     gcra.check_n_at(11, Duration::ZERO).retry_after(),
    ///     Some(Duration::MAX),
    /// );
    /// ```
    #[inline]
    pub fn check_n_at(&self, n: u32, now: Duration) -> Decision {
        self.params.check_n_in(&self.tat, n, now)
    }

    /// Reads the allowance at time `now` without consuming any: the burst, the
    /// units that would conform right now, and the wait until the bucket has
    /// drained back to its full burst.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    /// use core::time::Duration;
    ///
    /// let gcra = Gcra::new(Quota::per_second(10));
    /// assert!(gcra.check_n_at(4, Duration::ZERO).is_allow());
    /// let snapshot = gcra.peek_at(Duration::ZERO);
    /// assert_eq!(snapshot.remaining(), 6);
    /// assert_eq!(snapshot.reset_after(), Duration::from_millis(400));
    /// ```
    #[must_use]
    pub fn peek_at(&self, now: Duration) -> Snapshot {
        self.params.peek_in(&self.tat, now)
    }

    /// Restores the full burst immediately.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::{Gcra, Quota};
    /// use core::time::Duration;
    ///
    /// let gcra = Gcra::new(Quota::per_second(2));
    /// assert!(gcra.check_n_at(2, Duration::ZERO).is_allow());
    /// gcra.reset();
    /// assert!(gcra.check_n_at(2, Duration::ZERO).is_allow());
    /// ```
    pub fn reset(&self) {
        self.tat.store(0, Ordering::Release);
    }

    /// Whether the bucket holds its whole burst at `now`. See
    /// [`GcraParams::is_full_in`].
    #[cfg(all(feature = "std", feature = "algorithms"))]
    pub(crate) fn is_full_at(&self, now: Duration) -> bool {
        self.params.is_full_in(&self.tat, now)
    }

    /// Makes the bucket no fuller than one that is whole again exactly at
    /// `until` (a duration on the caller's time line), and never emptier than
    /// one drained at `now`. See [`GcraParams::restore_until_in`].
    #[cfg(all(feature = "std", feature = "algorithms"))]
    pub(crate) fn restore_until(&self, until: Duration, now: Duration) {
        self.params.restore_until_in(&self.tat, until, now);
    }
}

impl fmt::Debug for Gcra {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gcra")
            .field("quota", &self.params.quota)
            .finish_non_exhaustive()
    }
}

/// The strict checks behind [`Gcra::try_new`] and [`GcraParams::try_new`],
/// shared with the keyed builder.
pub(crate) fn validate(quota: &Quota) -> Result<(), RateLimiterError> {
    if quota.limit() == 0 {
        return Err(RateLimiterError::ZeroQuota);
    }
    if quota.period().is_zero() {
        return Err(RateLimiterError::ZeroPeriod);
    }
    if quota.burst() == 0 {
        return Err(RateLimiterError::ZeroBurst);
    }
    // One emission interval must be at least a nanosecond.
    if u128::from(quota.limit()) > quota.period().as_nanos() {
        return Err(RateLimiterError::RateTooHigh);
    }
    Ok(())
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{Gcra, GcraParams};
    use crate::decision::Decision;
    use crate::error::RateLimiterError;
    use crate::quota::Quota;
    use crate::snapshot::Snapshot;
    use core::sync::atomic::AtomicU64;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_admits_burst_then_spaces() {
        // 10 per second, burst 10 → one unit every 100ms after the burst.
        let lb = Gcra::new(Quota::per_second(10));
        for _ in 0..10 {
            assert_eq!(lb.check_n_at(1, at(0)), Decision::Allow);
        }
        // 11th at the same instant is denied.
        assert!(lb.check_n_at(1, at(0)).is_deny());
        // After 100ms exactly one more conforms.
        assert_eq!(lb.check_n_at(1, at(100)), Decision::Allow);
        assert!(lb.check_n_at(1, at(100)).is_deny());
        assert_eq!(lb.check_n_at(1, at(200)), Decision::Allow);
    }

    #[test]
    fn test_retry_after_points_at_next_conforming_instant() {
        let lb = Gcra::new(Quota::per_second(10));
        for _ in 0..10 {
            assert!(lb.check_n_at(1, at(0)).is_allow());
        }
        // The next unit conforms exactly 100ms out.
        assert_eq!(lb.check_n_at(1, at(0)).retry_after(), Some(at(100)));
        // Waiting exactly that long is enough.
        assert!(lb.check_n_at(1, at(100)).is_allow());
    }

    #[test]
    fn test_retry_after_is_honest_for_non_integral_intervals() {
        // 3 per second: the interval is 333_333_333.3ns, not a whole number.
        let lb = Gcra::new(Quota::per_second(3).with_burst(1));
        assert!(lb.check_n_at(1, Duration::ZERO).is_allow());
        let wait = lb
            .check_n_at(1, Duration::ZERO)
            .retry_after()
            .unwrap_or_default();
        assert_eq!(wait, Duration::from_nanos(333_333_334));
        assert!(
            lb.check_n_at(1, wait.saturating_sub(Duration::from_nanos(1)))
                .is_deny()
        );
        assert!(lb.check_n_at(1, wait).is_allow());
    }

    #[test]
    fn test_request_larger_than_burst_never_conforms() {
        let lb = Gcra::new(Quota::per_second(5));
        assert_eq!(
            lb.check_n_at(6, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }

    #[test]
    fn test_zero_limit_denies() {
        let lb = Gcra::new(Quota::per_second(0));
        assert!(lb.check_n_at(1, at(0)).is_deny());
    }

    #[test]
    fn test_zero_burst_and_zero_period_deny() {
        let lb = Gcra::new(Quota::per_second(5).with_burst(0));
        assert_eq!(lb.check_n_at(1, at(0)).retry_after(), Some(Duration::MAX));
        let lb = Gcra::new(Quota::from_parts(5, Duration::ZERO, 5));
        assert_eq!(lb.check_n_at(1, at(0)).retry_after(), Some(Duration::MAX));
    }

    #[test]
    fn test_peek_reports_conforming_units_and_drain_time() {
        let lb = Gcra::new(Quota::per_second(10));
        assert_eq!(lb.peek_at(at(0)), Snapshot::full(10));
        assert!(lb.check_n_at(4, at(0)).is_allow());
        let snapshot = lb.peek_at(at(0));
        assert_eq!(snapshot.remaining(), 6);
        // Four intervals of 100ms to drain back to empty.
        assert_eq!(snapshot.reset_after(), at(400));
        assert_eq!(lb.peek_at(at(150)).remaining(), 7);
        assert_eq!(lb.peek_at(at(400)), Snapshot::full(10));
    }

    #[test]
    fn test_reset_restores_the_full_burst() {
        let lb = Gcra::new(Quota::per_second(3));
        assert!(lb.check_n_at(3, at(0)).is_allow());
        assert!(lb.check_n_at(1, at(0)).is_deny());
        lb.reset();
        assert!(lb.check_n_at(3, at(0)).is_allow());
        assert_eq!(
            Gcra::new(Quota::per_second(0)).peek_at(at(0)),
            Snapshot::closed()
        );
    }

    #[test]
    fn test_try_new_validates_and_quota_round_trips() {
        assert_eq!(
            Gcra::try_new(Quota::per_second(5)).map(|g| g.quota()),
            Ok(Quota::per_second(5))
        );
        assert_eq!(
            Gcra::try_new(Quota::per_second(0)).err(),
            Some(RateLimiterError::ZeroQuota)
        );
        assert_eq!(
            Gcra::try_new(Quota::from_parts(5, Duration::ZERO, 5)).err(),
            Some(RateLimiterError::ZeroPeriod)
        );
        assert_eq!(
            Gcra::try_new(Quota::per_second(5).with_burst(0)).err(),
            Some(RateLimiterError::ZeroBurst)
        );
        assert_eq!(
            Gcra::try_new(Quota::per_second(1_000_000_001)).err(),
            Some(RateLimiterError::RateTooHigh)
        );
        assert!(Gcra::try_new(Quota::per_second(1_000_000_000)).is_ok());
    }

    #[test]
    fn test_check_at_and_debug() {
        let gcra = Gcra::new(Quota::per_second(1));
        assert!(gcra.check_at(at(0)).is_allow());
        assert!(gcra.check_at(at(0)).is_deny());
        assert!(format!("{gcra:?}").contains("Gcra"));
    }

    #[test]
    fn test_zero_units_always_admitted() {
        let lb = Gcra::new(Quota::per_second(1));
        assert_eq!(lb.check_n_at(0, at(0)), Decision::Allow);
    }

    #[test]
    fn test_rate_above_one_per_ns_is_clamped_not_unlimited() {
        // 2e9 per second used to truncate the interval to zero and admit
        // everything. Clamped to one per nanosecond, a frozen clock admits
        // exactly the burst.
        let lb = Gcra::new(Quota::per_second(2_000_000_000).with_burst(1));
        let admitted = (0..10_000)
            .filter(|_| lb.check_n_at(1, at(5)).is_allow())
            .count();
        assert_eq!(admitted, 1);
    }

    #[test]
    #[cfg_attr(miri, ignore = "a million checks is too slow under Miri")]
    fn test_high_rate_does_not_over_admit_from_truncation() {
        // 6e8 per second (interval 1.67ns) used to round down to 1ns and admit
        // ~1.67x the rate. Over 1ms it must admit at most burst + 600_000.
        let lb = Gcra::new(Quota::per_second(600_000_000).with_burst(1));
        let mut admitted = 0u64;
        for ns in 0..1_000_000u64 {
            if lb.check_n_at(1, Duration::from_nanos(ns)).is_allow() {
                admitted += 1;
            }
        }
        assert!(admitted <= 600_001, "admitted {admitted}");
        assert!(admitted >= 599_999, "under-admitted {admitted}");
    }

    #[test]
    fn test_interval_conversion_is_exact_floor() {
        // 3 per second: an interval of 333_333_333.3 ns splits into 2^28
        // time-line units (the largest power of two keeping a unit >= 1 ns).
        let lb = Gcra::new(Quota::per_second(3));
        assert_eq!(lb.params.shift(), 28);
        let per_period: u128 = 3 << 28;
        for ns in [
            0u64,
            1,
            333_333_333,
            333_333_334,
            999_999_999,
            1_000_000_000,
            u64::MAX,
        ] {
            let exact =
                u64::try_from(u128::from(ns) * per_period / 1_000_000_000).unwrap_or(u64::MAX);
            assert_eq!(lb.params.intervals_at(ns), exact, "ns={ns}");
        }
        let one_per_ns = Gcra::new(Quota::from_parts(1_000, Duration::from_micros(1), 1));
        assert_eq!(one_per_ns.params.shift(), 0);
        assert_eq!(one_per_ns.params.intervals_at(u64::MAX), u64::MAX);
        // Long intervals stop at 2^30 units.
        assert_eq!(Gcra::new(Quota::per_minute(1)).params.shift(), 30);
    }

    #[test]
    fn test_burst_one_spacing_holds_across_an_interval_boundary() {
        // 2.1 floored `now` to whole intervals and admitted at 1.999 s and
        // 2.000 s with burst 1: two units 1 ms apart.
        let lb = Gcra::new(Quota::per_second(1).with_burst(1));
        assert!(lb.check_at(at(0)).is_allow());
        assert!(lb.check_at(at(1_999)).is_allow());
        assert!(lb.check_at(at(2_000)).is_deny());
        assert!(lb.check_at(at(2_998)).is_deny());
        assert!(lb.check_at(at(2_999)).is_allow());

        // No window shorter than one interval (100 ms) ever holds more than
        // the burst, polled at uneven times.
        for burst in [1u32, 3, 10] {
            let lb = Gcra::new(Quota::per_second(10).with_burst(burst));
            let mut admitted = Vec::new();
            for step in 0..5_000u64 {
                let ms = step * 7 / 3;
                if lb.check_at(at(ms)).is_allow() {
                    admitted.push(ms);
                }
            }
            let width = usize::try_from(burst).unwrap_or(1) + 1;
            for w in admitted.windows(width) {
                let first = w.first().copied().unwrap_or(0);
                let last = w.last().copied().unwrap_or(0);
                assert!(
                    last - first >= 100,
                    "burst {burst}: {width} units within {} ms",
                    last - first
                );
            }
        }
    }

    #[test]
    #[cfg(all(feature = "std", feature = "algorithms"))]
    fn test_restore_never_holds_back_past_own_refill() {
        // A debt recorded under a slower quota (an hour) restored into a
        // one-per-100ms bucket of burst 10: the bucket is drained at most,
        // and whole again after its own one-second refill.
        let lb = Gcra::new(Quota::per_second(10));
        lb.restore_until(Duration::from_secs(3_600), at(5_000));
        assert!(lb.check_at(at(5_000)).is_deny());
        assert!(lb.check_at(at(5_100)).is_allow());
        // Drained at 5 s, one unit taken at 5.1 s: whole again at 6.1 s.
        assert_eq!(lb.peek_at(at(6_100)), Snapshot::full(10));
    }

    #[test]
    fn test_params_share_one_quota_across_independent_words() {
        let params = GcraParams::new(Quota::per_second(10).with_burst(2));
        let (a, b) = (AtomicU64::new(0), AtomicU64::new(0));
        assert!(params.check_n_at(&a, 2, at(0)).is_allow());
        assert!(params.check_at(&a, at(0)).is_deny());
        // `b` is untouched by `a`'s traffic.
        assert!(params.check_n_at(&b, 2, at(0)).is_allow());
        assert_eq!(params.peek_at(&a, at(0)).remaining(), 0);
        assert_eq!(params.peek_at(&a, at(100)).remaining(), 1);
    }

    #[test]
    fn test_params_denial_leaves_the_word_unchanged() {
        let params = GcraParams::new(Quota::per_second(10));
        let tat = AtomicU64::new(0);
        assert!(params.check_n_at(&tat, 10, at(0)).is_allow());
        let before = tat.load(core::sync::atomic::Ordering::Relaxed);
        assert!(params.check_at(&tat, at(0)).is_deny());
        assert!(params.check_n_at(&tat, 11, at(0)).is_deny());
        assert_eq!(tat.load(core::sync::atomic::Ordering::Relaxed), before);
    }

    #[test]
    fn test_params_zero_quotas_fail_closed() {
        for quota in [
            Quota::per_second(0),
            Quota::per_second(5).with_burst(0),
            Quota::from_parts(5, Duration::ZERO, 5),
        ] {
            let params = GcraParams::new(quota);
            let tat = AtomicU64::new(0);
            assert_eq!(
                params.check_at(&tat, at(0)).retry_after(),
                Some(Duration::MAX)
            );
            assert_eq!(params.peek_at(&tat, at(0)), Snapshot::closed());
            assert_eq!(params.check_n_at(&tat, 0, at(0)), Decision::Allow);
            assert_eq!(tat.load(core::sync::atomic::Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn test_params_reset_try_new_quota_and_debug() {
        let params = GcraParams::new(Quota::per_second(3));
        let tat = AtomicU64::new(0);
        assert!(params.check_n_at(&tat, 3, at(0)).is_allow());
        params.reset(&tat);
        assert!(params.check_n_at(&tat, 3, at(0)).is_allow());
        assert_eq!(params.quota(), Quota::per_second(3));
        assert_eq!(
            GcraParams::try_new(Quota::per_second(3)),
            Ok(GcraParams::new(Quota::per_second(3)))
        );
        assert_eq!(
            GcraParams::try_new(Quota::per_second(1_000_000_001)),
            Err(RateLimiterError::RateTooHigh)
        );
        assert!(format!("{params:?}").contains("GcraParams"));
        let gcra = Gcra::new(Quota::per_second(3));
        assert_eq!(*gcra.params(), params);
    }

    #[test]
    fn test_gcra_stays_eighty_bytes() {
        // The keyed leaky-bucket store holds one `Gcra` per key: splitting
        // the parameters out must not grow it. The closed quota is marked by
        // a zero reciprocal rather than a flag for this reason.
        assert!(core::mem::size_of::<Gcra>() <= 80);
        assert!(core::mem::size_of::<GcraParams>() <= 64);
    }
}
