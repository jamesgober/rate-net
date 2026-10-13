//! A fast, coarse monotonic clock.

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
/// A reading is never ahead of real time; it lags by at most one tick interval
/// plus however late the ticker runs. A lagging clock makes a limiter slightly
/// *stricter*: refills and window rollovers happen up to one tick late. Over any
/// span, a limiter on a `CoarseClock` admits no more than the quota allows over
/// that span plus one tick — for a 1 ms tick, at most `rate x 1 ms` extra once,
/// never accumulating. A 1 ms tick costs nothing measurable for limits
/// expressed per second or per minute; prefer [`SystemClock`] when decisions
/// need sub-millisecond precision.
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
    /// Real-time origin, captured just *after* the manual clock's anchor, so
    /// `anchor + elapsed-since-origin` never runs ahead of real time.
    origin: Instant,
    /// Nanoseconds since `origin` already applied to `clock`. Claimed by
    /// compare-and-swap, so concurrent ticks never apply the same span twice.
    applied_ns: AtomicU64,
}

impl Inner {
    fn new() -> Self {
        let clock = ManualClock::new();
        Self {
            clock,
            origin: Instant::now(),
            applied_ns: AtomicU64::new(0),
        }
    }

    fn tick(&self) {
        let target = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
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
            inner: Arc::new(Inner::new()),
        }
    }

    /// A coarse clock ticked every `resolution` by a background thread.
    ///
    /// `resolution` is raised to at least [`MIN_COARSE_RESOLUTION`]. The thread
    /// holds only a weak reference: it exits within one tick of the last clone
    /// of the clock being dropped. Clone one clock into every limiter rather
    /// than spawning one per limiter.
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
        let clock = Self::new();
        let weak: Weak<Inner> = Arc::downgrade(&clock.inner);
        let interval = resolution.max(MIN_COARSE_RESOLUTION);
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
        let applied = Duration::from_nanos(self.inner.applied_ns.load(Ordering::Acquire));
        self.inner.origin.elapsed().saturating_sub(applied)
    }
}

impl Default for CoarseClock {
    /// Same as [`CoarseClock::new`]: no ticker.
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for CoarseClock {
    /// The time of the last tick: one relaxed load, no system call.
    #[inline]
    fn now(&self) -> Monotonic {
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

    use super::{CoarseClock, MIN_COARSE_RESOLUTION};

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

    #[test]
    fn test_spawned_ticker_keeps_up_and_stops_with_the_last_clone() {
        let clock = CoarseClock::spawn(Duration::ZERO).unwrap();
        let start = clock.now();
        std::thread::sleep(Duration::from_millis(30));
        assert!(clock.now() > start, "the ticker never ticked");
        // Generous: CI schedulers can be late, but not by half a second.
        assert!(
            clock.lag() < Duration::from_millis(500),
            "lag {:?}",
            clock.lag()
        );
        let weak = std::sync::Arc::downgrade(&clock.inner);
        drop(clock);
        std::thread::sleep(MIN_COARSE_RESOLUTION * 20);
        assert!(weak.upgrade().is_none());
    }
}
