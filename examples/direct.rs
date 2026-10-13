//! Pacing one stream with a `DirectLimiter`: no keys, no map, no lock — the
//! cheapest check the crate offers, for client-side throttling.
//!
//! ```text
//! cargo run --example direct
//! ```

use rate_net::{DirectLimiter, Quota};
use std::time::{Duration, Instant};

fn main() {
    // An upstream API allows 20 calls per second with bursts of 5.
    let limiter = DirectLimiter::with_quota(Quota::per_second(20).with_burst(5));
    let start = Instant::now();

    let mut sent = 0;
    while sent < 12 {
        let decision = limiter.check();
        if decision.is_allow() {
            sent += 1;
            println!("{:>4} ms  call {sent}", start.elapsed().as_millis());
        } else if let Some(wait) = decision.retry_after() {
            // Sleep exactly as long as the limiter says, then try again.
            std::thread::sleep(wait.min(Duration::from_millis(100)));
        }
    }

    let snapshot = limiter.peek();
    println!(
        "done: {} of {} left, full again in {:?}",
        snapshot.remaining(),
        snapshot.limit(),
        snapshot.reset_after()
    );
}
