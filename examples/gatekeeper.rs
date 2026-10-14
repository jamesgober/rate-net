//! A minimal HTTP-style gatekeeper built on rate-net — the shape a service such
//! as `bouncer-io` wraps. It limits per client network (exact IPv4 addresses,
//! IPv6 `/64`s) and turns a denial into a `429 Too Many Requests` with a
//! `Retry-After`, using only the public API.
//!
//! ```text
//! cargo run --example gatekeeper
//! ```

use rate_net::{Key, RateLimiter};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
    /// seconds (rounded up).
    fn handle(&self, client: IpAddr) -> (u16, Option<u64>) {
        // One allowance per IPv4 address and per IPv6 /64, so a client cannot
        // rotate through its own subnet to reset its limit.
        let decision = self.limiter.check(Key::ip_prefix(client, 32, 64));
        if decision.is_allow() {
            (200, None)
        } else {
            (429, decision.retry_after_secs())
        }
    }
}

fn main() {
    // Three requests per second, per client network.
    let gate = Gatekeeper::new(3);
    let v4 = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    let v6_a = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, 1));
    let v6_b = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0xdead, 0xbeef, 0, 2));

    for (i, client) in [v4, v4, v4, v4, v6_a, v6_b, v6_a, v6_b]
        .into_iter()
        .enumerate()
    {
        match gate.handle(client) {
            (200, _) => println!("request {i} from {client}: 200 OK"),
            (429, Some(secs)) => {
                println!("request {i} from {client}: 429 Too Many Requests (Retry-After: {secs})");
            }
            (code, _) => println!("request {i} from {client}: {code}"),
        }
    }
}
