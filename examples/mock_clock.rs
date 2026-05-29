//! Deterministic testing: drive refills with a `ManualClock`, no real sleeping.
//!
//! ```text
//! cargo run --example mock_clock
//! ```

use clock_lib::ManualClock;
use rate_net::RateLimiter;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    // Share a manual clock with the limiter so the test drives time by hand.
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::per_second(3).with_clock(Arc::clone(&clock));

    for i in 1..=3 {
        assert!(limiter.check("k").is_allow());
        println!("request {i}: allowed");
    }
    assert!(limiter.check("k").is_deny());
    println!("request 4: denied (allowance spent)");

    // Advance one second — instantly, with no thread::sleep.
    clock.advance(Duration::from_secs(1));
    println!("\n-- advanced clock by 1s (no real time elapsed) --\n");

    assert!(limiter.check("k").is_allow());
    println!("request 5: allowed (allowance refilled)");
}
