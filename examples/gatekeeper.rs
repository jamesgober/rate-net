//! A minimal HTTP-style gatekeeper built on rate-net — the shape a service such
//! as `bouncer-io` wraps. It limits per client IP and turns a denial into a
//! `429 Too Many Requests` with a `Retry-After`, using only the public API.
//!
//! ```text
//! cargo run --example gatekeeper
//! ```

use rate_net::{Decision, RateLimiter};
use std::net::{IpAddr, Ipv4Addr};

/// Wraps a limiter and exposes an HTTP-shaped verdict. The gatekeeper never
/// touches rate-net's internals — it only constructs a limiter and calls
/// `check`.
struct Gatekeeper {
    limiter: RateLimiter,
}

impl Gatekeeper {
    fn new(per_second: u32) -> Self {
        Self {
            limiter: RateLimiter::per_second(per_second),
        }
    }

    /// Returns an HTTP status and, on a denial, the `Retry-After` in whole
    /// seconds.
    fn handle(&self, client: IpAddr) -> (u16, Option<u64>) {
        match self.limiter.check(client) {
            Decision::Allow => (200, None),
            Decision::Deny { retry_after } => (429, Some(retry_after.as_secs().max(1))),
            _ => (429, None),
        }
    }
}

fn main() {
    // Three requests per second, per client IP.
    let gate = Gatekeeper::new(3);
    let client = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    for i in 1..=5 {
        match gate.handle(client) {
            (200, _) => println!("request {i} from {client}: 200 OK"),
            (429, Some(secs)) => {
                println!("request {i} from {client}: 429 Too Many Requests (Retry-After: {secs})");
            }
            (code, _) => println!("request {i} from {client}: {code}"),
        }
    }
}
