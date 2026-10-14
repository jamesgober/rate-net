//! Rate-limiting an HTTP service with the `tower` middleware.
//!
//! Wraps a `tower::Service` (here a `service_fn`; in an application, an axum
//! `Router` or a hyper service) in a `RateLimitLayer` keyed by the client
//! address a trusted load balancer reports in `X-Forwarded-For`, and sends a
//! burst of requests from two clients through it. Over-limit requests get a
//! `429` with `Retry-After` and the `RateLimit` headers, without ever reaching
//! the inner service.
//!
//! Run with `cargo run --example tower_layer --features tower`.

use std::convert::Infallible;

use http::{Request, Response};
use rate_net::RateLimiter;
use rate_net::tower::{ForwardedFor, RateLimitLayer};
use tower::{Layer, Service, ServiceExt, service_fn};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Three requests per minute per client, one load balancer in front.
    let layer = RateLimitLayer::new(RateLimiter::per_minute(3), ForwardedFor::trusting(1))
        .with_policy("per-client")
        .with_headers_on_success(true);
    let mut service = layer.layer(service_fn(|_: Request<String>| async {
        Ok::<_, Infallible>(Response::new(String::from("hello")))
    }));

    for client in [
        "203.0.113.7",
        "203.0.113.7",
        "198.51.100.9",
        "203.0.113.7",
        "203.0.113.7",
    ] {
        let request = Request::get("/api")
            // The client may send any X-Forwarded-For it likes; the balancer
            // appends the address it actually saw, which is the one keyed.
            .header("x-forwarded-for", format!("10.9.9.9, {client}"))
            .body(String::new())
            .expect("a valid request");
        let response = match service.ready().await {
            Ok(ready) => ready.call(request).await,
            Err(never) => match never {},
        };
        let Ok(response) = response;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-")
                .to_owned()
        };
        // Printing is the point of the example.
        #[allow(clippy::print_stdout)]
        {
            println!(
                "{client:<14} -> {} remaining={} retry-after={} ratelimit={}",
                response.status(),
                header("ratelimit-remaining"),
                header("retry-after"),
                header("ratelimit"),
            );
        }
    }
}
