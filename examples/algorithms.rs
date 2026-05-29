//! Comparing the algorithms on the same quota and traffic.
//!
//! Requires the `algorithms` feature:
//!
//! ```text
//! cargo run --example algorithms --features algorithms
//! ```

use rate_net::{Algorithm, RateLimiter};
use std::time::Duration;

fn main() {
    let algorithms = [
        Algorithm::TokenBucket,
        Algorithm::LeakyBucket,
        Algorithm::FixedWindow,
        Algorithm::SlidingWindowLog,
        Algorithm::SlidingWindowCounter,
    ];

    // Five per second; fire ten requests at the same instant and see how many
    // each algorithm admits in the opening burst.
    for algorithm in algorithms {
        let limiter = RateLimiter::builder()
            .algorithm(algorithm)
            .quota(5, Duration::from_secs(1))
            .build();

        let admitted = (0..10).filter(|_| limiter.check("k").is_allow()).count();
        println!("{algorithm:?}: admitted {admitted}/10 at t=0 (limit 5/s)");
    }
}
