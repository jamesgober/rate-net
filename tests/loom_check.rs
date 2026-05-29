//! Loom concurrency model check for `rate-net`.
//!
//! Compiled and run only under `RUSTFLAGS="--cfg loom" cargo test --test
//! loom_check`; under a normal build the `#![cfg(loom)]` gate makes this file
//! empty.
//!
//! `0.1.0` has no `check` path yet, so this model proves two things ahead of the
//! sharded core: that the loom harness compiles and runs against this crate's
//! toolchain, and that the contention mechanic the per-key admit path will use —
//! a `compare_exchange` loop draining a shared per-key budget — never admits
//! more than the budget allows under any thread interleaving. The full per-key
//! acquire/refill interleaving model (the real no-over-admit proof, per
//! algorithm) replaces this in `0.3.0`.

#![cfg(loom)]

use loom::sync::Arc;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;

/// Admit one request against a shared per-key budget via CAS, reporting whether
/// it was admitted. This is the bare contention kernel of the future per-key
/// `check`: read the remaining budget, refuse if exhausted, otherwise commit a
/// decrement and retry on contention.
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
