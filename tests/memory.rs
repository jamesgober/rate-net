//! Memory budget: live heap per tracked key, asserted against the documented
//! figure.
//!
//! A global allocator tracks live heap bytes; the test fills a limiter with
//! unique keys and divides the growth by the key count. The README documents
//! the result, so a layout change that bloats per-key state fails here before
//! it ships.

#![cfg(feature = "std")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};

use rate_net::{Eviction, RateLimiter};

/// Live heap bytes across all threads.
static LIVE: AtomicIsize = AtomicIsize::new(0);

/// Tracks live bytes, delegating to the system allocator.
struct Tracking;

// SAFETY: every method forwards directly to the system allocator with the same
// arguments, so the allocator contract is upheld unchanged; the only added
// behaviour is a relaxed counter update, which never allocates.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _live = LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _live = LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: `ptr`/`layout` originate from `System.alloc` and are forwarded
        // unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _live = LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let delta = new_size as isize - layout.size() as isize;
        let _live = LIVE.fetch_add(delta, Ordering::Relaxed);
        // SAFETY: `ptr`/`layout`/`new_size` satisfy the system allocator's
        // contract and are forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

/// The documented ceiling: live heap per tracked key, worst case across the
/// growth steps of the shard vectors (which double, so a store that has just
/// grown carries up to 2x slack).
const BUDGET_BYTES_PER_KEY: isize = 512;

/// Live heap per key for `keys` unique keys.
fn bytes_per_key(keys: u64) -> isize {
    let before = LIVE.load(Ordering::Relaxed);
    let limiter = RateLimiter::per_second(10)
        .with_shards(64)
        .with_eviction(Eviction::unbounded());
    for key in 0..keys {
        let _ = limiter.check(key);
    }
    assert_eq!(limiter.tracked_keys() as u64, keys);
    let grown = LIVE.load(Ordering::Relaxed) - before;
    drop(limiter);
    grown / keys as isize
}

#[test]
fn test_live_heap_per_key_stays_within_budget() {
    // Several sizes, so the measurement spans different points of the shard
    // vectors' doubling.
    for keys in [50_000u64, 100_000, 150_000] {
        let per_key = bytes_per_key(keys);
        assert!(
            per_key <= BUDGET_BYTES_PER_KEY,
            "{keys} keys cost {per_key} B/key, budget {BUDGET_BYTES_PER_KEY}"
        );
        assert!(per_key >= 64, "implausibly small: {per_key} B/key");
    }
}

#[test]
#[ignore = "prints the measured figure for the documentation; run with --ignored --nocapture"]
fn report_bytes_per_key() {
    for keys in [100_000u64, 131_072, 200_000, 262_144, 1_000_000] {
        // Printing is the point of this report-only test.
        #[allow(clippy::print_stdout)]
        {
            println!("{keys:>9} keys: {} B/key", bytes_per_key(keys));
        }
    }
    // The default limiter (default shards and cap), filled to its cap.
    let before = LIVE.load(Ordering::Relaxed);
    let limiter = RateLimiter::per_second(10);
    for key in 0..rate_net::DEFAULT_MAX_KEYS as u64 {
        let _ = limiter.check(key);
    }
    let grown = LIVE.load(Ordering::Relaxed) - before;
    let keys = limiter.tracked_keys();
    #[allow(clippy::print_stdout)]
    {
        println!(
            "default limiter, {} shards, {keys} keys: {} MiB, {} B/key",
            limiter.shards(),
            grown / (1 << 20),
            grown / keys as isize
        );
    }
}
