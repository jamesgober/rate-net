//! Loom concurrency model checks for `rate-net`.
//!
//! Compiled and run only under `RUSTFLAGS="--cfg loom" cargo test --test
//! loom_check`; under a normal build the `#![cfg(loom)]` gate makes this file
//! empty.
//!
//! These models exhaustively explore the thread interleavings of the two
//! concurrency mechanics the limiter relies on, asserting the safety invariant —
//! **a key is never over-admitted** — holds under all of them. They model the
//! protocols `src/store.rs` and `better-bucket` implement (loom cannot see
//! through a dependency's `std` locks, so the model mirrors the production
//! logic rather than driving it):
//!
//! 1. the per-key acquire kernel — a `compare_exchange` loop draining a shared
//!    budget; and
//! 2. the store's check protocol — a shard read-lock fast path, with a
//!    write-lock insert-on-miss that uses get-or-insert so two racing
//!    first-touches of the same key share one budget rather than creating two.

#![cfg(loom)]

use std::collections::HashMap;

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::{Arc, RwLock};
use loom::thread;

/// Admit one request against a shared budget via CAS, reporting whether it was
/// admitted. The bare contention kernel of the per-key check.
fn try_admit_one(budget: &AtomicU64) -> bool {
    let mut current = budget.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return false;
        }
        match budget.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

#[test]
fn loom_concurrent_admit_never_over_admits() {
    loom::model(|| {
        let budget = Arc::new(AtomicU64::new(1));
        let racer = Arc::clone(&budget);

        // Two threads race for a single available unit.
        let handle = thread::spawn(move || try_admit_one(&racer));
        let admitted_here = try_admit_one(&budget);
        let admitted_there = handle.join().unwrap();

        // Budget was 1: at most one side may be admitted — never both, and the
        // remaining budget reflects exactly what was handed out.
        let admitted = u64::from(admitted_here) + u64::from(admitted_there);
        assert!(admitted <= 1, "over-admit: budget of 1 admitted {admitted}");
        assert_eq!(budget.load(Ordering::Acquire), 1 - admitted);
    });
}

/// A shard's key space: a budget per key behind a single `RwLock`, mirroring
/// `Store`'s per-shard `RwLock<HashMap<Key, Entry>>`.
type Shard = RwLock<HashMap<u64, Arc<AtomicU64>>>;

/// The store's check protocol for one key: a shared read-lock fast path for an
/// existing key, then an exclusive write-lock slow path that inserts on miss via
/// get-or-insert (so a concurrent first-touch never creates a second budget).
/// Returns whether the request was admitted.
fn check(shard: &Shard, key: u64, initial_budget: u64) -> bool {
    {
        let map = shard.read().unwrap();
        if let Some(budget) = map.get(&key) {
            return try_admit_one(budget);
        }
    }
    let budget = {
        let mut map = shard.write().unwrap();
        Arc::clone(
            map.entry(key)
                .or_insert_with(|| Arc::new(AtomicU64::new(initial_budget))),
        )
    };
    try_admit_one(&budget)
}

#[test]
fn loom_concurrent_first_touch_shares_one_budget() {
    loom::model(|| {
        let shard: Arc<Shard> = Arc::new(RwLock::new(HashMap::new()));
        let other = Arc::clone(&shard);

        // Two threads check the same brand-new key at the same instant, racing
        // through the read miss into the write path.
        let handle = thread::spawn(move || u64::from(check(&other, 7, 1)));
        let admitted_here = u64::from(check(&shard, 7, 1));
        let admitted_there = handle.join().unwrap();

        // The single available unit goes to exactly one of them...
        let admitted = admitted_here + admitted_there;
        assert!(
            admitted <= 1,
            "over-admit: shared budget of 1 admitted {admitted}"
        );

        // ...and exactly one budget was created for the key (no duplicate).
        let map = shard.read().unwrap();
        assert_eq!(map.len(), 1, "first-touch race created duplicate buckets");
    });
}
