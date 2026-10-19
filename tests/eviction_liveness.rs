//! Liveness under eviction pressure (RN2-001): never resetting an evicted
//! key's limit must not come at the price of denying keys that never owed
//! anything, or users who stay inside their own limits.
//!
//! `tests/eviction_bypass.rs` checks the safety side (no evicted key ever comes
//! back looser). These check the other side, with the scenarios of the
//! 2.0 re-audit's attack harness scaled down to run in well under a second.
//! The store keeps evicted debts exactly for up to about twice its cap (a
//! cuckoo-hashed table). Past that, the default `Overflow::FailClosed` falls
//! back to a shared sketch that can charge an unrelated key, so its scenarios
//! run inside the bound, where it must be (all but) free of false denials;
//! the opt-in `Overflow::Forget` drops such debts and must never deny a
//! newcomer at all. `docs/BENCHMARKS.md` has the numbers past the bound and
//! at full scale.

#![cfg(feature = "std")]
#![allow(
    clippy::cast_precision_loss,
    reason = "percentages of counts far below 2^52"
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Eviction, Overflow, Quota, RateLimiter};

/// A small deterministic PRNG (xorshift), so every run is the same.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x % n
    }
}

fn limiter(quota: Quota, cap: usize) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .quota(quota.limit(), quota.period())
        .burst(quota.burst())
        .shards(16)
        .eviction(Eviction::capacity(cap))
        .clock(Arc::clone(&clock))
        .build();
    (clock, limiter)
}

/// Percentage of never-seen keys denied their first unit after a flood of
/// `flood` unique keys that each drain their whole quota, with the clock
/// frozen (no debt is ever repaid: the worst case).
fn newcomers_denied_after_drain_flood(cap: usize, flood: u64) -> f64 {
    let (_clock, limiter) = limiter(Quota::per_minute(10), cap);
    for key in 0..flood {
        let _ = limiter.check_n_bytes(key.to_be_bytes(), 10);
    }
    let denied = (0..2_000u64)
        .filter(|i| limiter.check(format!("newcomer-{i}")).is_deny())
        .count();
    denied as f64 * 100.0 / 2_000.0
}

#[test]
fn test_newcomers_are_admitted_after_a_drain_flood_up_to_twice_the_cap() {
    // First 2.0 candidate: 23 % at a flood of 1.5x the cap, 61 % at 2x.
    // 1.1.0: 0 % (it forgot every debt). Now (measured): 0-0.05 % at 1.5x,
    // about 1.4 % at 2x, where the flood's unrecovered debts fill the exact
    // table.
    let cap = 4_000;
    let at_one_and_a_half = newcomers_denied_after_drain_flood(cap, cap as u64 * 5 / 2);
    assert!(
        at_one_and_a_half < 1.0,
        "{at_one_and_a_half} % denied at 1.5x"
    );
    let at_two = newcomers_denied_after_drain_flood(cap, cap as u64 * 3);
    assert!(at_two < 5.0, "{at_two} % denied at 2x");
}

#[test]
fn test_well_behaved_users_outnumbering_the_cap_are_not_denied() {
    // A population twice the cap, every user sending only while its own
    // exact bucket holds at least two tokens. First 2.0 candidate: 87 %
    // false denials at 3x; 1.1.0: 0 %. Now (measured): 0 at 2x, 0-0.6 % at
    // 2.5x (where a shard's share can approach its table, depending on the
    // random hasher seed), 1-3 % at 3x, 22 % at 4x (past the exact table, see
    // the docs).
    let (users, cap, visits_per_sec) = (4_000u64, 2_000usize, 1_000u64);
    let (clock, limiter) = limiter(Quota::per_minute(10), cap);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    // The oracle: each user's exact token bucket (tokens, last time).
    let mut oracle: HashMap<u64, (f64, f64)> = HashMap::new();
    let step = Duration::from_nanos(1_000_000_000 / visits_per_sec);
    let (mut t, mut checks, mut false_denies) = (0.0f64, 0u64, 0u64);
    for _ in 0..visits_per_sec * 120 {
        clock.advance(step);
        t += step.as_secs_f64();
        let user = rng.below(users);
        for _ in 0..=rng.below(3) {
            let bucket = oracle.entry(user).or_insert((10.0, t));
            bucket.0 = (bucket.0 + (t - bucket.1) * 10.0 / 60.0).min(10.0);
            bucket.1 = t;
            if bucket.0 < 2.0 {
                break;
            }
            checks += 1;
            if limiter.check(user).is_allow() {
                bucket.0 -= 1.0;
            } else {
                false_denies += 1;
            }
        }
    }
    let rate = false_denies as f64 * 100.0 / checks as f64;
    assert!(
        rate < 0.5,
        "{rate} % false denials ({false_denies} of {checks})"
    );
}

#[test]
fn test_a_one_request_flood_does_not_ratchet_into_denying_newcomers() {
    // Every flood key sends exactly one request (1 of 5 per hour) at a rate
    // whose live debts are 3.6 times the cap — far past the exact table, so
    // most of them overflow into the shared sketch; nobody exceeds anything.
    // First 2.0 candidate: 50 % of newcomers denied at the default cap (debt
    // ratcheted up through merged records). Now the sketch only ever holds
    // one-unit debts, so no newcomer is denied and none starts more than one
    // unit short (about one in eight does).
    let (clock, limiter) = limiter(
        Quota::rate(5, Duration::from_secs(3_600)).unwrap_or(Quota::per_minute(5)),
        4_000,
    );
    for key in 0..40_000u64 {
        clock.advance(Duration::from_millis(50));
        let _ = limiter.check_n_bytes(key.to_be_bytes(), 1);
    }
    let mut denied = 0;
    let mut short = 0;
    for i in 0..1_000u64 {
        let key = format!("newcomer-{i}");
        let remaining = limiter.peek(key.as_str()).remaining();
        assert!(
            remaining >= 4,
            "{key} started {} units short",
            5 - remaining
        );
        if remaining < 5 {
            short += 1;
        }
        if limiter.check(key.as_str()).is_deny() {
            denied += 1;
        }
    }
    assert_eq!(denied, 0, "{denied} of 1000 newcomers denied");
    assert!(short < 250, "{short} of 1000 newcomers started short");
}

#[test]
fn test_a_returning_key_resumes_without_rounding_losses() {
    // Exact restore: a key evicted and restored on every visit loses nothing
    // to rounding, so a user who spends one unit every six seconds on
    // per_minute(10) — exactly the refill rate — is never denied.
    let (clock, limiter) = limiter(Quota::per_minute(10), 16);
    let mut other = 0u64;
    for _ in 0..200 {
        assert!(limiter.check("steady").is_allow());
        // Push "steady" out of its shard before it comes back.
        for _ in 0..64 {
            other += 1;
            let _ = limiter.check(other);
        }
        clock.advance(Duration::from_secs(6));
    }
}

#[test]
fn test_forget_admits_every_newcomer_after_any_drain_flood() {
    // `Forget` never charges a key that owed nothing: after a drain flood of
    // ten times the cap, every newcomer has its whole allowance.
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_minute(10)
        .shards(16)
        .eviction(Eviction::capacity(4_000).with_overflow(Overflow::Forget))
        .clock(Arc::clone(&clock))
        .build();
    for key in 0..44_000u64 {
        let _ = limiter.check_n_bytes(key.to_be_bytes(), 10);
    }
    let denied = (0..2_000u64)
        .filter(|i| limiter.check_n(format!("newcomer-{i}"), 10).is_deny())
        .count();
    assert_eq!(denied, 0);
}
