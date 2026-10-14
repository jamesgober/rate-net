//! A fast, coarse monotonic clock.

use core::cell::Cell;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Instant;

use clock_lib::{Clock, ManualClock, Monotonic, Wall};

/// The finest tick [`CoarseClock::spawn`] accepts. Shorter requests are raised
/// to it, so the ticker can never spin a core.
pub const MIN_COARSE_RESOLUTION: Duration = Duration::from_micros(100);

/// How often (in reads, per thread) a spawned clock checks whether it has
/// fallen behind. One real clock read per this many coarse reads costs well
/// under a nanosecond per reading, amortized.
///
/// The counter is per thread, shared by every `CoarseClock` the thread reads,
/// and the check refreshes whichever clock the due read was of. The period is
/// prime so that a thread rotating through several clocks in a fixed order (any
/// cycle shorter than 61) lands its due reads on each of them in turn; with a
/// power of two, a thread alternating two clocks would only ever refresh one.
const READS_PER_REFRESH: u32 = 61;

thread_local! {
    /// Coarse reads on this thread (of any `CoarseClock`) since its last
    /// staleness check.
    static READS: Cell<u32> = const { Cell::new(0) };
}

/// A monotonic [`Clock`] that is cheap to read because it only moves when it is
/// ticked.
///
/// Reading the OS monotonic clock (`Instant::now`) is the largest single cost
/// in a token-bucket check — about 26 ns on the reference Windows machine, and
/// in the 15–25 ns range on Linux. `CoarseClock` replaces that system call with
/// one relaxed atomic load: a reading costs about 4 ns. The price is
/// resolution. The clock holds the time of its last *tick*, and ticks come from
/// either
///
/// - a background thread, with [`CoarseClock::spawn`] — one thread per clock
///   (share the clock, it is `Clone`), which exits on its own once every clone
///   is dropped; or
/// - your own code, with [`tick`](Self::tick) — from an existing timer, an
///   event loop, or a tokio interval — for a clock with no thread at all.
///
/// # What coarseness means for limiting
///
/// A reading is never ahead of real time, but it lags it. Call the lag at any
/// moment `L` (see [`lag`](Self::lag)). Lag is not free: a limiter credits
/// refill by the clock's readings, so when a lagging clock catches up it
/// credits the lagged time too. The exact looseness is: **over any span of real
/// time, a limiter on a `CoarseClock` admits at most what the quota allows over
/// that span plus `rate × L`, where `L` is the lag at the start of the span.**
/// It never accumulates — the extra is bounded by the lag, not by how long the
/// clock runs — and with no lag the bound is the quota's own.
///
/// How large `L` can get depends on the mode:
///
/// - **Spawned** ([`spawn`](Self::spawn)): one tick interval plus however late
///   the ticker thread runs. A starved ticker (a machine with every core busy)
///   cannot stretch this indefinitely: every 61st read on a thread re-reads the
///   real clock, and if the clock read then has fallen a whole interval behind
///   it ticks it. On any thread that reads the clock steadily, `L` stays within
///   one interval plus the time of 61 reads. (The count is per thread, across
///   all coarse clocks it reads; see [`spawn`](Self::spawn).)
/// - **Manual** ([`new`](Self::new)): exactly the time since the last
///   [`tick`](Self::tick). The clock never moves on its own; tick it at least
///   as often as the looseness you can accept.
///
/// A 1 ms interval adds at most `rate × 1 ms` (one unit at 1,000/s) for limits
/// expressed per second or per minute. Prefer [`SystemClock`] when decisions
/// must be exact to the nanosecond.
///
/// [`SystemClock`]: clock_lib::SystemClock
///
/// # Examples
///
/// ```
/// use rate_net::{CoarseClock, RateLimiter};
/// use std::time::Duration;
///
/// // One 1 ms ticker thread, shared by every limiter that clones the clock.
/// let clock = CoarseClock::spawn(Duration::from_millis(1))?;
/// let limiter = RateLimiter::per_second(1_000).with_clock(clock.clone());
/// assert!(limiter.check("user:42").is_allow());
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// Ticking by hand instead of with a thread:
///
/// ```
/// use clock_lib::Clock;
/// use rate_net::CoarseClock;
///
/// let clock = CoarseClock::new();
/// let before = clock.now();
/// std::thread::sleep(std::time::Duration::from_millis(2));
/// assert_eq!(clock.now(), before); // frozen until ticked
/// clock.tick();
/// assert!(clock.now() > before);
/// ```
#[derive(Clone)]
pub struct CoarseClock {
    inner: Arc<Inner>,
}

struct Inner {
    /// Readings come from here: its anchor plus the offset applied so far.
    clock: ManualClock,
    /// For a spawned clock, the lag (ns) at which a reading thread ticks the
    /// clock itself; `0` for a manual clock, which never moves on its own.
    refresh_after_ns: u64,
    /// Real-time origin, captured just *after* the manual clock's anchor, so
    /// `anchor + elapsed-since-origin` never runs ahead of real time.
    origin: Instant,
    /// Nanoseconds since `origin` already applied to `clock`. Claimed by
    /// compare-and-swap, so concurrent ticks never apply the same span twice.
    applied_ns: AtomicU64,
}

impl Inner {
    fn new(refresh_after_ns: u64) -> Self {
        let clock = ManualClock::new();
        Self {
            clock,
            refresh_after_ns,
            origin: Instant::now(),
            applied_ns: AtomicU64::new(0),
        }
    }

    fn tick(&self) {
        let target = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.advance_to(target);
    }

    /// Ticks only if the clock has fallen `refresh_after_ns` behind, so a
    /// steady stream of readers does not contend on the clock's state.
    fn tick_if_stale(&self) {
        let target = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let applied = self.applied_ns.load(Ordering::Relaxed);
        if target.saturating_sub(applied) >= self.refresh_after_ns {
            self.advance_to(target);
        }
    }

    /// Advances the clock to `target` ns past the origin, if it is behind.
    fn advance_to(&self, target: u64) {
        let mut applied = self.applied_ns.load(Ordering::Relaxed);
        while target > applied {
            match self.applied_ns.compare_exchange_weak(
                applied,
                target,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // This tick owns the span `applied..target`.
                    self.clock.advance(Duration::from_nanos(target - applied));
                    return;
                }
                Err(observed) => applied = observed,
            }
        }
    }
}

impl CoarseClock {
    /// A coarse clock with no ticker: it reads the time of its construction
    /// until [`tick`](Self::tick) is called.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::CoarseClock;
    ///
    /// let clock = CoarseClock::new();
    /// clock.tick();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new(0)),
        }
    }

    /// A clock that ticks itself from its readers once `refresh_after` behind,
    /// with no ticker thread. The building block of [`spawn`](Self::spawn).
    fn self_refreshing(refresh_after: Duration) -> Self {
        let refresh_after_ns = u64::try_from(refresh_after.as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        Self {
            inner: Arc::new(Inner::new(refresh_after_ns)),
        }
    }

    /// A coarse clock ticked every `resolution` by a background thread.
    ///
    /// `resolution` is raised to at least [`MIN_COARSE_RESOLUTION`]. The thread
    /// holds only a weak reference: it exits within one tick of the last clone
    /// of the clock being dropped. Clone one clock into every limiter rather
    /// than spawning one per limiter.
    ///
    /// Readers also keep a spawned clock fresh when its ticker is starved:
    /// every 61st coarse read on a thread checks the real clock and ticks the
    /// clock being read if it is a whole `resolution` behind. That count is
    /// kept per thread across every spawned clock the thread reads, so a
    /// thread reading several clocks refreshes each in proportion to how often
    /// it reads it — one more reason to share a single clock.
    ///
    /// # Errors
    ///
    /// Returns the OS error if the ticker thread cannot be spawned.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::CoarseClock;
    /// use std::time::Duration;
    ///
    /// let clock = CoarseClock::spawn(Duration::from_millis(1))?;
    /// std::thread::sleep(Duration::from_millis(20));
    /// assert!(clock.lag() < Duration::from_millis(500));
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn spawn(resolution: Duration) -> std::io::Result<Self> {
        let interval = resolution.max(MIN_COARSE_RESOLUTION);
        let clock = Self::self_refreshing(interval);
        let weak: Weak<Inner> = Arc::downgrade(&clock.inner);
        let _ticker = thread::Builder::new()
            .name("rate-net-coarse-clock".into())
            .spawn(move || {
                loop {
                    thread::sleep(interval);
                    match weak.upgrade() {
                        Some(inner) => inner.tick(),
                        None => break,
                    }
                }
            })?;
        Ok(clock)
    }

    /// Brings the clock up to the current real time. Cheap enough to call from
    /// any periodic hook; concurrent ticks are safe.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::CoarseClock;
    /// use std::time::Duration;
    ///
    /// let clock = CoarseClock::new();
    /// std::thread::sleep(Duration::from_millis(5));
    /// clock.tick();
    /// assert!(clock.lag() < Duration::from_millis(5));
    /// ```
    pub fn tick(&self) {
        self.inner.tick();
    }

    /// How far the clock currently trails real time — the staleness of its
    /// readings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::CoarseClock;
    /// use std::time::Duration;
    ///
    /// let clock = CoarseClock::new();
    /// std::thread::sleep(Duration::from_millis(5));
    /// assert!(clock.lag() >= Duration::from_millis(5));
    /// ```
    #[must_use]
    pub fn lag(&self) -> Duration {
        // The offset readings actually carry (a tick that has claimed a span
        // but not yet applied it still counts as lag).
        self.inner
            .origin
            .elapsed()
            .saturating_sub(self.inner.clock.offset())
    }
}

impl Default for CoarseClock {
    /// Same as [`CoarseClock::new`]: no ticker.
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for CoarseClock {
    /// The time of the last tick: one relaxed load and, for a spawned clock, a
    /// thread-local counter (every 61st read checks the real clock and ticks
    /// if a whole interval behind).
    #[inline]
    fn now(&self) -> Monotonic {
        if self.inner.refresh_after_ns != 0 {
            let due = READS.with(|reads| {
                let next = reads.get().wrapping_add(1);
                reads.set(next);
                next % READS_PER_REFRESH == 0
            });
            if due {
                self.inner.tick_if_stale();
            }
        }
        self.inner.clock.now()
    }

    /// The wall-clock time of the last tick.
    fn wall(&self) -> Wall {
        self.inner.clock.wall()
    }
}

impl fmt::Debug for CoarseClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoarseClock")
            .field("lag", &self.lag())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::time::{Duration, Instant};

    use clock_lib::{Clock, SystemClock};

    use super::CoarseClock;

    #[test]
    fn test_manual_coarse_clock_moves_only_when_ticked() {
        let clock = CoarseClock::default();
        let first = clock.now();
        std::thread::sleep(Duration::from_millis(3));
        assert_eq!(clock.now(), first);
        assert!(clock.lag() >= Duration::from_millis(3));
        clock.tick();
        let second = clock.now();
        assert!(second.duration_since(first) >= Duration::from_millis(3));
        // Ticking twice without time passing never moves it backwards.
        clock.tick();
        assert!(clock.now() >= second);
        let _ = clock.wall();
        assert!(format!("{clock:?}").contains("CoarseClock"));
    }

    #[test]
    fn test_coarse_clock_never_runs_ahead_of_real_time() {
        let system = SystemClock::new();
        let start_real = system.now();
        let clock = CoarseClock::new();
        let start = clock.now();
        for _ in 0..50 {
            clock.tick();
            let coarse_elapsed = clock.now().duration_since(start);
            let real_elapsed = system.now().duration_since(start_real);
            assert!(coarse_elapsed <= real_elapsed);
        }
    }

    #[test]
    fn test_concurrent_ticks_apply_each_span_once() {
        let clock = CoarseClock::new();
        let real_start = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let clock = clock.clone();
                let _ticker = scope.spawn(move || {
                    for _ in 0..1_000 {
                        clock.tick();
                    }
                });
            }
        });
        let applied = Duration::from_nanos(
            clock
                .inner
                .applied_ns
                .load(std::sync::atomic::Ordering::Acquire),
        );
        assert!(applied <= real_start.elapsed() + Duration::from_millis(1));
    }

    /// Polls `condition` until it holds or `deadline` passes.
    fn eventually(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        condition()
    }

    #[test]
    fn test_spawned_ticker_keeps_up_and_stops_with_the_last_clone() {
        let clock = CoarseClock::spawn(Duration::ZERO).unwrap();
        let start = clock.inner.clock.now();
        // Read the inner clock directly: this checks the ticker thread, not
        // the readers' self-refresh.
        assert!(
            eventually(Duration::from_secs(10), || clock.inner.clock.now() > start),
            "the ticker never ticked"
        );
        let weak = std::sync::Arc::downgrade(&clock.inner);
        drop(clock);
        assert!(
            eventually(Duration::from_secs(10), || weak.upgrade().is_none()),
            "the ticker kept the clock alive"
        );
    }

    #[test]
    fn test_readers_refresh_a_stale_clock_without_any_ticker() {
        // A self-refreshing clock with no ticker thread at all: only reads
        // can move it, as when a ticker is starved of CPU.
        let clock = CoarseClock::self_refreshing(Duration::from_millis(1));
        let start = clock.inner.clock.now();
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..(super::READS_PER_REFRESH * 2) {
            let _ = clock.now();
        }
        // The real clock moved 20ms, past the 1ms refresh threshold, so some
        // read among the last 128 ticked it at least 20ms forward.
        assert!(clock.inner.clock.now().duration_since(start) >= Duration::from_millis(20));
    }

    #[test]
    fn test_a_thread_alternating_two_clocks_refreshes_both() {
        // With a power-of-two period a strict alternation landed every due
        // read on the same clock; the prime period reaches both.
        let a = CoarseClock::self_refreshing(Duration::from_millis(1));
        let b = CoarseClock::self_refreshing(Duration::from_millis(1));
        let (a0, b0) = (a.inner.clock.now(), b.inner.clock.now());
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..(super::READS_PER_REFRESH * 4) {
            let _ = a.now();
            let _ = b.now();
        }
        assert!(a.inner.clock.now().duration_since(a0) >= Duration::from_millis(20));
        assert!(b.inner.clock.now().duration_since(b0) >= Duration::from_millis(20));
    }

    #[test]
    fn test_manual_clock_never_refreshes_itself() {
        let clock = CoarseClock::new();
        let start = clock.now();
        std::thread::sleep(Duration::from_millis(3));
        for _ in 0..(super::READS_PER_REFRESH * 4) {
            assert_eq!(clock.now(), start);
        }
    }
}
