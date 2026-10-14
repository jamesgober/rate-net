//! Per-key quota overrides and the key-length policies (`quota_for`,
//! `max_key_len`, `digest_keys_longer_than`), and the borrowed-key methods.

#![cfg(feature = "std")]

use std::sync::Arc;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Eviction, Key, Quota, RateLimiter};

#[test]
fn test_quota_for_gives_each_tier_its_own_quota() {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_minute(10)
        .quota_for(|key| match key.as_bytes().first() {
            Some(b'p') => Some(Quota::per_minute(1_000)),
            Some(b'b') => Some(Quota::per_minute(0)), // blocked
            _ => None,
        })
        .clock(Arc::clone(&clock))
        .build();

    assert!(limiter.check_n("pro", 1_000).is_allow());
    assert!(limiter.check("pro").is_deny());
    assert!(limiter.check_n("free", 10).is_allow());
    assert!(limiter.check("free").is_deny());
    // A quota that admits nothing fails closed and is never stored.
    assert!(limiter.check("blocked").is_impossible());
    assert_eq!(limiter.peek("blocked").limit(), 0);
    assert_eq!(limiter.tracked_keys(), 2);
    // Untracked keys report their own tier's allowance.
    assert_eq!(limiter.peek("pro:new").limit(), 1_000);
    assert_eq!(limiter.peek("free:new").limit(), 10);
    // The default quota is still the limiter's quota.
    assert_eq!(limiter.quota().limit(), 10);

    clock.advance(Duration::from_secs(60));
    assert!(limiter.check_n("pro", 1_000).is_allow());
}

#[test]
fn test_quota_for_survives_rebuilds_and_eviction() {
    let limiter = RateLimiter::builder()
        .per_second(1)
        .quota_for(|key| (key.as_bytes() == b"vip").then(|| Quota::per_second(5)))
        .build()
        .with_eviction(Eviction::capacity(1))
        .with_shards(1);
    assert!(limiter.check_n("vip", 5).is_allow());
    // Evicted while drained, it returns drained under its own quota.
    assert!(limiter.check("other").is_allow());
    assert!(limiter.check("vip").is_deny());
    assert_eq!(limiter.peek("vip").limit(), 5);
}

#[test]
fn test_max_key_len_refuses_long_keys_without_storing_them() {
    let limiter = RateLimiter::builder().per_second(10).max_key_len(8).build();
    assert!(limiter.check("12345678").is_allow());
    assert!(limiter.check("123456789").is_impossible());
    assert!(limiter.check_bytes([0u8; 9]).is_impossible());
    assert_eq!(limiter.tracked_keys(), 1);
    assert_eq!(limiter.peek("123456789").limit(), 0);
    assert!(!limiter.reset("123456789"));
    assert!(!limiter.remove("123456789"));
    // Zero units of an over-long key are still refused: the key is the problem.
    assert!(limiter.check_n("123456789", 0).is_impossible());
}

#[test]
fn test_digested_keys_keep_separate_limits_and_bounded_memory() {
    let limiter = RateLimiter::builder()
        .per_second(2)
        .digest_keys_longer_than(16)
        .build();
    let a = "a".repeat(100);
    let b = "b".repeat(100);
    assert!(limiter.check_n(a.as_str(), 2).is_allow());
    assert!(limiter.check(a.as_str()).is_deny());
    // A different long key has its own allowance.
    assert!(limiter.check_n_bytes(&b, 2).is_allow());
    // Owned and borrowed forms of one long key are the same key.
    assert!(limiter.check_bytes(&a).is_deny());
    assert_eq!(limiter.peek(a.as_str()).remaining(), 0);
    assert_eq!(limiter.peek_bytes(&b).remaining(), 0);
    // Short keys are stored as they are, and never equal a digest.
    assert!(limiter.check("short").is_allow());
    assert_eq!(limiter.tracked_keys(), 3);
    // Reset and remove find digested keys too.
    assert!(limiter.reset(a.as_str()));
    assert!(limiter.check(a.as_str()).is_allow());
    assert!(limiter.remove(Key::from(b.as_str())));
    assert_eq!(limiter.tracked_keys(), 2);
}

#[test]
fn test_digest_resolver_sees_the_original_key() {
    let limiter = RateLimiter::builder()
        .per_second(1)
        .digest_keys_longer_than(4)
        .quota_for(|key| {
            key.as_bytes()
                .starts_with(b"pro-")
                .then(|| Quota::per_second(9))
        })
        .build();
    let long = format!("pro-{}", "x".repeat(200));
    assert!(limiter.check_n(long.as_str(), 9).is_allow());
    assert_eq!(limiter.peek(long.as_str()).limit(), 9);
}

#[test]
fn test_borrowed_and_owned_checks_share_state() {
    let limiter = RateLimiter::per_second(3);
    let token = String::from("tenant-0123456789-abcdefghijklmnop");
    assert!(limiter.check(&token).is_allow());
    assert!(limiter.check_bytes(&token).is_allow());
    assert!(limiter.check_n_bytes(token.as_bytes(), 1).is_allow());
    assert!(limiter.check(token.as_str()).is_deny());
    assert_eq!(limiter.peek_bytes(token.as_bytes()).remaining(), 0);
    assert_eq!(limiter.tracked_keys(), 1);
}

#[cfg(feature = "metrics")]
#[test]
fn test_metrics_count_rejections_and_remembered_debts() {
    let limiter = RateLimiter::builder()
        .per_minute(1)
        .max_key_len(4)
        .shards(1)
        .eviction(Eviction::capacity(1))
        .build();
    assert!(limiter.check("toolong").is_impossible());
    assert!(limiter.check("a").is_allow());
    assert!(limiter.check("b").is_allow()); // evicts "a", owing its unit
    assert!(limiter.check("a").is_deny()); // restored from its debt
    let metrics = limiter.metrics();
    assert_eq!(metrics.keys_rejected, 1);
    assert_eq!(metrics.denied, 2);
    assert_eq!(metrics.keys_remembered, 2);
    assert_eq!(metrics.keys_restored, 1);
    assert_eq!(metrics.records_merged, 0);
}
