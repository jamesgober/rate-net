//! Awaiting until a key is allowed, instead of shedding the request.
//!
//! Requires the `async` feature:
//!
//! ```text
//! cargo run --example async_wait --features async
//! ```

use rate_net::{AsyncLimiter, RateLimiter};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Five per second → a token refills roughly every 200ms.
    let limiter = AsyncLimiter::new(RateLimiter::per_second(5));

    // Spend the whole allowance up front.
    for _ in 0..5 {
        let _ = limiter.check("job-queue");
    }
    println!("allowance spent; awaiting a refill...");

    let start = std::time::Instant::now();
    limiter.until_ready("job-queue").await;
    println!("admitted after {:?}", start.elapsed());
}
