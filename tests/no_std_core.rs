//! The clock-free core every build exposes — including `--no-default-features`,
//! where the library is `no_std` — driven exactly as an embedded or kernel
//! caller would: time supplied by the caller, no keys, no allocation.

use core::time::Duration;

use rate_net::{Decision, Gcra, Quota, RateLimiterError, Snapshot};

/// A caller-side monotonic tick source: here, plain milliseconds.
fn tick(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[test]
fn test_gcra_paces_a_stream_with_caller_supplied_time() {
    // 20 per second, bursts of 5.
    let gcra = Gcra::try_new(Quota::per_second(20).with_burst(5)).expect("valid quota");
    let burst = (0..10)
        .filter(|_| gcra.check_at(tick(0)).is_allow())
        .count();
    assert_eq!(burst, 5);
    // One unit every 50ms after the burst.
    assert_eq!(gcra.check_at(tick(0)).retry_after(), Some(tick(50)));
    assert_eq!(gcra.check_at(tick(50)), Decision::Allow);
    assert!(gcra.check_at(tick(50)).is_deny());

    let snapshot: Snapshot = gcra.peek_at(tick(50));
    assert_eq!(snapshot.limit(), 5);
    assert!(snapshot.is_exhausted());
}

#[test]
fn test_gcra_never_over_admits_over_a_long_run() {
    let gcra = Gcra::new(Quota::per_second(1_000));
    let mut admitted = 0u64;
    // Ten seconds of 10 attempts per millisecond.
    for ms in 0..10_000u64 {
        for _ in 0..10 {
            if gcra.check_at(tick(ms)).is_allow() {
                admitted += 1;
            }
        }
    }
    // The initial burst plus the rate times the elapsed time.
    assert!(admitted <= 1_000 + 10_000, "admitted {admitted}");
    assert!(admitted >= 10_000, "under-admitted {admitted}");
}

#[test]
fn test_validation_is_available_without_std() {
    assert_eq!(
        Gcra::try_new(Quota::per_second(0)).err(),
        Some(RateLimiterError::ZeroQuota)
    );
    assert_eq!(
        Quota::rate(5, Duration::ZERO),
        Err(RateLimiterError::ZeroPeriod)
    );
}
