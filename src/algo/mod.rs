//! Per-key algorithm state.
//!
//! Each algorithm's per-key state is its own type implementing [`KeyState`]:
//! the token bucket is [`better_bucket`]'s clock-free [`BucketCore`]; the leaky
//! bucket ([`Gcra`]) and the window algorithms are compiled in under the
//! `algorithms` feature. The keyed store is generic over the state type, so a
//! stored key holds exactly its algorithm's state — no enum tag, no padding to
//! a tag's alignment — and the algorithm is chosen once per limiter, not per
//! check. [`AlgoState`] is the tagged form for the single-state
//! [`DirectLimiter`](crate::DirectLimiter).
//!
//! Every algorithm is driven by the same `now` — the limiter's elapsed time,
//! read once per check — so per-key state holds no clock, and the store can
//! judge any key's state at one instant (eviction asks whether a key has
//! recovered without reading the clock again).

use core::time::Duration;

use better_bucket::{BucketConfig, BucketCore};

use crate::algorithm::Algorithm;
use crate::decision::Decision;
use crate::quota::Quota;
use crate::snapshot::Snapshot;

#[cfg(feature = "algorithms")]
mod fixed_window;
#[cfg(feature = "algorithms")]
mod sliding_counter;
#[cfg(feature = "algorithms")]
mod sliding_log;

#[cfg(feature = "algorithms")]
pub(crate) use self::fixed_window::FixedWindow;
#[cfg(feature = "algorithms")]
pub(crate) use self::sliding_counter::SlidingCounter;
#[cfg(feature = "algorithms")]
pub(crate) use self::sliding_log::SlidingLog;
#[cfg(feature = "algorithms")]
use crate::gcra::Gcra;

/// What an evicted key still owed when it was forgotten: `spent` units of its
/// capacity were unavailable, and all of them would have been back by the
/// elapsed time `until`. Enough to re-create the key no more permissive than it
/// was (see [`KeyState::restore`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Debt {
    pub(crate) spent: u32,
    pub(crate) until: Duration,
}

/// One key's state under one algorithm.
pub(crate) trait KeyState: Send + Sync + Sized + 'static {
    /// Fresh state with its whole allowance as of `now`, or `None` for a quota
    /// that admits nothing (such a key is never stored: it fails closed).
    fn create(quota: &Quota, now: Duration) -> Option<Self>;

    /// Checks `n` units as of `now`.
    fn acquire(&self, n: u32, now: Duration) -> Decision;

    /// The remaining allowance as of `now`, without consuming any.
    fn peek(&self, now: Duration) -> Snapshot;

    /// Restores the whole allowance as of `now`.
    fn reset(&self, now: Duration);

    /// What the key owes as of `now`, or `None` if it has fully recovered (so
    /// forgetting it loses nothing).
    fn debt(&self, now: Duration) -> Option<Debt> {
        let snapshot = self.peek(now);
        let spent = snapshot.limit() - snapshot.remaining();
        (spent > 0).then(|| Debt {
            spent,
            until: now.saturating_add(snapshot.reset_after()),
        })
    }

    /// Whether the key's state is indistinguishable from a fresh key's.
    fn is_full(&self, now: Duration) -> bool {
        self.debt(now).is_none()
    }

    /// The units still owed at `now` on `debt`. The window algorithms owe the
    /// whole `spent` count until `debt.until`; the buckets override this with
    /// the time left converted at their refill rate.
    fn owed(&self, debt: Debt, _now: Duration) -> u32 {
        debt.spent
    }

    /// Charges a *fresh* state so it is no more permissive than a key that
    /// owed `debt` — the state of a key that was evicted before it recovered
    /// and has now returned.
    ///
    /// The buckets are exact to within one unit: the outstanding debt is the
    /// time left until `debt.until`, converted at the key's refill rate and
    /// rounded up. The window algorithms charge the whole `spent` count at
    /// `now`, which is never looser than the original (whose units were
    /// admitted earlier and so expire no later) but may hold the allowance
    /// back for up to one window after `debt.until`.
    fn restore(&self, debt: Debt, now: Duration) {
        if debt.until <= now || debt.spent == 0 {
            return;
        }
        let owed = self.owed(debt, now).min(self.peek(now).limit());
        if owed > 0 {
            // A fresh state holds its whole capacity, so this always admits.
            let _charged = self.acquire(owed, now);
        }
    }
}

impl KeyState for BucketCore {
    fn create(quota: &Quota, now: Duration) -> Option<Self> {
        let burst = quota.burst();
        // Only a zero limit, burst, or period is rejected: such a key admits
        // nothing.
        BucketConfig::new(burst, quota.limit(), quota.period(), burst)
            .ok()
            .map(|config| Self::new(config, now))
    }

    #[inline]
    fn acquire(&self, n: u32, now: Duration) -> Decision {
        self.acquire_at(n, now).into()
    }

    fn peek(&self, now: Duration) -> Snapshot {
        let capacity = self.capacity();
        let reset_after = self.time_until_at(capacity, now).unwrap_or(Duration::MAX);
        Snapshot::new(capacity, self.available_at(now), reset_after)
    }

    fn reset(&self, now: Duration) {
        self.reset_at(now);
    }

    /// Recovered means every token is back: one read of the balance, without
    /// computing the refill time `peek` also reports.
    fn is_full(&self, now: Duration) -> bool {
        self.available_at(now) >= self.capacity()
    }

    fn owed(&self, debt: Debt, now: Duration) -> u32 {
        let config = self.config();
        owed_units(debt, now, config.refill_amount(), config.refill_period())
    }
}

#[cfg(feature = "algorithms")]
impl KeyState for Gcra {
    fn create(quota: &Quota, _now: Duration) -> Option<Self> {
        admits_anything(Algorithm::LeakyBucket, quota).then(|| Self::new(*quota))
    }

    #[inline]
    fn acquire(&self, n: u32, now: Duration) -> Decision {
        self.check_n_at(n, now)
    }

    fn peek(&self, now: Duration) -> Snapshot {
        self.peek_at(now)
    }

    fn reset(&self, _now: Duration) {
        Self::reset(self);
    }

    fn owed(&self, debt: Debt, now: Duration) -> u32 {
        let quota = self.quota();
        owed_units(debt, now, quota.limit(), quota.period())
    }
}

#[cfg(feature = "algorithms")]
impl KeyState for FixedWindow {
    fn create(quota: &Quota, now: Duration) -> Option<Self> {
        admits_anything(Algorithm::FixedWindow, quota).then(|| Self::new(quota, now))
    }

    #[inline]
    fn acquire(&self, n: u32, now: Duration) -> Decision {
        Self::acquire(self, n, now)
    }

    fn peek(&self, now: Duration) -> Snapshot {
        Self::peek(self, now)
    }

    fn reset(&self, _now: Duration) {
        Self::reset(self);
    }
}

#[cfg(feature = "algorithms")]
impl KeyState for SlidingLog {
    fn create(quota: &Quota, _now: Duration) -> Option<Self> {
        admits_anything(Algorithm::SlidingWindowLog, quota).then(|| Self::new(quota))
    }

    #[inline]
    fn acquire(&self, n: u32, now: Duration) -> Decision {
        Self::acquire(self, n, now)
    }

    fn peek(&self, now: Duration) -> Snapshot {
        Self::peek(self, now)
    }

    fn reset(&self, _now: Duration) {
        Self::reset(self);
    }
}

#[cfg(feature = "algorithms")]
impl KeyState for SlidingCounter {
    fn create(quota: &Quota, now: Duration) -> Option<Self> {
        admits_anything(Algorithm::SlidingWindowCounter, quota).then(|| Self::new(quota, now))
    }

    #[inline]
    fn acquire(&self, n: u32, now: Duration) -> Decision {
        Self::acquire(self, n, now)
    }

    fn peek(&self, now: Duration) -> Snapshot {
        Self::peek(self, now)
    }

    fn reset(&self, _now: Duration) {
        Self::reset(self);
    }
}

/// Whether `quota` admits anything at all under `algorithm`.
#[cfg_attr(not(feature = "algorithms"), allow(dead_code))]
fn admits_anything(algorithm: Algorithm, quota: &Quota) -> bool {
    fresh_snapshot(algorithm, quota).limit() > 0
}

/// Units a bucket refilling `amount` per `period` still owes at `now` on
/// `debt`: the time left until `debt.until` at that rate, rounded up, and never
/// more than was spent.
fn owed_units(debt: Debt, now: Duration, amount: u32, period: Duration) -> u32 {
    let period = period.as_nanos();
    if period == 0 {
        return 0;
    }
    let left = debt.until.saturating_sub(now).as_nanos();
    let units = left.saturating_mul(u128::from(amount)).div_ceil(period);
    u32::try_from(units).unwrap_or(u32::MAX).min(debt.spent)
}

/// The decision of a key that admits nothing: zero units always fit, any more
/// never will.
pub(crate) const fn closed(n: u32) -> Decision {
    if n == 0 {
        Decision::Allow
    } else {
        Decision::Deny {
            retry_after: Duration::MAX,
        }
    }
}

/// One state of whichever algorithm a [`DirectLimiter`](crate::DirectLimiter)
/// runs — the tagged form of [`KeyState`], for a limiter that holds just one.
pub(crate) enum AlgoState {
    /// A quota that admits nothing.
    Closed,
    TokenBucket(BucketCore),
    #[cfg(feature = "algorithms")]
    LeakyBucket(Gcra),
    #[cfg(feature = "algorithms")]
    FixedWindow(FixedWindow),
    #[cfg(feature = "algorithms")]
    SlidingLog(SlidingLog),
    #[cfg(feature = "algorithms")]
    SlidingCounter(SlidingCounter),
}

/// Runs `$body` with `$state` bound to the live state of an [`AlgoState`], or
/// evaluates `$closed` for [`AlgoState::Closed`].
macro_rules! dispatch {
    ($self:expr, $state:ident => $body:expr, closed => $closed:expr) => {
        match $self {
            AlgoState::Closed => $closed,
            AlgoState::TokenBucket($state) => $body,
            #[cfg(feature = "algorithms")]
            AlgoState::LeakyBucket($state) => $body,
            #[cfg(feature = "algorithms")]
            AlgoState::FixedWindow($state) => $body,
            #[cfg(feature = "algorithms")]
            AlgoState::SlidingLog($state) => $body,
            #[cfg(feature = "algorithms")]
            AlgoState::SlidingCounter($state) => $body,
        }
    };
}

impl AlgoState {
    /// Builds fresh state for `algorithm` under `quota`, with its whole
    /// allowance as of the elapsed time `now`.
    pub(crate) fn new(algorithm: Algorithm, quota: &Quota, now: Duration) -> Self {
        fn wrap<S: KeyState>(
            quota: &Quota,
            now: Duration,
            variant: impl FnOnce(S) -> AlgoState,
        ) -> AlgoState {
            S::create(quota, now).map_or(AlgoState::Closed, variant)
        }
        match algorithm {
            Algorithm::TokenBucket => wrap(quota, now, Self::TokenBucket),
            #[cfg(feature = "algorithms")]
            Algorithm::LeakyBucket => wrap(quota, now, Self::LeakyBucket),
            #[cfg(feature = "algorithms")]
            Algorithm::FixedWindow => wrap(quota, now, Self::FixedWindow),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowLog => wrap(quota, now, Self::SlidingLog),
            #[cfg(feature = "algorithms")]
            Algorithm::SlidingWindowCounter => wrap(quota, now, Self::SlidingCounter),
        }
    }

    /// Checks `n` units as of elapsed time `now`.
    #[inline]
    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        dispatch!(self, state => KeyState::acquire(state, n, now), closed => closed(n))
    }

    /// The remaining allowance as of `now`, without consuming any.
    pub(crate) fn peek(&self, now: Duration) -> Snapshot {
        dispatch!(self, state => KeyState::peek(state, now), closed => Snapshot::closed())
    }

    /// Restores the whole allowance as of `now`.
    pub(crate) fn reset(&self, now: Duration) {
        dispatch!(self, state => KeyState::reset(state, now), closed => {});
    }
}

/// What a key that has never been seen reports: its whole allowance under
/// `quota`, or nothing for a quota that admits nothing.
pub(crate) fn fresh_snapshot(algorithm: Algorithm, quota: &Quota) -> Snapshot {
    if quota.limit() == 0 || quota.period().is_zero() {
        return Snapshot::closed();
    }
    let capacity = match algorithm {
        Algorithm::TokenBucket => quota.burst(),
        #[cfg(feature = "algorithms")]
        Algorithm::LeakyBucket => quota.burst(),
        #[cfg(feature = "algorithms")]
        Algorithm::FixedWindow | Algorithm::SlidingWindowLog | Algorithm::SlidingWindowCounter => {
            quota.limit()
        }
    };
    if capacity == 0 {
        Snapshot::closed()
    } else {
        Snapshot::full(capacity)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use core::time::Duration;

    use better_bucket::BucketCore;

    use super::{AlgoState, Debt, KeyState, fresh_snapshot};
    use crate::algorithm::Algorithm;
    use crate::quota::Quota;
    use crate::snapshot::Snapshot;

    fn token(quota: Quota) -> BucketCore {
        BucketCore::create(&quota, Duration::ZERO).unwrap()
    }

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_token_bucket_peek_reset_and_fullness() {
        let state = token(Quota::per_second(10));
        assert_eq!(KeyState::peek(&state, Duration::ZERO), Snapshot::full(10));
        assert!(state.is_full(Duration::ZERO));
        assert!(KeyState::acquire(&state, 4, Duration::ZERO).is_allow());
        let snapshot = KeyState::peek(&state, Duration::ZERO);
        assert_eq!(snapshot.remaining(), 6);
        // Four tokens at 10/s take 400ms to come back.
        assert_eq!(snapshot.reset_after(), Duration::from_millis(400));
        assert!(!state.is_full(Duration::ZERO));
        KeyState::reset(&state, Duration::ZERO);
        assert!(state.is_full(Duration::ZERO));
        assert!(KeyState::acquire(&state, 10, Duration::ZERO).is_allow());
        assert!(state.is_full(ms(1_000)));
    }

    #[test]
    fn test_token_bucket_honours_burst_and_degenerate_quotas_are_never_created() {
        let state = token(Quota::per_second(10).with_burst(25));
        assert_eq!(KeyState::peek(&state, Duration::ZERO).limit(), 25);
        assert!(KeyState::acquire(&state, 25, Duration::ZERO).is_allow());
        for degenerate in [
            Quota::per_second(0),
            Quota::per_second(5).with_burst(0),
            Quota::from_parts(5, Duration::ZERO, 5),
        ] {
            assert!(BucketCore::create(&degenerate, Duration::ZERO).is_none());
            let state = AlgoState::new(Algorithm::TokenBucket, &degenerate, Duration::ZERO);
            assert!(state.acquire(1, Duration::ZERO).is_deny());
            assert!(state.acquire(0, Duration::ZERO).is_allow());
            assert_eq!(state.peek(Duration::ZERO), Snapshot::closed());
            state.reset(Duration::ZERO);
        }
    }

    #[test]
    fn test_token_bucket_burst_above_the_old_packing_ceiling() {
        // better-bucket 1.0 capped capacity at ~4.29M tokens; 1.1 does not.
        let state = token(Quota::per_second(10_000_000));
        assert_eq!(KeyState::peek(&state, Duration::ZERO).limit(), 10_000_000);
        assert!(KeyState::acquire(&state, 5_000_000, Duration::ZERO).is_allow());
        assert!(KeyState::acquire(&state, 5_000_000, Duration::ZERO).is_allow());
        assert!(KeyState::acquire(&state, 1, Duration::ZERO).is_deny());
    }

    #[test]
    fn test_fresh_snapshot_reports_capacity_per_algorithm() {
        let quota = Quota::per_second(10).with_burst(20);
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &quota),
            Snapshot::full(20)
        );
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &Quota::per_second(0)),
            Snapshot::closed()
        );
        assert_eq!(
            fresh_snapshot(Algorithm::TokenBucket, &quota.with_burst(0)),
            Snapshot::closed()
        );
        #[cfg(feature = "algorithms")]
        {
            assert_eq!(
                fresh_snapshot(Algorithm::LeakyBucket, &quota),
                Snapshot::full(20)
            );
            for window in [
                Algorithm::FixedWindow,
                Algorithm::SlidingWindowLog,
                Algorithm::SlidingWindowCounter,
            ] {
                assert_eq!(fresh_snapshot(window, &quota), Snapshot::full(10));
            }
        }
    }

    #[test]
    fn test_debt_is_none_when_recovered_and_exact_otherwise() {
        let state = token(Quota::per_second(10));
        assert_eq!(state.debt(Duration::ZERO), None);
        assert!(KeyState::acquire(&state, 4, Duration::ZERO).is_allow());
        assert_eq!(
            state.debt(Duration::ZERO),
            Some(Debt {
                spent: 4,
                until: ms(400)
            })
        );
        assert_eq!(state.debt(ms(400)), None);
    }

    #[test]
    fn test_restore_resumes_a_token_bucket_where_it_was() {
        // Drained at t=0 (10 spent, back by t=1s), returning at t=300ms: the
        // original would hold 3 tokens. The restored state holds no more.
        let original = token(Quota::per_second(10));
        assert!(KeyState::acquire(&original, 10, Duration::ZERO).is_allow());
        let debt = original.debt(Duration::ZERO).unwrap();

        let restored = BucketCore::create(&Quota::per_second(10), ms(300)).unwrap();
        restored.restore(debt, ms(300));
        assert_eq!(KeyState::peek(&restored, ms(300)).remaining(), 3);
        assert_eq!(KeyState::peek(&original, ms(300)).remaining(), 3);
        assert_eq!(KeyState::peek(&restored, ms(1_000)), Snapshot::full(10));
        // A debt already repaid restores nothing.
        let late = BucketCore::create(&Quota::per_second(10), ms(1_000)).unwrap();
        late.restore(debt, ms(1_000));
        assert_eq!(KeyState::peek(&late, ms(1_000)), Snapshot::full(10));
    }

    #[test]
    fn test_restore_never_charges_more_than_capacity_or_spent() {
        let state = token(Quota::per_second(5));
        state.restore(
            Debt {
                spent: u32::MAX,
                until: Duration::MAX,
            },
            Duration::ZERO,
        );
        assert_eq!(KeyState::peek(&state, Duration::ZERO).remaining(), 0);
        let small = token(Quota::per_second(5));
        small.restore(
            Debt {
                spent: 1,
                until: Duration::MAX,
            },
            Duration::ZERO,
        );
        assert_eq!(KeyState::peek(&small, Duration::ZERO).remaining(), 4);
    }

    /// For one algorithm: a key spends `spend` at 100ms, is evicted, and is
    /// restored `gap` ms later; at the return and every later instant it admits
    /// no more than the original would have.
    fn restore_is_never_looser<S: KeyState>(quota: &Quota) {
        for spend in [1u32, 4, 10] {
            for gap in [0u64, 50, 250, 700, 999, 1_500] {
                let original = S::create(quota, ms(100)).unwrap();
                assert!(original.acquire(spend, ms(100)).is_allow());
                let debt = original.debt(ms(100)).unwrap();
                let at = ms(100 + gap);
                let restored = S::create(quota, at).unwrap();
                restored.restore(debt, at);
                for later in [0u64, 1, 100, 400, 900, 1_000, 2_100] {
                    let t = at + ms(later);
                    assert!(
                        restored.peek(t).remaining() <= original.peek(t).remaining(),
                        "spend {spend} gap {gap} later {later}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_restore_is_never_looser_than_the_original() {
        let quota = Quota::per_second(10);
        restore_is_never_looser::<BucketCore>(&quota);
        #[cfg(feature = "algorithms")]
        {
            restore_is_never_looser::<crate::gcra::Gcra>(&quota);
            restore_is_never_looser::<super::FixedWindow>(&quota);
            restore_is_never_looser::<super::SlidingLog>(&quota);
            restore_is_never_looser::<super::SlidingCounter>(&quota);
        }
    }
}
