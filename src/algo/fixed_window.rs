//! Fixed window counter.
//!
//! Time is sliced into fixed windows of `period`; each window admits up to
//! `limit` units and resets at the boundary. The cheapest algorithm — one packed
//! atomic per key, updated by compare-and-swap — at the cost of tolerating up to
//! `2 * limit` across a window boundary (the classic fixed-window burst). It
//! ignores the quota's `burst`.

use core::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::decision::Decision;
use crate::quota::Quota;

/// Clamps a duration-since-epoch to nanoseconds.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// Packs `(window_index, count)` into one word. The index wraps at `2^32`
/// windows; consecutive windows always differ, and a collision could only follow
/// an idle gap of `2^32` windows (long since evicted), where the effect is at
/// worst an over-strict denial — never an over-admit.
const fn pack(window_index: u32, count: u32) -> u64 {
    ((window_index as u64) << 32) | (count as u64)
}

const fn unpack(state: u64) -> (u32, u32) {
    ((state >> 32) as u32, (state & 0xFFFF_FFFF) as u32)
}

/// Fixed-window state for one key.
pub(crate) struct FixedWindow {
    /// Packed `(window_index, count)`.
    state: AtomicU64,
    /// Window length in ns (always `>= 1`).
    period_ns: u64,
    limit: u32,
}

impl FixedWindow {
    pub(crate) fn new(quota: &Quota, now: Duration) -> Self {
        let period_ns = nanos(quota.period()).max(1);
        let window_index = (nanos(now) / period_ns) as u32;
        Self {
            state: AtomicU64::new(pack(window_index, 0)),
            period_ns,
            limit: quota.limit(),
        }
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        // A request larger than a whole window's allowance can never fit.
        if n > self.limit {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        let now_ns = nanos(now);
        let window_number = now_ns / self.period_ns;
        let window_index = window_number as u32;

        loop {
            let current = self.state.load(Ordering::Acquire);
            let (stored_index, stored_count) = unpack(current);
            // A new window resets the count.
            let base = if stored_index == window_index {
                stored_count
            } else {
                0
            };
            let new_count = base + n; // `base <= limit` and `n <= limit`, so this fits in u32

            if new_count > self.limit {
                // Full for this window: the next window admits again.
                let next_start = window_number
                    .saturating_add(1)
                    .saturating_mul(self.period_ns);
                return Decision::Deny {
                    retry_after: Duration::from_nanos(next_start.saturating_sub(now_ns)),
                };
            }

            let next = pack(window_index, new_count);
            match self.state.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Decision::Allow,
                Err(_) => continue,
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::FixedWindow;
    use crate::decision::Decision;
    use crate::quota::Quota;
    use core::time::Duration;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn test_window_admits_limit_then_denies_until_rollover() {
        let fw = FixedWindow::new(&Quota::per_second(5), at(0));
        for _ in 0..5 {
            assert_eq!(fw.acquire(1, at(10)), Decision::Allow);
        }
        assert!(fw.acquire(1, at(10)).is_deny());
        // Next window (>= 1000ms) starts fresh.
        assert_eq!(fw.acquire(1, at(1000)), Decision::Allow);
    }

    #[test]
    fn test_retry_after_points_at_window_boundary() {
        let fw = FixedWindow::new(&Quota::per_second(1), at(0));
        assert!(fw.acquire(1, at(200)).is_allow());
        match fw.acquire(1, at(200)) {
            Decision::Deny { retry_after } => {
                // Window [0,1000): boundary is 800ms away.
                assert_eq!(retry_after, Duration::from_millis(800));
            }
            other => panic!("expected denial, got {other:?}"),
        }
    }

    #[test]
    fn test_check_n_consumes_multiple_units() {
        let fw = FixedWindow::new(&Quota::per_second(10), at(0));
        assert_eq!(fw.acquire(7, at(0)), Decision::Allow);
        assert_eq!(fw.acquire(3, at(0)), Decision::Allow);
        assert!(fw.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_request_larger_than_limit_never_admits() {
        let fw = FixedWindow::new(&Quota::per_second(5), at(0));
        assert_eq!(
            fw.acquire(6, at(0)),
            Decision::Deny {
                retry_after: Duration::MAX
            }
        );
    }
}
