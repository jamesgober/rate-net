//! Fixed window counter.
//!
//! Time is sliced into fixed windows of `period`; each window admits up to
//! `limit` units and resets at the boundary. The cheapest algorithm — one packed
//! atomic per key, updated by compare-and-swap — at the cost of tolerating up to
//! `2 * limit` across a window boundary (the classic fixed-window burst). It
//! ignores the quota's `burst`.
//!
//! # Late threads
//!
//! `now` is read before the compare-and-swap, so a thread can arrive carrying a
//! timestamp from a window that another thread has already closed. Such a
//! thread must never write its older window back — that would wipe the newer
//! window's count and let it admit a second full `limit`. A second word,
//! `latest`, records the full (un-truncated) number of the newest window any
//! thread has opened; a late thread sees `latest` ahead of its own window and
//! counts against the newer window instead.

use core::time::Duration;

use crate::decision::Decision;
use crate::quota::Quota;
use crate::sync::{AtomicU64, Ordering};

/// Clamps a duration-since-epoch to nanoseconds.
fn nanos(now: Duration) -> u64 {
    u64::try_from(now.as_nanos()).unwrap_or(u64::MAX)
}

/// Packs `(window_index, count)` into one word. The index is the low 32 bits of
/// the window number; the full number lives in `latest`, which is what decides
/// whether a stored count belongs to the caller's window, so a wrapped index can
/// never be mistaken for the current window.
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
    /// Full number of the newest window any thread has opened. Only ever moves
    /// forward (`fetch_max`), and is raised *before* the state word is switched
    /// to the new window, so a thread that observes the new state also observes
    /// the raised `latest`.
    latest: AtomicU64,
    /// Window length in ns. Zero only for a degenerate (zero-period) quota,
    /// which `deny_all` covers.
    period_ns: u64,
    limit: u32,
    /// Set when the quota admits nothing (`limit == 0` or a zero period).
    deny_all: bool,
}

impl FixedWindow {
    pub(crate) fn new(quota: &Quota, now: Duration) -> Self {
        let period_ns = nanos(quota.period());
        let deny_all = quota.limit() == 0 || period_ns == 0;
        let window = nanos(now).checked_div(period_ns).unwrap_or(0);
        Self {
            // Truncation to the low 32 bits is the packing contract; `latest`
            // keeps the full number.
            state: AtomicU64::new(pack(window as u32, 0)),
            latest: AtomicU64::new(window),
            period_ns,
            limit: quota.limit(),
            deny_all,
        }
    }

    /// The full window number containing `now_ns`.
    fn window_of(&self, now_ns: u64) -> u64 {
        now_ns.checked_div(self.period_ns).unwrap_or(0)
    }

    /// The wait from `now_ns` until window `window` ends.
    fn until_end_of(&self, window: u64, now_ns: u64) -> Duration {
        let next_start = window.saturating_add(1).saturating_mul(self.period_ns);
        Duration::from_nanos(next_start.saturating_sub(now_ns))
    }

    /// The count already admitted in `window`, given a state word and the
    /// `latest` value loaded after it.
    fn count_in(window: u64, current: u64, latest: u64) -> u32 {
        let (stored_index, stored_count) = unpack(current);
        // The stored count belongs to `window` only if `latest` says `window`
        // is the newest window and the state word has switched to it. A state
        // word that has not switched yet (another thread raised `latest` and is
        // about to swap) holds an older window's count, which does not apply.
        if latest == window && stored_index == window as u32 {
            stored_count
        } else {
            0
        }
    }

    pub(crate) fn acquire(&self, n: u32, now: Duration) -> Decision {
        if n == 0 {
            return Decision::Allow;
        }
        // A request larger than a whole window's allowance can never fit, and a
        // degenerate quota admits nothing.
        if self.deny_all || n > self.limit {
            return Decision::Deny {
                retry_after: Duration::MAX,
            };
        }

        let now_ns = nanos(now);
        let own_window = self.window_of(now_ns);

        loop {
            // Load the state word first: whoever switched it raised `latest`
            // before doing so, so the `latest` read below is at least as new.
            let current = self.state.load(Ordering::Acquire);
            let latest = self.latest.load(Ordering::Acquire);
            // A late thread counts against the newer window, never its own.
            let window = own_window.max(latest);
            let base = Self::count_in(window, current, latest);

            // Widened: `base` and `n` are each at most `limit`, so their sum can
            // exceed `u32::MAX` when `limit > u32::MAX / 2`.
            let total = u64::from(base) + u64::from(n);
            if total > u64::from(self.limit) {
                return Decision::Deny {
                    retry_after: self.until_end_of(window, now_ns),
                };
            }

            if window > latest {
                // Opening a new window: publish it before switching the state.
                let _previous = self.latest.fetch_max(window, Ordering::AcqRel);
            }
            // `total <= limit <= u32::MAX`, so the narrowing is exact.
            let next = pack(window as u32, total as u32);
            if self
                .state
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Decision::Allow;
            }
            core::hint::spin_loop();
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

    #[test]
    fn test_huge_limit_sum_does_not_overflow_or_over_admit() {
        // Two requests of 3e9 against a u32::MAX limit: the sum (6e9) exceeds
        // u32::MAX, which used to panic in debug and wrap in release.
        let fw = FixedWindow::new(&Quota::per_second(u32::MAX), at(0));
        assert!(fw.acquire(3_000_000_000, at(0)).is_allow());
        assert!(fw.acquire(3_000_000_000, at(0)).is_deny());
        // The remaining 1_294_967_295 units still fit exactly.
        assert!(fw.acquire(u32::MAX - 3_000_000_000, at(0)).is_allow());
        assert!(fw.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_late_thread_counts_against_newer_window() {
        let fw = FixedWindow::new(&Quota::per_second(10), at(0));
        // Window 1 is opened and filled by up-to-date threads.
        for _ in 0..10 {
            assert!(fw.acquire(1, at(1_000)).is_allow());
        }
        // A late thread still carrying a window-0 timestamp must not roll the
        // window back: window 1 is full, so it is denied...
        assert!(fw.acquire(1, at(999)).is_deny());
        // ...and window 1 still holds its count for everyone else.
        assert!(fw.acquire(1, at(1_500)).is_deny());
        assert!(fw.acquire(1, at(2_000)).is_allow());
    }

    #[test]
    fn test_late_thread_retry_after_targets_newer_window_end() {
        let fw = FixedWindow::new(&Quota::per_second(1), at(0));
        assert!(fw.acquire(1, at(1_000)).is_allow());
        // A late thread at t=900 must wait for window 1 to end (t=2000).
        assert_eq!(fw.acquire(1, at(900)).retry_after(), Some(at(1_100)));
    }

    #[test]
    fn test_zero_period_denies_everything() {
        let quota = Quota::per_second(10);
        let degenerate = crate::quota::Quota::from_parts(quota.limit(), Duration::ZERO, 10);
        let fw = FixedWindow::new(&degenerate, at(0));
        assert_eq!(fw.acquire(1, at(0)).retry_after(), Some(Duration::MAX));
        assert_eq!(fw.acquire(0, at(0)), Decision::Allow);
    }

    #[test]
    fn test_zero_limit_denies_everything() {
        let fw = FixedWindow::new(&Quota::per_second(0), at(0));
        assert!(fw.acquire(1, at(0)).is_deny());
    }

    #[test]
    fn test_index_wrap_does_not_carry_a_stale_count() {
        // A 1ns window wraps the 32-bit index after 2^32 ns (~4.3s). A key idle
        // across exactly one wrap must start its window fresh.
        let quota = crate::quota::Quota::from_parts(1, Duration::from_nanos(1), 1);
        let fw = FixedWindow::new(&quota, Duration::ZERO);
        assert!(fw.acquire(1, Duration::from_nanos(5)).is_allow());
        assert!(fw.acquire(1, Duration::from_nanos(5)).is_deny());
        let wrapped = Duration::from_nanos(5 + (1u64 << 32));
        assert!(fw.acquire(1, wrapped).is_allow());
    }
}
