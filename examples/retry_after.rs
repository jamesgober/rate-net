//! Turning a denial into an HTTP `429 Too Many Requests` with a `Retry-After`.
//!
//! ```text
//! cargo run --example retry_after
//! ```

use rate_net::{Decision, RateLimiter};

/// Maps a check to an HTTP status and an optional `Retry-After` value (whole
/// seconds, the header's unit).
fn respond(limiter: &RateLimiter, key: &str) -> (u16, Option<u64>) {
    match limiter.check(key) {
        Decision::Allow => (200, None),
        Decision::Deny { retry_after } => {
            // Round up to at least one second so the client always backs off.
            (429, Some(retry_after.as_secs().max(1)))
        }
        _ => (429, None),
    }
}

fn main() {
    // Two requests per minute makes the wait easy to see.
    let limiter = RateLimiter::per_minute(2);

    for i in 0..4 {
        match respond(&limiter, "client:7") {
            (status, Some(retry)) => {
                println!("request {i}: HTTP {status}  Retry-After: {retry}");
            }
            (status, None) => println!("request {i}: HTTP {status}"),
        }
    }
}
