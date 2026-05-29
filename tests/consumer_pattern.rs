//! First-consumer shake-out.
//!
//! Exercises the public surface the way a gatekeeper (e.g. `bouncer-io`) will:
//! hold the limiter behind an `Arc`, code against the [`Limiter`] trait rather
//! than the concrete type, key by caller identity, and turn a [`Decision`] into
//! the gatekeeper's own allow/deny with a retry hint. The consumer touches
//! **only** the public API — never any internal state — and the point is to
//! surface any friction before the real integration. If these read naturally,
//! the surface is consumable.

#![cfg(feature = "std")]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use clock_lib::ManualClock;
use rate_net::{Decision, Limiter, RateLimiter};

/// The verdict a gatekeeper returns to its caller — the shape a downstream like
/// `bouncer-io` surfaces. Built purely from rate-net's public [`Decision`].
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Serve,
    TooManyRequests { retry_after_secs: u64 },
}

/// A gatekeeper's admission check, coded against the [`Limiter`] **trait** — proof
/// the surface is consumable generically, without naming the concrete limiter or
/// its clock type. The wildcard arm honours `Decision`'s `#[non_exhaustive]`.
fn admit<L: Limiter>(limiter: &L, key: impl Into<rate_net::Key>) -> Verdict {
    match limiter.check(key) {
        Decision::Allow => Verdict::Serve,
        Decision::Deny { retry_after } => Verdict::TooManyRequests {
            retry_after_secs: retry_after.as_secs().max(1),
        },
        _ => Verdict::TooManyRequests {
            retry_after_secs: 1,
        },
    }
}

#[test]
fn test_gatekeeper_consumes_through_the_trait() {
    let clock = Arc::new(ManualClock::new());
    let limiter = RateLimiter::per_second(2).with_clock(Arc::clone(&clock));

    assert_eq!(admit(&limiter, "client:7"), Verdict::Serve);
    assert_eq!(admit(&limiter, "client:7"), Verdict::Serve);
    // Drained — denied with a usable retry hint.
    assert_eq!(
        admit(&limiter, "client:7"),
        Verdict::TooManyRequests {
            retry_after_secs: 1
        }
    );
    // Wait it out; the hint is honest.
    clock.advance(Duration::from_secs(1));
    assert_eq!(admit(&limiter, "client:7"), Verdict::Serve);
}

#[test]
fn test_per_identity_isolation() {
    let limiter = RateLimiter::per_second(1);

    // Per-IP and per-user keys are independent — one caller's flood never
    // affects another.
    let attacker = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    assert_eq!(admit(&limiter, attacker), Verdict::Serve);
    assert!(matches!(
        admit(&limiter, attacker),
        Verdict::TooManyRequests { .. }
    ));

    let other = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1));
    assert_eq!(admit(&limiter, other), Verdict::Serve);
    assert_eq!(admit(&limiter, "user:42"), Verdict::Serve);
}

#[test]
fn test_shared_gatekeeper_across_threads_holds_per_client_limits() {
    // A frozen clock makes the per-client quota exact under contention: the
    // gatekeeper is shared via `Arc` exactly as a server would share it.
    const CLIENTS: u64 = 32;
    const LIMIT: u32 = 100;
    const THREADS: usize = 8;
    const PASSES: u32 = 40;

    let clock = Arc::new(ManualClock::new());
    let limiter = Arc::new(RateLimiter::per_second(LIMIT).with_clock(Arc::clone(&clock)));
    let served: Arc<Vec<AtomicU32>> = Arc::new((0..CLIENTS).map(|_| AtomicU32::new(0)).collect());

    let mut handles = Vec::with_capacity(THREADS);
    for _ in 0..THREADS {
        let limiter = Arc::clone(&limiter);
        let served = Arc::clone(&served);
        handles.push(thread::spawn(move || {
            for _ in 0..PASSES {
                for client in 0..CLIENTS {
                    if admit(&*limiter, client) == Verdict::Serve {
                        let _ = served[client as usize].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker panicked");
    }

    for client in 0..CLIENTS {
        assert_eq!(
            served[client as usize].load(Ordering::Relaxed),
            LIMIT,
            "client {client} served the wrong count under concurrency"
        );
    }
}

#[cfg(feature = "algorithms")]
#[test]
fn test_gatekeeper_works_with_any_configured_algorithm() {
    use rate_net::Algorithm;

    // A gatekeeper picks the algorithm at startup; the admission code is
    // unchanged across all of them.
    for algorithm in [
        Algorithm::TokenBucket,
        Algorithm::LeakyBucket,
        Algorithm::FixedWindow,
        Algorithm::SlidingWindowLog,
        Algorithm::SlidingWindowCounter,
    ] {
        let limiter = RateLimiter::builder()
            .algorithm(algorithm)
            .per_second(3)
            .build();
        let served = (0..10)
            .filter(|_| admit(&limiter, "client") == Verdict::Serve)
            .count();
        assert_eq!(served, 3, "{algorithm:?} gatekeeper served the wrong count");
    }
}

#[cfg(feature = "async")]
#[tokio::test]
async fn test_async_gatekeeper_awaits_then_serves() {
    use rate_net::AsyncLimiter;

    // An async gatekeeper that queues rather than sheds: await until the caller
    // is within its limit, then serve.
    let limiter = AsyncLimiter::new(RateLimiter::per_second(200));
    for _ in 0..200 {
        let _ = limiter.check("client");
    }
    let completed =
        tokio::time::timeout(Duration::from_secs(2), limiter.until_ready("client")).await;
    assert!(
        completed.is_ok(),
        "async gatekeeper did not admit within 2s"
    );
}
