//! The rate-limited `tracing` warnings an operator gets when the store runs
//! out of room for evicted keys' debts.
//!
//! One test, run alone in its own binary: `tracing` caches each call site's
//! interest process-wide, and a concurrent test hitting the same call sites
//! with no subscriber can race that cache.

#![cfg(all(feature = "metrics", feature = "tracing"))]

use clock_lib::ManualClock;
use rate_net::{Eviction, RateLimiter};

fn limiter(overflow: Overflow) -> (Arc<ManualClock>, RateLimiter<Arc<ManualClock>>) {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::builder()
        .per_minute(1)
        .shards(1)
        .eviction(Eviction::capacity(64).with_overflow(overflow))
        .clock(Arc::clone(&clock))
        .build();
    (clock, limiter)
}

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rate_net::Overflow;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

/// Collects the messages of warnings from `rate_net::eviction`.
#[derive(Clone, Default)]
struct Collect(Arc<Mutex<Vec<String>>>);

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        }
    }
}

impl Subscriber for Collect {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        if *metadata.level() == Level::WARN && metadata.target() == "rate_net::eviction" {
            let mut message = Message(String::new());
            event.record(&mut message);
            self.0.lock().unwrap().push(message.0);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn count(warnings: &Collect, needle: &str) -> usize {
    warnings
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|message| message.contains(needle))
        .count()
}

fn saturation_warns_once_then_at_most_once_a_minute() {
    let warnings = Collect::default();
    tracing::subscriber::with_default(warnings.clone(), || {
        let (clock, limiter) = limiter(Overflow::FailClosed);
        // Thousands of spills and refusals at one instant: one warning of
        // each kind.
        for key in 0..5_000u64 {
            let _ = limiter.check(key);
        }
        assert!(limiter.metrics().keys_refused > 100);
        assert_eq!(count(&warnings, "saturated"), 1);
        assert_eq!(count(&warnings, "spilled"), 1);
        // Thirty seconds on: still rate-limited.
        clock.advance(Duration::from_secs(30));
        for key in 5_000..6_000u64 {
            let _ = limiter.check(key);
        }
        assert_eq!(count(&warnings, "saturated"), 1);
        // Past the interval, the next refusal warns again, with the count
        // since the last warning.
        clock.advance(Duration::from_secs(31));
        for key in 6_000..12_000u64 {
            let _ = limiter.check(key);
        }
        assert_eq!(count(&warnings, "saturated"), 2);
    });
    assert_eq!(count(&warnings, "dropped"), 0);
}

fn forget_warns_when_it_drops_debts() {
    let warnings = Collect::default();
    tracing::subscriber::with_default(warnings.clone(), || {
        let (_clock, limiter) = limiter(Overflow::Forget);
        for key in 0..5_000u64 {
            let _ = limiter.check(key);
        }
    });
    assert_eq!(count(&warnings, "dropped"), 1);
    assert_eq!(
        count(&warnings, "saturated") + count(&warnings, "spilled"),
        0
    );
}

fn a_store_with_room_never_warns() {
    let warnings = Collect::default();
    tracing::subscriber::with_default(warnings.clone(), || {
        let (_clock, limiter) = limiter(Overflow::FailClosed);
        for key in 0..100u64 {
            let _ = limiter.check(key);
        }
    });
    assert!(warnings.0.lock().unwrap().is_empty());
}

#[test]
fn test_saturation_warnings_are_rate_limited_and_specific() {
    a_store_with_room_never_warns();
    saturation_warns_once_then_at_most_once_a_minute();
    forget_warns_when_it_drops_debts();
}
