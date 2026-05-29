//! Allocation audit for the steady-state check path.
//!
//! The hot path — checking a key that already has state — must not allocate.
//! This installs a global allocator that counts allocations per thread, warms
//! the key into the store (the one-time insert is allowed to allocate), then
//! asserts that a long run of `check` on that existing key performs zero
//! allocations on the measuring thread.
//!
//! Per-thread counting matters: a global counter would also catch incidental
//! allocations made by the test harness on other threads, which has nothing to
//! do with the limiter.

#![cfg(feature = "std")]
#![allow(clippy::unwrap_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use clock_lib::ManualClock;
use rate_net::RateLimiter;

thread_local! {
    // `const` init uses native thread-local storage, so reading or incrementing
    // it never allocates and cannot recurse into the allocator below.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn note_alloc() {
    ALLOCATIONS.with(|c| c.set(c.get() + 1));
}

fn alloc_count() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// Counts the current thread's allocations, delegating to the system allocator.
struct Counting;

// SAFETY: every method forwards directly to the system allocator with the same
// arguments, so the allocator contract is upheld unchanged; the only added
// behaviour is a non-allocating per-thread counter increment.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr`/`layout` originate from `System.alloc` and are forwarded
        // unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc();
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_alloc();
        // SAFETY: `ptr`/`layout`/`new_size` satisfy the system allocator's
        // contract and are forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[test]
fn test_existing_key_check_does_not_allocate() {
    // A high limit and a frozen clock keep every check on the admit branch, and
    // a short, inline-sized key means key construction never touches the heap.
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::per_second(u32::MAX).with_clock(Arc::clone(&clock));

    // Warm up: the first check inserts the key (allowed to allocate), and the
    // rest exercise every step the measured loop will run so any one-time lazy
    // initialisation in std happens before the measurement window.
    let mut warm = 0u64;
    for _ in 0..5_000 {
        if limiter.check("k").is_allow() {
            warm += 1;
        }
    }
    assert!(warm > 0);

    // Steady-state window: checking an existing key must allocate nothing.
    let before = alloc_count();
    let mut admitted = 0u64;
    for _ in 0..200_000 {
        if limiter.check("k").is_allow() {
            admitted += 1;
        }
    }
    let allocations = alloc_count() - before;

    assert_eq!(
        allocations, 0,
        "steady-state check allocated {allocations} time(s) on the measuring thread"
    );
    assert!(admitted > 0); // the loop did real work, so the assertion is meaningful
}
