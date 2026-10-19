//! The byte-facing HTTP surface: key extraction from attacker-controlled
//! headers (`X-Forwarded-For`, an API-key header) and the structured
//! `RateLimit` / `RateLimit-Policy` field values built from arbitrary policy
//! names. Nothing may panic, an extracted address must come from the header,
//! and a formatted field value must always be a valid header value made of a
//! quoted Structured Field string and integer parameters.

#![no_main]

use arbitrary::Arbitrary;
use http::{HeaderName, HeaderValue, Request};
use libfuzzer_sys::fuzz_target;
use rate_net::tower::{ForwardedFor, HeaderKey, KeyExtractor, RequestInfo};
use rate_net::{Key, Quota, RateLimiter};

#[derive(Arbitrary, Debug)]
struct Input {
    forwarded: Vec<Vec<u8>>,
    api_key: Vec<u8>,
    hops: u8,
    policy: String,
    remaining: u32,
}

fuzz_target!(|input: Input| {
    let mut request = Request::new(());
    for value in &input.forwarded {
        if let Ok(value) = HeaderValue::from_bytes(value) {
            let _ = request.headers_mut().append("x-forwarded-for", value);
        }
    }
    if let Ok(value) = HeaderValue::from_bytes(&input.api_key) {
        let _ = request.headers_mut().insert("x-api-key", value);
    }
    let info = RequestInfo::of(&request);

    let hops = usize::from(input.hops % 8);
    if let Some(key) = ForwardedFor::trusting(hops).extract(&info) {
        // Every address the extractor can return is one written in the header.
        let joined: Vec<u8> = input.forwarded.join(&b","[..]);
        let entries: Vec<Key> = joined
            .split(|&byte| byte == b',')
            .filter_map(|entry| core::str::from_utf8(entry).ok())
            .filter_map(|entry| {
                let entry = entry.trim();
                entry
                    .parse::<std::net::IpAddr>()
                    .ok()
                    .or_else(|| entry.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()))
            })
            .map(|addr| Key::ip_prefix(addr, 32, 64))
            .collect();
        assert!(entries.contains(&key));
    }
    let _ = HeaderKey::new(HeaderName::from_static("x-api-key")).extract(&info);

    // Structured field values are always valid header values.
    let limiter = RateLimiter::per_minute(input.remaining);
    let snapshot = limiter.peek("k");
    for value in [
        snapshot.ratelimit(&input.policy).to_string(),
        Quota::per_minute(input.remaining)
            .ratelimit_policy(&input.policy)
            .to_string(),
    ] {
        assert!(value.starts_with('"'));
        assert!(value.bytes().all(|byte| (0x20..0x7f).contains(&byte)));
        assert!(HeaderValue::from_str(&value).is_ok());
    }
});
