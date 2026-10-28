//! `GcraParams` against `Gcra` and against the 2.0.1 arithmetic.
//!
//! `Gcra` is built on `GcraParams`, so the two agreeing would prove little on
//! its own. Both are also checked against `Reference` below: the GCRA exactly
//! as `rate-net` 2.0.1 shipped it, before the parameters were split out, kept
//! here as the oracle. Every check, peek, and reset must give the same
//! decision, the same snapshot, and leave the same theoretical arrival time,
//! across random quotas (zero limits and bursts, rates past one per
//! nanosecond, non-integral intervals), random `n` (`0`, within the burst, and
//! past it), and random times that mostly advance but sometimes step back (a
//! late reading) or saturate.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use proptest::prelude::*;
use rate_net::{Decision, Gcra, GcraParams, Quota, Snapshot};

/// The single-key GCRA as released in 2.0.1, verbatim apart from the snapshot
/// being returned as a tuple (its constructor is crate-private).
struct Reference {
    tat: AtomicU64,
    limit: u64,
    period_ns: u64,
    per_ns: u128,
    tolerance: u64,
    deny_all: bool,
}

fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

impl Reference {
    fn new(quota: Quota) -> Self {
        let period_ns = nanos(quota.period());
        let burst = quota.burst();
        let limit = u64::from(quota.limit()).min(period_ns);
        let deny_all = limit == 0 || burst == 0;
        let per_ns = if deny_all {
            0
        } else {
            (u128::from(limit) << 64) / u128::from(period_ns)
        };
        Self {
            tat: AtomicU64::new(0),
            limit,
            period_ns,
            per_ns,
            tolerance: u64::from(burst.saturating_sub(1)),
            deny_all,
        }
    }

    fn intervals_at(&self, now_ns: u64) -> u64 {
        let wide = u128::from(now_ns);
        let estimate = ((wide * self.per_ns) >> 64) as u64;
        let next = u128::from(estimate) + 1;
        if next * u128::from(self.period_ns) <= wide * u128::from(self.limit) {
            estimate + 1
        } else {
            estimate
        }
    }

    fn nanos_at(&self, interval: u64) -> u64 {
        let scaled = u128::from(interval) * u128::from(self.period_ns);
        let ns = scaled.div_ceil(u128::from(self.limit));
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    fn check_n_at(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        let span = u64::from(n - 1);
        if self.deny_all || span > self.tolerance {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }
        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let ceiling = now_iv.saturating_add(self.tolerance);
        loop {
            let tat = self.tat.load(Ordering::Acquire);
            let effective = tat.max(now_iv);
            if effective.saturating_add(span) <= ceiling {
                let new_tat = effective.saturating_add(u64::from(n));
                if self
                    .tat
                    .compare_exchange_weak(tat, new_tat, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Decision::Allow;
                }
                continue;
            }
            let ready_iv = effective.saturating_add(span) - self.tolerance;
            let ready_ns = self.nanos_at(ready_iv);
            return Decision::Deny {
                retry_after: Duration::from_nanos(ready_ns.saturating_sub(now_ns).max(1)),
            };
        }
    }

    /// `(limit, remaining, reset_after)`, or `None` for a closed quota.
    fn peek_at(&self, now: Duration) -> Option<(u32, u32, Duration)> {
        if self.deny_all {
            return None;
        }
        let burst = self.tolerance + 1;
        let now_ns = nanos(now);
        let now_iv = self.intervals_at(now_ns);
        let tat = self.tat.load(Ordering::Acquire).max(now_iv);
        let remaining = now_iv.saturating_add(burst).saturating_sub(tat);
        let reset_after = if tat == now_iv {
            Duration::ZERO
        } else {
            Duration::from_nanos(self.nanos_at(tat).saturating_sub(now_ns))
        };
        Some((
            u32::try_from(burst).unwrap_or(u32::MAX),
            u32::try_from(remaining).unwrap_or(u32::MAX),
            reset_after,
        ))
    }

    fn reset(&self) {
        self.tat.store(0, Ordering::Release);
    }

    fn word(&self) -> u64 {
        self.tat.load(Ordering::Relaxed)
    }
}

/// A closed quota's snapshot as `Gcra` reports it, for comparing with
/// `Reference::peek_at`'s `None`.
fn closed() -> Snapshot {
    Gcra::new(Quota::per_second(0)).peek_at(Duration::ZERO)
}

fn same_snapshot(snapshot: Snapshot, reference: Option<(u32, u32, Duration)>) -> bool {
    match reference {
        None => snapshot == closed(),
        Some((limit, remaining, reset_after)) => {
            snapshot.limit() == limit
                && snapshot.remaining() == remaining
                && snapshot.reset_after() == reset_after
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Check(u32),
    Peek,
    Reset,
}

/// Where the next reading lands relative to the latest one.
#[derive(Debug, Clone, Copy)]
enum Step {
    Forward(u64),
    /// A late reading: this far before the latest.
    Back(u64),
    /// A reading past the `u64` nanosecond range, which saturates.
    Saturate,
}

fn quotas() -> impl Strategy<Value = Quota> {
    let limit = prop_oneof![
        3 => 1u32..=1_000,
        1 => Just(0u32),
        1 => prop::sample::select(vec![3u32, 7, 600_000_000, 1_000_000_000, 2_000_000_000, u32::MAX]),
    ];
    let period = prop::sample::select(vec![
        Duration::from_nanos(1),
        Duration::from_nanos(1_000),
        Duration::from_millis(1),
        Duration::from_millis(250),
        Duration::from_secs(1),
        Duration::from_secs(3),
        Duration::from_secs(60),
        Duration::from_secs(86_400),
        Duration::MAX,
    ]);
    let burst = prop_oneof![
        3 => 1u32..=64,
        1 => Just(0u32),
        1 => Just(u32::MAX),
    ];
    (limit, period, burst).prop_filter_map("a quota with a nonzero period", |(l, p, b)| {
        Quota::rate(l.max(1), p).ok().map(|q| {
            // `rate` rejects a zero limit; `per_second(0)` is the public way
            // to build one.
            let q = if l == 0 { Quota::per_second(0) } else { q };
            q.with_burst(b)
        })
    })
}

fn ops() -> impl Strategy<Value = Vec<(Step, Op)>> {
    let step = prop_oneof![
        8 => (0u64..=2_000_000_000).prop_map(Step::Forward),
        2 => (0u64..=50).prop_map(Step::Forward),
        2 => (0u64..=500_000_000).prop_map(Step::Back),
        1 => Just(Step::Saturate),
    ];
    let op = prop_oneof![
        8 => (0u32..=4).prop_map(Op::Check),
        2 => (0u32..=80).prop_map(Op::Check),
        1 => Just(Op::Check(u32::MAX)),
        2 => Just(Op::Peek),
        1 => Just(Op::Reset),
    ];
    prop::collection::vec((step, op), 1..200)
}

/// Applies a step to the latest reading.
fn advance(latest: &mut Duration, step: Step) -> Duration {
    match step {
        Step::Forward(ns) => {
            *latest = latest.saturating_add(Duration::from_nanos(ns));
            *latest
        }
        Step::Back(ns) => latest.saturating_sub(Duration::from_nanos(ns)),
        Step::Saturate => Duration::MAX,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn prop_params_gcra_and_reference_decide_identically(
        quota in quotas(),
        ops in ops(),
        start in prop::sample::select(vec![0u64, 1, 1_000_000_000, u64::MAX - 10_000_000_000]),
    ) {
        let params = GcraParams::new(quota);
        let word = AtomicU64::new(0);
        let gcra = Gcra::new(quota);
        let reference = Reference::new(quota);
        let mut latest = Duration::from_nanos(start);

        for (step, op) in ops {
            let now = advance(&mut latest, step);
            match op {
                Op::Check(n) => {
                    // `check_at` is `check_n_at(1, ..)`: run it on a copy.
                    let copy = AtomicU64::new(word.load(Ordering::Relaxed));
                    let single = params.check_at(&copy, now);
                    let expected = reference.check_n_at(n, now);
                    prop_assert_eq!(params.check_n_at(&word, n, now), expected);
                    prop_assert_eq!(gcra.check_n_at(n, now), expected);
                    if n == 1 {
                        prop_assert_eq!(single, expected);
                        prop_assert_eq!(copy.load(Ordering::Relaxed), word.load(Ordering::Relaxed));
                    }
                }
                Op::Peek => {
                    let expected = reference.peek_at(now);
                    prop_assert!(same_snapshot(params.peek_at(&word, now), expected));
                    prop_assert!(same_snapshot(gcra.peek_at(now), expected));
                }
                Op::Reset => {
                    reference.reset();
                    params.reset(&word);
                    gcra.reset();
                }
            }
            prop_assert_eq!(word.load(Ordering::Relaxed), reference.word());
        }
    }

    #[test]
    fn prop_one_params_over_many_words_matches_one_gcra_per_word(
        quota in quotas(),
        ops in prop::collection::vec((0usize..4, 0u64..=300_000_000, 0u32..=6), 1..300),
    ) {
        // The keyed use: one set of parameters, a word per key.
        let params = GcraParams::new(quota);
        let words: Vec<AtomicU64> = (0..4).map(|_| AtomicU64::new(0)).collect();
        let gcras: Vec<Gcra> = (0..4).map(|_| Gcra::new(quota)).collect();
        let mut now = Duration::ZERO;
        for (key, dt, n) in ops {
            now += Duration::from_nanos(dt);
            prop_assert_eq!(
                params.check_n_at(&words[key], n, now),
                gcras[key].check_n_at(n, now)
            );
            prop_assert_eq!(params.peek_at(&words[key], now), gcras[key].peek_at(now));
        }
    }
}

/// A long deterministic run on the quotas a gatekeeper uses, with readings
/// that jitter backwards the way a thread's stale timestamp does.
#[test]
fn test_params_match_reference_over_a_long_jittered_run() {
    let quotas = [
        Quota::per_second(10),
        Quota::per_second(3).with_burst(1),
        Quota::per_second(1_000).with_burst(50),
        Quota::per_minute(100).with_burst(20),
        Quota::per_second(600_000_000).with_burst(4),
        Quota::per_second(2_000_000_000).with_burst(2),
        Quota::per_second(0),
        Quota::per_second(10).with_burst(0),
    ];
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for quota in quotas {
        let params = GcraParams::new(quota);
        let word = AtomicU64::new(0);
        let reference = Reference::new(quota);
        let mut latest = 0u64;
        for _ in 0..20_000 {
            let r = next();
            latest += r % 40_000_000;
            // One reading in eight is up to 20 ms late.
            let now_ns = if r % 8 == 0 {
                latest.saturating_sub((r >> 8) % 20_000_000)
            } else {
                latest
            };
            let now = Duration::from_nanos(now_ns);
            let n = u32::try_from((r >> 32) % 6).unwrap_or(1);
            assert_eq!(
                params.check_n_at(&word, n, now),
                reference.check_n_at(n, now),
                "{quota:?} n={n} now={now:?}"
            );
            assert_eq!(word.load(Ordering::Relaxed), reference.word());
        }
    }
}
