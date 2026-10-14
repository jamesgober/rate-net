//! Loom model checks of the production limiter.
//!
//! Compiled and run only under
//! `RUSTFLAGS="--cfg loom" cargo test --test loom_check --features algorithms --release`;
//! under a normal build the `#![cfg(loom)]` gate makes this file empty.
//!
//! Under `cfg(loom)` every atomic and lock in rate-net's own state (`src/sync.rs`)
//! and in `better-bucket`'s token bucket is a loom primitive, so these models
//! drive the real `RateLimiter` — the real sharded store, the real algorithm
//! state — and loom explores every interleaving of their atomic operations.
//! Each model asserts the safety invariant: **a key is never over-admitted**.
//!
//! The clock is a `ManualClock` shared with the model. A thread reads the time
//! before its first loom operation; with a schedule point placed before the
//! main thread advances the clock, loom explores the spawned thread reading the
//! old time, stalling, and landing after the main thread has committed a later
//! timestamp: exactly the stale-timestamp interleaving behind RN-COR-01 (token
//! bucket) and RN-COR-03 (fixed window), which frozen-clock tests can never
//! reach.

#![cfg(loom)]

use std::sync::Arc as StdArc;
use std::time::Duration;

use clock_lib::{Clock, ManualClock, Monotonic, Wall};
use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;
use rate_net::{Eviction, RateLimiter};

/// A manual clock whose reads and advances are loom operations.
///
/// `ManualClock` keeps its offset in a `std` atomic, which loom cannot see:
/// a thread's clock read is then never a schedule point, and loom cannot
/// interleave "read the time" and "act on it" the way a preempted thread does.
/// Wrapping every read and advance in a loom atomic access makes each one a
/// schedule point, so a model can reach a thread that reads the time, stalls,
/// and acts after another thread has advanced the clock and committed — the
/// RN-COR-01 race inside the token bucket's own `acquire`.
#[derive(Clone)]
struct LoomClock {
    inner: StdArc<ManualClock>,
    /// Advances published so far; read on every `now` to create a schedule
    /// point in the reading thread.
    advances: Arc<AtomicU64>,
}

impl LoomClock {
    fn new() -> Self {
        Self {
            inner: StdArc::new(ManualClock::new()),
            advances: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The schedule point comes first, so loom may let other threads read
    /// the old time before it moves.
    fn advance(&self, by: Duration) {
        let _published = self.advances.fetch_add(1, Ordering::SeqCst);
        self.inner.advance(by);
    }
}

impl Clock for LoomClock {
    fn now(&self) -> Monotonic {
        let reading = self.inner.now();
        // The schedule point sits after the read: loom may run other threads
        // here, before this thread acts on its (now possibly stale) reading.
        let _seen = self.advances.load(Ordering::SeqCst);
        reading
    }

    fn wall(&self) -> Wall {
        self.inner.wall()
    }
}

/// A token-bucket limiter on a shared manual clock, one shard, no eviction.
fn limiter(
    limit: u32,
    period: Duration,
    burst: u32,
) -> (StdArc<ManualClock>, RateLimiter<StdArc<ManualClock>>) {
    let clock = StdArc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .quota(limit, period)
        .burst(burst)
        .shards(1)
        .eviction(Eviction::unbounded())
        .clock(StdArc::clone(&clock))
        .build();
    (clock, limiter)
}

/// Two threads first-touch the same key at once, racing through the read miss
/// into the insert path: exactly one entry is created and the single unit is
/// admitted at most once.
#[test]
fn loom_concurrent_first_touch_shares_one_allowance() {
    loom::model(|| {
        let (_clock, limiter) = limiter(1, Duration::from_secs(1), 1);
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        let handle = thread::spawn(move || u32::from(other.check("k").is_allow()));
        let here = u32::from(limiter.check("k").is_allow());
        let there = handle.join().expect("model thread panicked");

        assert!(
            here + there <= 1,
            "over-admit: a quota of 1 admitted {}",
            here + there
        );
        assert_eq!(limiter.tracked_keys(), 1, "duplicate state for one key");
    });
}

/// RN-COR-01: a thread that read the clock *before* another thread committed a
/// later grant must not see a negative gap as a huge refill. One token per
/// millisecond, burst 1: once the burst is spent, at most one more unit fits in
/// the first millisecond. With `better-bucket` 1.0's logic this model fails.
#[test]
fn loom_token_bucket_stale_timestamp_never_over_grants() {
    loom::model(|| {
        let clock = LoomClock::new();
        let limiter = RateLimiter::builder()
            .quota(1_000, Duration::from_secs(1))
            .burst(1)
            .shards(1)
            .eviction(Eviction::unbounded())
            .clock(clock.clone())
            .build();
        assert!(limiter.check("k").is_allow()); // spend the burst at t = 0
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        // Reads the clock at t = 0 or t = 1ms, depending on the schedule.
        let handle = thread::spawn(move || u32::from(other.check("k").is_allow()));
        clock.advance(Duration::from_millis(1));
        let here = u32::from(limiter.check("k").is_allow());
        let there = handle.join().expect("model thread panicked");

        assert!(
            here + there <= 1,
            "over-grant: {} admitted after the burst within one refill",
            here + there
        );
    });
}

/// Capacity eviction racing a check of another key in the same full shard:
/// the store stays bounded and no check panics or loses its decision.
#[test]
fn loom_eviction_races_a_concurrent_insert() {
    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_second(1)
            .shards(1)
            .eviction(Eviction::capacity(1))
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("a").is_allow());
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        let handle = thread::spawn(move || other.check("b").is_allow());
        let here = limiter.check("c").is_allow();
        let there = handle.join().expect("model thread panicked");

        // Fresh keys always have their first unit.
        assert!(here && there);
        assert_eq!(limiter.tracked_keys(), 1, "capacity cap exceeded");
    });
}

/// RN-005: a drained key evicted by a concurrent insert must come back
/// drained. One thread re-checks the drained key "a" while another inserts "b"
/// into the full one-key shard, evicting "a" (recording its debt) — in every
/// interleaving "a" is refused: the record is written under the same write
/// lock that removes the key, and consulted under the write lock that
/// re-creates it.
#[test]
fn loom_evicted_drained_key_is_never_reset() {
    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_minute(1)
            .shards(1)
            .eviction(Eviction::capacity(1))
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("a").is_allow()); // "a" is drained
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        let handle = thread::spawn(move || other.check("b").is_allow());
        let again = limiter.check("a").is_allow();
        let inserted = handle.join().expect("model thread panicked");

        assert!(!again, "eviction reset a drained key");
        assert!(inserted, "a fresh key has its first unit");
        assert!(limiter.check("a").is_deny(), "eviction reset a drained key");
    });
}

/// Two drained keys taking turns in a one-key shard: each re-check evicts the
/// other key (recording its debt) and re-creates itself from its own recorded
/// debt (taking it). Run concurrently, in every interleaving both stay
/// refused: recording and taking a debt never lose one, whichever order the
/// two write locks are granted in.
#[test]
fn loom_concurrent_record_and_take_never_reset_a_debt() {
    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_minute(1)
            .shards(1)
            .eviction(Eviction::capacity(1))
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("a").is_allow()); // "a" is drained
        assert!(limiter.check("b").is_allow()); // "b" is drained; "a" recorded
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        let handle = thread::spawn(move || other.check("a").is_allow());
        let b_again = limiter.check("b").is_allow();
        let a_again = handle.join().expect("model thread panicked");

        assert!(!a_again && !b_again, "a recorded debt was lost");
        assert!(limiter.check("a").is_deny(), "a recorded debt was lost");
        assert!(limiter.check("b").is_deny(), "a recorded debt was lost");
    });
}

/// RN3-001: an idle-expired key with a slow per-key quota, re-checked while a
/// concurrent insert's idle sweep takes it. The key spent again after it was
/// stored, so its stored recovery hint is stale; in every interleaving the
/// re-check (before or after the sweep) is refused and the debt survives.
#[test]
fn loom_idle_sweep_racing_a_recheck_never_resets_a_slow_key() {
    use rate_net::Quota;

    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .per_second(10)
            .quota_for(|key| (key.as_bytes() == b"pro").then(|| Quota::per_minute(10)))
            .shards(1)
            .eviction(Eviction::new(8, Duration::from_secs(1)))
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("pro").is_allow());
        clock.advance(Duration::from_millis(500));
        assert!(limiter.check_n("pro", 9).is_allow());
        clock.advance(Duration::from_secs(10));
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        // A new key's insert runs the idle sweep, which takes "pro".
        let handle = thread::spawn(move || other.check("fresh").is_allow());
        let again = limiter.check_n("pro", 10).is_allow();
        let inserted = handle.join().expect("model thread panicked");

        assert!(inserted, "a fresh key has its first unit");
        assert!(!again, "an idle-expired slow key was reset");
        assert!(
            limiter.check_n("pro", 10).is_deny(),
            "an idle-expired slow key was reset"
        );
    });
}

/// RN-COR-03: a thread whose timestamp falls in a window another thread has
/// already closed must count against the newer window, never write its older
/// window back and wipe the newer window's count.
#[cfg(feature = "algorithms")]
#[test]
fn loom_fixed_window_late_thread_cannot_roll_back() {
    use rate_net::Algorithm;

    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .algorithm(Algorithm::FixedWindow)
            .per_second(1)
            .shards(1)
            .eviction(Eviction::unbounded())
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("k").is_allow()); // window 0 is full
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        // Reads the clock in window 0 or window 1, depending on the schedule.
        let handle = thread::spawn(move || u32::from(other.check("k").is_allow()));
        // A schedule point: the other thread may read the clock (and stall)
        // before time moves on.
        thread::yield_now();
        clock.advance(Duration::from_secs(1));
        let first = u32::from(limiter.check("k").is_allow());
        let second = u32::from(limiter.check("k").is_allow());
        let there = handle.join().expect("model thread panicked");

        // Window 0 had no room left and window 1 holds exactly one unit.
        assert!(
            first + second + there <= 1,
            "over-admit: {} admitted across a full window and a 1-unit window",
            first + second + there
        );
    });
}

/// The leaky bucket's single compare-and-swap under the same stale-timestamp
/// schedule: never more than the burst plus the intervals that elapsed.
#[cfg(feature = "algorithms")]
#[test]
fn loom_leaky_bucket_stale_timestamp_never_over_admits() {
    use rate_net::Algorithm;

    loom::model(|| {
        let clock = StdArc::new(ManualClock::new());
        let limiter = RateLimiter::builder()
            .algorithm(Algorithm::LeakyBucket)
            .quota(1_000, Duration::from_secs(1))
            .burst(1)
            .shards(1)
            .eviction(Eviction::unbounded())
            .clock(StdArc::clone(&clock))
            .build();
        assert!(limiter.check("k").is_allow());
        let limiter = Arc::new(limiter);
        let other = Arc::clone(&limiter);

        let handle = thread::spawn(move || u32::from(other.check("k").is_allow()));
        // A schedule point: the other thread may read the clock (and stall)
        // before time moves on.
        thread::yield_now();
        clock.advance(Duration::from_millis(1));
        let here = u32::from(limiter.check("k").is_allow());
        let there = handle.join().expect("model thread panicked");

        assert!(here + there <= 1, "over-admit: {}", here + there);
    });
}
