//! Tier-1: limit a key to N requests per second.
//!
//! ```text
//! cargo run --example per_second
//! ```

use rate_net::{Decision, RateLimiter};

fn main() {
    // Five requests per second, per key.
    let limiter = RateLimiter::per_second(5);

    let mut allowed = 0u32;
    let mut denied = 0u32;
    for i in 0..8 {
        match limiter.check("user:42") {
            Decision::Allow => {
                allowed += 1;
                println!("request {i}: allowed");
            }
            Decision::Deny { retry_after } => {
                denied += 1;
                println!("request {i}: denied (retry after {retry_after:?})");
            }
            _ => {}
        }
    }

    println!("\n{allowed} allowed, {denied} denied against a 5/s limit");
}
