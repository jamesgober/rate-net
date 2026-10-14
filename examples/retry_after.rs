//! Turning a denial into an HTTP `429 Too Many Requests` with a `Retry-After`,
//! plus the `RateLimit-*` headers on every response.
//!
//! ```text
//! cargo run --example retry_after
//! ```

use rate_net::{RateLimiter, headers};

/// Maps a check to an HTTP status and the rate-limit headers to send with it.
fn respond(limiter: &RateLimiter, key: &str) -> (u16, Vec<(&'static str, u64)>) {
    let decision = limiter.check(key);
    // The RateLimit-* headers describe the key after this request.
    let mut response_headers = limiter.peek(key).headers().to_vec();
    if decision.is_allow() {
        return (200, response_headers);
    }
    if decision.is_impossible() {
        // The request can never fit the limit: retrying is pointless.
        return (413, response_headers);
    }
    // Whole seconds, rounded up, so the client never retries too early.
    if let Some(secs) = decision.retry_after_secs() {
        response_headers.push((headers::RETRY_AFTER, secs));
    }
    (429, response_headers)
}

fn main() {
    // Two requests per minute makes the wait easy to see.
    let limiter = RateLimiter::per_minute(2);

    for i in 0..4 {
        let (status, response_headers) = respond(&limiter, "client:7");
        let rendered: Vec<String> = response_headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        println!("request {i}: HTTP {status}  {}", rendered.join("  "));
    }
}
