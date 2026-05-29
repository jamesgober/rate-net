//! Per-key limiting: every IP, user, or endpoint gets its own allowance.
//!
//! ```text
//! cargo run --example per_key
//! ```

use rate_net::RateLimiter;
use std::net::{IpAddr, Ipv4Addr};

fn main() {
    // Two requests per second, applied independently per key.
    let limiter = RateLimiter::per_second(2);

    let alice = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    let bob = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9));

    // Alice spends her whole allowance.
    println!("alice #1: {}", verdict(limiter.check(alice).is_allow()));
    println!("alice #2: {}", verdict(limiter.check(alice).is_allow()));
    println!("alice #3: {}", verdict(limiter.check(alice).is_allow()));

    // Bob is on a different key, so he is untouched by Alice's traffic.
    println!("bob   #1: {}", verdict(limiter.check(bob).is_allow()));

    // String keys work the same way — per user, per tenant, per endpoint.
    println!("user:42 : {}", verdict(limiter.check("user:42").is_allow()));
    println!(
        "GET /api: {}",
        verdict(limiter.check("GET /api").is_allow())
    );
}

fn verdict(allowed: bool) -> &'static str {
    if allowed { "allowed" } else { "denied" }
}
