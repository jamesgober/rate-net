//! [`tower`](https://docs.rs/tower) middleware: rate-limit inbound HTTP requests
//! in front of any `tower::Service` — an axum router, a hyper service, a tonic
//! server — and answer the ones over their limit with `429 Too Many Requests`.
//!
//! - [`RateLimitLayer`] wraps a service with a shared [`RateLimiter`].
//! - A [`KeyExtractor`] decides *who* each request is charged to:
//!   [`PeerIp`] (the connection's address), [`ForwardedFor`] (the client
//!   address a trusted proxy chain reports in `X-Forwarded-For`),
//!   [`HeaderKey`] (an API key or tenant header), or any closure over
//!   [`RequestInfo`].
//! - A denied request never reaches the inner service. It gets a `429` with
//!   `Retry-After` (whole seconds, rounded up) and the `RateLimit-Limit`,
//!   `RateLimit-Remaining`, and `RateLimit-Reset` headers, plus the structured
//!   `RateLimit` / `RateLimit-Policy` fields when a policy name is set; the
//!   response body is the body type's `Default` (empty for axum, `http-body`
//!   `Empty`/`Full`, and most others).
//!
//! The decision is made synchronously in `call`, before the inner service is
//! touched, so there is no waiting and no extra allocation: an admitted
//! request's future is the inner service's own, and a denied one is a ready
//! response.
//!
//! Behind the `tower` feature, which adds the `http`, `tower-layer`,
//! `tower-service`, and `pin-project-lite` crates — not `tower` itself.
//!
//! # Examples
//!
//! ```
//! use std::convert::Infallible;
//! use std::net::{IpAddr, Ipv4Addr};
//! use http::{Request, Response, StatusCode};
//! use rate_net::RateLimiter;
//! use rate_net::tower::{PeerIp, RateLimitLayer};
//! use tower::{Layer, Service, ServiceExt, service_fn};
//!
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let layer = RateLimitLayer::new(RateLimiter::per_minute(1), PeerIp::default());
//! let mut service = layer.layer(service_fn(|_: Request<String>| async {
//!     Ok::<_, Infallible>(Response::new(String::from("hello")))
//! }));
//!
//! // The server records each connection's address in the request extensions.
//! let request = || {
//!     let mut request = Request::new(String::new());
//!     request.extensions_mut().insert(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
//!     request
//! };
//!
//! let first = service.ready().await.unwrap().call(request()).await.unwrap();
//! assert_eq!(first.status(), StatusCode::OK);
//!
//! let second = service.ready().await.unwrap().call(request()).await.unwrap();
//! assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
//! assert_eq!(second.headers()["retry-after"], "60");
//! # }
//! ```

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use clock_lib::{Clock, SystemClock};
use http::header::{HeaderName, HeaderValue, RETRY_AFTER};
use http::{Extensions, HeaderMap, Method, Request, Response, StatusCode, Uri};
use tower_layer::Layer;
use tower_service::Service;

use crate::key::{Digester, Key, KeyRef};
use crate::limiter::RateLimiter;
use crate::quota::Quota;
use crate::snapshot::Snapshot;

/// The parts of a request a [`KeyExtractor`] may look at, borrowed — the
/// request's method, URI, headers, and extensions (where servers put the peer
/// address and anything earlier middleware attached).
#[derive(Debug, Clone, Copy)]
pub struct RequestInfo<'a> {
    method: &'a Method,
    uri: &'a Uri,
    headers: &'a HeaderMap,
    extensions: &'a Extensions,
}

impl<'a> RequestInfo<'a> {
    /// A view of `request`.
    ///
    /// # Examples
    ///
    /// ```
    /// use http::Request;
    /// use rate_net::tower::RequestInfo;
    ///
    /// let request = Request::get("/api/items").body(()).unwrap();
    /// assert_eq!(RequestInfo::of(&request).uri().path(), "/api/items");
    /// ```
    #[must_use]
    pub fn of<B>(request: &'a Request<B>) -> Self {
        Self {
            method: request.method(),
            uri: request.uri(),
            headers: request.headers(),
            extensions: request.extensions(),
        }
    }

    /// The request method.
    #[must_use]
    pub const fn method(&self) -> &'a Method {
        self.method
    }

    /// The request URI.
    #[must_use]
    pub const fn uri(&self) -> &'a Uri {
        self.uri
    }

    /// The request headers.
    #[must_use]
    pub const fn headers(&self) -> &'a HeaderMap {
        self.headers
    }

    /// The request extensions.
    #[must_use]
    pub const fn extensions(&self) -> &'a Extensions {
        self.extensions
    }
}

/// Decides which [`Key`] a request is charged to.
///
/// Returning `None` means the request carries no usable identity; every such
/// request is charged to one shared *anonymous* key (which no extracted key
/// can equal), so unidentifiable traffic is limited as a group rather than
/// let through.
///
/// Implemented for the built-in extractors and for any
/// `Fn(&RequestInfo<'_>) -> Option<Key>` closure.
///
/// # Examples
///
/// ```
/// use rate_net::Key;
/// use rate_net::tower::{KeyExtractor, RequestInfo};
///
/// // Charge each tenant, named by a path prefix.
/// let tenant = |request: &RequestInfo<'_>| {
///     let path = request.uri().path();
///     path.strip_prefix("/t/")
///         .and_then(|rest| rest.split('/').next())
///         .map(Key::from)
/// };
/// let request = http::Request::get("/t/acme/items").body(()).unwrap();
/// assert_eq!(tenant.extract(&RequestInfo::of(&request)), Some(Key::from("acme")));
/// ```
pub trait KeyExtractor: Clone + Send + Sync + 'static {
    /// The key for `request`, or `None` if it has no usable identity.
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key>;
}

impl<F> KeyExtractor for F
where
    F: Fn(&RequestInfo<'_>) -> Option<Key> + Clone + Send + Sync + 'static,
{
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        self(request)
    }
}

/// The client address prefixes a network key masks to: IPv4 addresses to
/// `v4` bits, IPv6 to `v6` (see [`Key::ip_prefix`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Prefixes {
    v4: u8,
    v6: u8,
}

impl Prefixes {
    /// Exact IPv4 addresses and IPv6 `/64`s — the usual per-client key, since
    /// an IPv6 host is typically assigned a whole `/64`.
    const DEFAULT: Self = Self { v4: 32, v6: 64 };

    fn key(self, addr: IpAddr) -> Key {
        Key::ip_prefix(addr, self.v4, self.v6)
    }
}

/// Keys a request by the connection's peer address.
///
/// The address is read from the request extensions, as an
/// [`IpAddr`] or a [`SocketAddr`] (the port is ignored) — insert one per
/// connection in the server's accept loop. axum keeps the peer address in its
/// own `ConnectInfo<SocketAddr>` extension; with axum, use a closure
/// extractor that reads it (see the [module docs](self)).
///
/// By default exact IPv4 addresses and IPv6 `/64` networks are keyed (see
/// [`Key::ip_prefix`]); [`with_prefixes`](Self::with_prefixes) changes that.
///
/// # Examples
///
/// ```
/// use std::net::SocketAddr;
/// use rate_net::Key;
/// use rate_net::tower::{KeyExtractor, PeerIp, RequestInfo};
///
/// let mut request = http::Request::new(());
/// let peer: SocketAddr = "[2001:db8::1]:50000".parse().unwrap();
/// request.extensions_mut().insert(peer);
///
/// let key = PeerIp::default().extract(&RequestInfo::of(&request));
/// assert_eq!(key, Some(Key::ip_prefix(peer.ip(), 32, 64)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIp {
    prefixes: Prefixes,
}

impl PeerIp {
    /// Keys IPv4 peers by their first `v4` bits and IPv6 peers by their first
    /// `v6` bits.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::tower::PeerIp;
    ///
    /// // Group IPv4 clients by /24 and IPv6 clients by /48.
    /// let extractor = PeerIp::default().with_prefixes(24, 48);
    /// # let _ = extractor;
    /// ```
    #[must_use]
    pub const fn with_prefixes(mut self, v4: u8, v6: u8) -> Self {
        self.prefixes = Prefixes { v4, v6 };
        self
    }
}

impl Default for PeerIp {
    fn default() -> Self {
        Self {
            prefixes: Prefixes::DEFAULT,
        }
    }
}

/// The peer address recorded in `extensions`, if any.
fn peer_addr(extensions: &Extensions) -> Option<IpAddr> {
    extensions
        .get::<IpAddr>()
        .copied()
        .or_else(|| extensions.get::<SocketAddr>().map(SocketAddr::ip))
}

impl KeyExtractor for PeerIp {
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        peer_addr(request.extensions).map(|addr| self.prefixes.key(addr))
    }
}

/// Keys a request by the client address in `X-Forwarded-For`, trusting a
/// fixed number of proxies.
///
/// Each proxy appends the address it received the request from, so the
/// header's rightmost entries are written by your own proxies and everything
/// to their left is whatever the client sent — trivially forged. With
/// `trusted_proxies` proxies in front of the service, the client is the entry
/// that many places from the right: that is the address taken. A header with
/// fewer entries or an unparseable one yields no key (see
/// [`Anonymous`]).
///
/// **The header is only as trustworthy as the path to the service.** A client
/// that can reach the service *directly*, bypassing the proxies, writes the
/// whole header itself and can choose its key. So either make the service
/// unreachable except through the proxies, or name them with
/// [`only_from`](Self::only_from): the header is then used only when the
/// connection's peer address (from the request extensions, as for
/// [`PeerIp`]) is one of those networks, and any other request is keyed by
/// its peer address — so a direct client is limited as itself, whatever it
/// writes in the header.
///
/// With `trusted_proxies == 0` the header is ignored and the peer address is
/// used, as [`PeerIp`].
///
/// Addresses are compared as [`Key::ip`] compares them: an IPv6 address with
/// an embedded IPv4 one — mapped (`::ffff:a.b.c.d`) or compatible
/// (`::a.b.c.d`) — is that IPv4 address, for peers and for `only_from`
/// networks alike (an embedded network keeps its last `prefix_len - 96`
/// bits, so `::ffff:10.0.0.0/104` is `10.0.0.0/8`). A prefix length of `0`
/// matches every peer, of either family.
///
/// # Examples
///
/// ```
/// use std::net::IpAddr;
/// use rate_net::Key;
/// use rate_net::tower::{ForwardedFor, KeyExtractor, RequestInfo};
///
/// // One load balancer in front: it appends the client's address.
/// let request = http::Request::get("/")
///     .header("x-forwarded-for", "198.51.100.66, 203.0.113.7")
///     .body(())
///     .unwrap();
/// let key = ForwardedFor::trusting(1).extract(&RequestInfo::of(&request));
/// let client: IpAddr = "203.0.113.7".parse().unwrap();
/// assert_eq!(key, Some(Key::ip_prefix(client, 32, 64)));
///
/// // Verified: only a peer inside 10.0.0.0/8 may speak for the client.
/// let verified = ForwardedFor::trusting(1).only_from("10.0.0.0".parse().unwrap(), 8);
/// let mut direct = http::Request::get("/")
///     .header("x-forwarded-for", "198.51.100.1")
///     .body(())
///     .unwrap();
/// let attacker: IpAddr = "192.0.2.66".parse().unwrap();
/// direct.extensions_mut().insert(attacker);
/// // A direct client is keyed by its own address, not the one it wrote.
/// assert_eq!(
///     verified.extract(&RequestInfo::of(&direct)),
///     Some(Key::ip_prefix(attacker, 32, 64))
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardedFor {
    trusted_proxies: usize,
    prefixes: Prefixes,
    /// Networks the connection's peer must belong to for the header to be
    /// believed; empty means "every peer is a proxy".
    proxies: Vec<(IpAddr, u8)>,
}

impl ForwardedFor {
    /// Trusts the rightmost `trusted_proxies` hops of `X-Forwarded-For`.
    #[must_use]
    pub const fn trusting(trusted_proxies: usize) -> Self {
        Self {
            trusted_proxies,
            prefixes: Prefixes::DEFAULT,
            proxies: Vec::new(),
        }
    }

    /// Believes the header only from a connection whose peer address is in
    /// the network `addr / prefix_len` (repeat for several networks); any
    /// other request is keyed by its peer address. See the type docs.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::tower::ForwardedFor;
    ///
    /// let extractor = ForwardedFor::trusting(1)
    ///     .only_from("10.0.0.0".parse().unwrap(), 8)
    ///     .only_from("fd00::".parse().unwrap(), 8);
    /// # let _ = extractor;
    /// ```
    #[must_use]
    pub fn only_from(mut self, addr: IpAddr, prefix_len: u8) -> Self {
        self.proxies.push(network(addr, prefix_len));
        self
    }

    /// Keys IPv4 clients by their first `v4` bits and IPv6 clients by their
    /// first `v6` bits (default: 32 and 64).
    #[must_use]
    pub const fn with_prefixes(mut self, v4: u8, v6: u8) -> Self {
        self.prefixes = Prefixes { v4, v6 };
        self
    }
}

/// Whether `addr` is in the network `net / len` (IPv4-mapped addresses
/// compare as IPv4).
fn in_network(addr: IpAddr, net: IpAddr, len: u8) -> bool {
    if len == 0 {
        return true;
    }
    match (canonical(addr), net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            let mask = u32::MAX
                .checked_shl(32 - u32::from(len.min(32)))
                .unwrap_or(0);
            u32::from(a) & mask == u32::from(n) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            let mask = u128::MAX
                .checked_shl(128 - u32::from(len.min(128)))
                .unwrap_or(0);
            u128::from(a) & mask == u128::from(n) & mask
        }
        _ => false,
    }
}

/// `addr` with an embedded IPv4 address (mapped or compatible) unwrapped, as
/// [`Key::ip`] does.
fn canonical(addr: IpAddr) -> IpAddr {
    crate::key::canonical_ip(addr)
}

/// The network `addr / prefix_len` in canonical form: an IPv6 network whose
/// prefix covers an embedded IPv4 address (`prefix_len >= 96`) becomes that
/// IPv4 network with the remaining bits; lengths past the family's width are
/// clamped.
fn network(addr: IpAddr, prefix_len: u8) -> (IpAddr, u8) {
    match (addr, canonical(addr)) {
        (IpAddr::V6(_), IpAddr::V4(v4)) if prefix_len >= 96 => {
            (IpAddr::V4(v4), (prefix_len - 96).min(32))
        }
        (IpAddr::V4(v4), _) => (IpAddr::V4(v4), prefix_len.min(32)),
        (v6, _) => (v6, prefix_len.min(128)),
    }
}

impl KeyExtractor for ForwardedFor {
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        if self.trusted_proxies == 0 {
            return peer_addr(request.extensions).map(|addr| self.prefixes.key(addr));
        }
        if !self.proxies.is_empty() {
            let peer = peer_addr(request.extensions)?;
            let from_proxy = self
                .proxies
                .iter()
                .any(|&(net, len)| in_network(peer, net, len));
            if !from_proxy {
                return Some(self.prefixes.key(peer));
            }
        }
        // Every entry across every `X-Forwarded-For` line, in order.
        let mut hops = request
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
            .rev();
        let entry = hops.nth(self.trusted_proxies - 1)?;
        let text = core::str::from_utf8(entry).ok()?.trim();
        parse_addr(text).map(|addr| self.prefixes.key(addr))
    }
}

/// An address from an `X-Forwarded-For` entry: a bare IPv4 or IPv6 address,
/// one with a port (`a.b.c.d:port`, `[v6]:port`), a bracketed IPv6 address
/// (`[v6]`), or an IPv6 address with a zone (`fe80::1%eth0`, the zone
/// dropped).
fn parse_addr(text: &str) -> Option<IpAddr> {
    if let Ok(addr) = text.parse::<IpAddr>() {
        return Some(addr);
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr.ip());
    }
    let unbracketed = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(text);
    let without_zone = unbracketed.split('%').next().unwrap_or(unbracketed);
    without_zone.parse::<IpAddr>().ok()
}

/// Keys a request by the value of one header — an API key, a tenant id, a
/// session token. A request without the header has no key.
///
/// Header values are chosen by the client and can be kilobytes long, so a
/// value longer than 64 bytes (configurable with
/// [`with_digest_over`](Self::with_digest_over)) is keyed by a 16-byte keyed
/// digest of it instead of its bytes: the bytes a key holds are bounded
/// whatever clients send. Shorter values are the key as they are. The digest
/// is two independently, randomly seeded 64-bit hashes (fresh per
/// extractor, and shared by its clones): a client cannot aim a value at
/// another's key, but it is not a cryptographic hash. Two separately created
/// `HeaderKey`s digest the same long value differently, so layers that share
/// one limiter should share one extractor (clone it) to key a long value
/// alike.
///
/// # Examples
///
/// ```
/// use rate_net::Key;
/// use rate_net::tower::{HeaderKey, KeyExtractor, RequestInfo};
///
/// let request = http::Request::get("/")
///     .header("x-api-key", "k-123")
///     .body(())
///     .unwrap();
/// let key = HeaderKey::new(http::HeaderName::from_static("x-api-key"))
///     .extract(&RequestInfo::of(&request));
/// assert_eq!(key, Some(Key::from("k-123")));
/// ```
#[derive(Debug, Clone)]
pub struct HeaderKey {
    name: HeaderName,
    digest_over: usize,
    digester: Digester,
}

impl HeaderKey {
    /// Keys requests by the first value of the header `name`, digesting
    /// values longer than 64 bytes.
    #[must_use]
    pub fn new(name: HeaderName) -> Self {
        Self {
            name,
            digest_over: 64,
            digester: Digester::new(),
        }
    }

    /// Digests values longer than `len` bytes instead of 64.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::tower::{HeaderKey, KeyExtractor, RequestInfo};
    ///
    /// let request = http::Request::get("/")
    ///     .header("x-api-key", "a".repeat(40))
    ///     .body(())
    ///     .unwrap();
    /// let short = HeaderKey::new(http::HeaderName::from_static("x-api-key")).with_digest_over(16);
    /// let key = short.extract(&RequestInfo::of(&request)).unwrap();
    /// assert_eq!(key.len(), 16); // digested
    /// ```
    #[must_use]
    pub const fn with_digest_over(mut self, len: usize) -> Self {
        self.digest_over = len;
        self
    }
}

impl KeyExtractor for HeaderKey {
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        let value = request.headers.get(&self.name)?.as_bytes();
        Some(if value.len() > self.digest_over {
            self.digester.digest_key(KeyRef::raw(value))
        } else {
            Key::from(value)
        })
    }
}

/// What the layer does with a request whose extractor finds no identity —
/// no peer address, no header, an unparseable `X-Forwarded-For`.
///
/// `#[non_exhaustive]`: more policies may be added.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anonymous {
    /// Charge every such request to one shared anonymous key, which no
    /// extracted key can equal (the default). Unidentifiable traffic is then
    /// limited as a group — and, because the group shares one allowance,
    /// clients that cannot be identified can exhaust it for each other.
    Shared,
    /// Charge such requests to this key instead, so it can be given its own
    /// quota with [`Builder::quota_for`](crate::Builder::quota_for).
    Key(Key),
    /// Answer such requests with this status without charging anything or
    /// calling the inner service.
    Reject(StatusCode),
}

/// A [`Layer`] that rate-limits every request through a shared
/// [`RateLimiter`], charging it to the key a [`KeyExtractor`] picks. See the
/// [module docs](self).
pub struct RateLimitLayer<K, C: Clock + Clone = SystemClock> {
    limiter: Arc<RateLimiter<C>>,
    config: Arc<Config<K>>,
}

/// The per-layer settings, shared by every service it builds.
#[derive(Debug)]
struct Config<K> {
    extractor: K,
    cost: u32,
    policy: Option<String>,
    headers_on_success: bool,
    impossible_status: StatusCode,
    anonymous: Anonymous,
}

impl<K: KeyExtractor> RateLimitLayer<K, SystemClock> {
    /// Rate-limits requests through `limiter`, keyed by `extractor`.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_second(50), PeerIp::default());
    /// assert_eq!(layer.limiter().quota().limit(), 50);
    /// ```
    #[must_use]
    pub fn new(limiter: RateLimiter, extractor: K) -> Self {
        Self::shared(Arc::new(limiter), extractor)
    }
}

impl<K: KeyExtractor, C: Clock + Clone> RateLimitLayer<K, C> {
    /// Rate-limits requests through a limiter shared with other code — several
    /// routers, or a handler that also checks it directly.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let limiter = Arc::new(RateLimiter::per_second(50));
    /// let layer = RateLimitLayer::shared(Arc::clone(&limiter), PeerIp::default());
    /// assert!(Arc::ptr_eq(layer.limiter(), &limiter));
    /// ```
    #[must_use]
    pub fn shared(limiter: Arc<RateLimiter<C>>, extractor: K) -> Self {
        Self {
            limiter,
            config: Arc::new(Config {
                extractor,
                cost: 1,
                policy: None,
                headers_on_success: false,
                impossible_status: StatusCode::TOO_MANY_REQUESTS,
                anonymous: Anonymous::Shared,
            }),
        }
    }

    /// Charges `cost` units per request instead of one. A cost of `0` is
    /// treated as `1`: a zero-cost check always succeeds, so the layer would
    /// not limit anything.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_second(100), PeerIp::default()).with_cost(10);
    /// # let _ = layer;
    /// ```
    #[must_use]
    pub fn with_cost(self, cost: u32) -> Self {
        self.map_config(|config| config.cost = cost.max(1))
    }

    /// What to do with requests the extractor cannot identify; see
    /// [`Anonymous`]. Defaults to [`Anonymous::Shared`].
    ///
    /// # Examples
    ///
    /// ```
    /// use http::StatusCode;
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{Anonymous, PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_second(10), PeerIp::default())
    ///     .with_anonymous(Anonymous::Reject(StatusCode::BAD_REQUEST));
    /// # let _ = layer;
    /// ```
    #[must_use]
    pub fn with_anonymous(self, anonymous: Anonymous) -> Self {
        self.map_config(|config| config.anonymous = anonymous)
    }

    /// Names the rate-limit policy, so responses also carry the structured
    /// `RateLimit` and `RateLimit-Policy` fields of the current IETF draft
    /// (see [`headers`](crate::headers)).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_minute(600), PeerIp::default())
    ///     .with_policy("default");
    /// # let _ = layer;
    /// ```
    #[must_use]
    pub fn with_policy(self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.map_config(|config| config.policy = Some(name))
    }

    /// Adds the rate-limit headers to admitted responses too, not only to
    /// `429`s. It costs one extra read of the key's state per request.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_second(10), PeerIp::default())
    ///     .with_headers_on_success(true);
    /// # let _ = layer;
    /// ```
    #[must_use]
    pub fn with_headers_on_success(self, enabled: bool) -> Self {
        self.map_config(|config| config.headers_on_success = enabled)
    }

    /// The status for a request that can never be admitted
    /// ([`Decision::is_impossible`](crate::Decision::is_impossible): a cost above the key's capacity, a key the
    /// limiter refuses, or a quota that admits nothing). Such a response has no
    /// `Retry-After`. Defaults to `429`; `400`, `403`, or `413` may suit
    /// better, depending on the cause.
    ///
    /// # Examples
    ///
    /// ```
    /// use http::StatusCode;
    /// use rate_net::RateLimiter;
    /// use rate_net::tower::{PeerIp, RateLimitLayer};
    ///
    /// let layer = RateLimitLayer::new(RateLimiter::per_second(10), PeerIp::default())
    ///     .with_impossible_status(StatusCode::FORBIDDEN);
    /// # let _ = layer;
    /// ```
    #[must_use]
    pub fn with_impossible_status(self, status: StatusCode) -> Self {
        self.map_config(|config| config.impossible_status = status)
    }

    /// The shared limiter.
    #[must_use]
    pub const fn limiter(&self) -> &Arc<RateLimiter<C>> {
        &self.limiter
    }

    fn map_config(self, change: impl FnOnce(&mut Config<K>)) -> Self {
        let mut config = Config::clone_from(&self.config);
        change(&mut config);
        Self {
            limiter: self.limiter,
            config: Arc::new(config),
        }
    }
}

impl<K: Clone> Config<K> {
    fn clone_from(config: &Self) -> Self {
        Self {
            extractor: config.extractor.clone(),
            cost: config.cost,
            policy: config.policy.clone(),
            headers_on_success: config.headers_on_success,
            impossible_status: config.impossible_status,
            anonymous: config.anonymous.clone(),
        }
    }
}

impl<K, C: Clock + Clone> Clone for RateLimitLayer<K, C> {
    fn clone(&self) -> Self {
        Self {
            limiter: Arc::clone(&self.limiter),
            config: Arc::clone(&self.config),
        }
    }
}

impl<K: fmt::Debug, C: Clock + Clone> fmt::Debug for RateLimitLayer<K, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimitLayer")
            .field("limiter", &self.limiter)
            .field("extractor", &self.config.extractor)
            .field("cost", &self.config.cost)
            .field("policy", &self.config.policy)
            .finish_non_exhaustive()
    }
}

impl<S, K, C: Clock + Clone> Layer<S> for RateLimitLayer<K, C> {
    type Service = RateLimited<S, K, C>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimited {
            inner,
            limiter: Arc::clone(&self.limiter),
            config: Arc::clone(&self.config),
        }
    }
}

/// The service [`RateLimitLayer`] produces.
pub struct RateLimited<S, K, C: Clock + Clone = SystemClock> {
    inner: S,
    limiter: Arc<RateLimiter<C>>,
    config: Arc<Config<K>>,
}

impl<S: Clone, K, C: Clock + Clone> Clone for RateLimited<S, K, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            limiter: Arc::clone(&self.limiter),
            config: Arc::clone(&self.config),
        }
    }
}

impl<S, K, C: Clock + Clone> fmt::Debug for RateLimited<S, K, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimited")
            .field("limiter", &self.limiter)
            .finish_non_exhaustive()
    }
}

impl<S, K, C, ReqBody, ResBody> Service<Request<ReqBody>> for RateLimited<S, K, C>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
    K: KeyExtractor,
    C: Clock + Clone,
    ResBody: Default,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, ResBody>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        let config = &*self.config;
        let key = match config.extractor.extract(&RequestInfo::of(&request)) {
            Some(key) => key,
            None => match &config.anonymous {
                Anonymous::Shared => Key::anonymous(),
                Anonymous::Key(key) => key.clone(),
                Anonymous::Reject(status) => {
                    let mut response = Response::new(ResBody::default());
                    *response.status_mut() = *status;
                    return ResponseFuture::limited(response);
                }
            },
        };
        let decision = self.limiter.check_ref(key.as_key_ref(), config.cost);
        if decision.is_allow() {
            let headers = config.headers_on_success.then(|| {
                let mut headers = HeaderMap::new();
                let snapshot = self.limiter.peek_ref(key.as_key_ref());
                let quota = self.limiter.quota_of_ref(key.as_key_ref());
                write_headers(&mut headers, snapshot, quota, config);
                headers
            });
            return ResponseFuture::inner(self.inner.call(request), headers);
        }
        let mut response = Response::new(ResBody::default());
        *response.status_mut() = if decision.is_impossible() {
            config.impossible_status
        } else {
            StatusCode::TOO_MANY_REQUESTS
        };
        if let Some(secs) = decision.retry_after_secs() {
            let _previous = response.headers_mut().insert(RETRY_AFTER, secs.into());
        }
        let snapshot = self.limiter.peek_ref(key.as_key_ref());
        let quota = self.limiter.quota_of_ref(key.as_key_ref());
        write_headers(response.headers_mut(), snapshot, quota, config);
        ResponseFuture::limited(response)
    }
}

/// Writes the rate-limit headers for `snapshot` — a key under `quota` (its
/// own, per-key override included) — into `headers`.
fn write_headers<K>(headers: &mut HeaderMap, snapshot: Snapshot, quota: Quota, config: &Config<K>) {
    let [(_, limit), (_, remaining), (_, reset)] = snapshot.headers();
    for (name, value) in [
        ("ratelimit-limit", limit),
        ("ratelimit-remaining", remaining),
        ("ratelimit-reset", reset),
    ] {
        let _previous = headers.insert(HeaderName::from_static(name), value.into());
    }
    if let Some(policy) = &config.policy {
        if let Ok(value) = HeaderValue::from_str(&snapshot.ratelimit(policy).to_string()) {
            let _previous = headers.insert(HeaderName::from_static("ratelimit"), value);
        }
        let policy = quota.ratelimit_policy(policy).to_string();
        if let Ok(value) = HeaderValue::from_str(&policy) {
            let _previous = headers.insert(HeaderName::from_static("ratelimit-policy"), value);
        }
    }
}

pin_project_lite::pin_project! {
    /// The future [`RateLimited`] returns: the inner service's own future for
    /// an admitted request, or a ready `429` for a denied one.
    pub struct ResponseFuture<F, B> {
        #[pin]
        state: State<F, B>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProj]
    enum State<F, B> {
        // An admitted request: the inner service's future, plus the headers to
        // add to its response.
        Inner {
            #[pin]
            future: F,
            headers: Option<HeaderMap>,
        },
        // A denied request's response, taken when first polled.
        Limited {
            response: Option<Response<B>>,
        },
    }
}

impl<F, B> ResponseFuture<F, B> {
    const fn inner(future: F, headers: Option<HeaderMap>) -> Self {
        Self {
            state: State::Inner { future, headers },
        }
    }

    const fn limited(response: Response<B>) -> Self {
        Self {
            state: State::Limited {
                response: Some(response),
            },
        }
    }
}

impl<F, B, E> Future for ResponseFuture<F, B>
where
    F: Future<Output = Result<Response<B>, E>>,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().state.project() {
            StateProj::Inner { future, headers } => {
                let mut response = match future.poll(cx) {
                    Poll::Ready(Ok(response)) => response,
                    other => return other,
                };
                if let Some(extra) = headers.take() {
                    response.headers_mut().extend(extra);
                }
                Poll::Ready(Ok(response))
            }
            StateProj::Limited { response } => match response.take() {
                Some(response) => Poll::Ready(Ok(response)),
                // Polled again after completing: a contract violation by the
                // caller; stay pending rather than panic.
                None => Poll::Pending,
            },
        }
    }
}

impl<F, B> fmt::Debug for ResponseFuture<F, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.state {
            State::Inner { .. } => "admitted",
            State::Limited { .. } => "limited",
        };
        f.debug_struct("ResponseFuture")
            .field("state", &state)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use core::future::{Ready, ready};
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use http::{HeaderName, Request, Response, StatusCode};
    use tower::util::ServiceFn;
    use tower::{Layer, Service, ServiceExt, service_fn};

    use std::sync::Arc;

    use super::{ForwardedFor, HeaderKey, KeyExtractor, PeerIp, RateLimitLayer, RequestInfo};
    use crate::{Key, Quota, RateLimiter};

    fn from_peer(ip: [u8; 4]) -> Request<String> {
        let mut request = Request::new(String::new());
        let _previous = request
            .extensions_mut()
            .insert(IpAddr::V4(Ipv4Addr::from(ip)));
        request
    }

    async fn status<S>(service: &mut S, request: Request<String>) -> Response<String>
    where
        S: tower::Service<Request<String>, Response = Response<String>, Error = Infallible>,
    {
        service.ready().await.unwrap().call(request).await.unwrap()
    }

    type Respond = fn(Request<String>) -> Ready<Result<Response<String>, Infallible>>;

    fn echo() -> ServiceFn<Respond> {
        fn respond(_: Request<String>) -> Ready<Result<Response<String>, Infallible>> {
            ready(Ok(Response::new(String::from("ok"))))
        }
        service_fn(respond as Respond)
    }

    #[tokio::test]
    async fn test_layer_limits_each_peer_separately() {
        let layer = RateLimitLayer::new(RateLimiter::per_minute(2), PeerIp::default());
        let mut service = layer.layer(echo());
        for _ in 0..2 {
            assert_eq!(
                status(&mut service, from_peer([10, 0, 0, 1]))
                    .await
                    .status(),
                StatusCode::OK
            );
        }
        let denied = status(&mut service, from_peer([10, 0, 0, 1])).await;
        assert_eq!(denied.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(denied.headers()["retry-after"], "30");
        assert_eq!(denied.headers()["ratelimit-limit"], "2");
        assert_eq!(denied.headers()["ratelimit-remaining"], "0");
        assert_eq!(denied.body(), "");
        // Another client is unaffected.
        assert_eq!(
            status(&mut service, from_peer([10, 0, 0, 2]))
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_policy_headers_and_headers_on_success() {
        // A frozen clock, so the reported numbers are exact on a loaded host.
        let frozen = std::sync::Arc::new(clock_lib::ManualClock::new());
        let limiter = std::sync::Arc::new(RateLimiter::per_minute(60).with_clock(frozen));
        let layer = RateLimitLayer::shared(limiter, PeerIp::default())
            .with_policy("default")
            .with_headers_on_success(true);
        let mut service = layer.clone().layer(echo());
        let ok = status(&mut service, from_peer([10, 0, 0, 1])).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.body(), "ok");
        assert_eq!(ok.headers()["ratelimit-remaining"], "59");
        assert_eq!(ok.headers()["ratelimit"], r#""default";r=59;t=1"#);
        assert_eq!(ok.headers()["ratelimit-policy"], r#""default";q=60;w=60"#);
        assert!(format!("{layer:?}").contains("RateLimitLayer"));
        assert!(format!("{service:?}").contains("RateLimited"));
    }

    #[tokio::test]
    async fn test_impossible_requests_use_the_configured_status() {
        let layer = RateLimitLayer::new(RateLimiter::per_second(5), PeerIp::default())
            .with_cost(6)
            .with_impossible_status(StatusCode::PAYLOAD_TOO_LARGE);
        let mut service = layer.layer(echo());
        let future = service
            .ready()
            .await
            .unwrap()
            .call(from_peer([10, 0, 0, 1]));
        assert!(format!("{future:?}").contains("limited"));
        let response = future.await.unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response.headers().get("retry-after").is_none());
    }

    #[tokio::test]
    async fn test_unidentified_requests_share_one_anonymous_key() {
        let layer = RateLimitLayer::new(RateLimiter::per_minute(1), PeerIp::default());
        let mut service = layer.layer(echo());
        let anonymous = || Request::new(String::new());
        assert_eq!(
            status(&mut service, anonymous()).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            status(&mut service, anonymous()).await.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        // An identified client is not charged to the anonymous key.
        assert_eq!(
            status(&mut service, from_peer([10, 0, 0, 9]))
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn test_inner_errors_pass_through_and_services_clone() {
        let layer = RateLimitLayer::new(RateLimiter::per_minute(5), PeerIp::default());
        assert_eq!(layer.limiter().quota().limit(), 5);
        let failing = service_fn(|_: Request<String>| async {
            Err::<Response<String>, std::io::Error>(std::io::Error::other("down"))
        });
        let mut service = layer.layer(failing);
        let mut clone = service.clone();
        let error = clone
            .ready()
            .await
            .unwrap()
            .call(from_peer([10, 0, 0, 1]))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "down");
        let future = service
            .ready()
            .await
            .unwrap()
            .call(from_peer([10, 0, 0, 1]));
        assert!(format!("{future:?}").contains("admitted"));
        assert!(future.await.is_err());
    }

    #[test]
    fn test_peer_ip_reads_ip_or_socket_addr_extensions() {
        let mut request = Request::new(());
        assert_eq!(PeerIp::default().extract(&RequestInfo::of(&request)), None);
        let peer: SocketAddr = "192.0.2.5:4000".parse().unwrap();
        let _previous = request.extensions_mut().insert(peer);
        assert_eq!(
            PeerIp::default()
                .with_prefixes(24, 48)
                .extract(&RequestInfo::of(&request)),
            Some(Key::ip_prefix(peer.ip(), 24, 48))
        );
        let info = RequestInfo::of(&request);
        assert_eq!(info.method(), http::Method::GET);
        assert!(info.headers().is_empty());
        assert!(info.extensions().get::<SocketAddr>().is_some());
    }

    #[test]
    fn test_forwarded_for_takes_the_hop_the_trusted_proxies_reported() {
        let request = Request::get("/")
            .header("x-forwarded-for", "1.1.1.1, 2.2.2.2")
            .header("x-forwarded-for", "[2001:db8::7]:443, 3.3.3.3:80")
            .body(())
            .unwrap();
        let info = RequestInfo::of(&request);
        let key = |hops: usize| ForwardedFor::trusting(hops).extract(&info);
        let ip = |text: &str| Some(Key::ip_prefix(text.parse().unwrap(), 32, 64));
        assert_eq!(key(1), ip("3.3.3.3"));
        assert_eq!(key(2), ip("2001:db8::7"));
        assert_eq!(key(4), ip("1.1.1.1"));
        // Fewer entries than trusted proxies: no key, never a forged one.
        assert_eq!(key(5), None);
        // Zero trusted proxies: the header is ignored for the peer address.
        assert_eq!(key(0), None);
        let garbage = Request::get("/")
            .header("x-forwarded-for", "not-an-ip")
            .body(())
            .unwrap();
        assert_eq!(
            ForwardedFor::trusting(1)
                .with_prefixes(24, 48)
                .extract(&RequestInfo::of(&garbage)),
            None
        );
    }

    #[test]
    fn test_header_key_and_closure_extractors() {
        let request = Request::get("/t/acme/x")
            .header("x-api-key", "k1")
            .body(())
            .unwrap();
        let info = RequestInfo::of(&request);
        let header = HeaderKey::new(HeaderName::from_static("x-api-key"));
        assert_eq!(header.extract(&info), Some(Key::from("k1")));
        assert_eq!(
            HeaderKey::new(HeaderName::from_static("x-missing")).extract(&info),
            None
        );
        let by_path = |info: &RequestInfo<'_>| Some(Key::from(info.uri().path()));
        assert_eq!(by_path.extract(&info), Some(Key::from("/t/acme/x")));
    }

    #[test]
    fn test_forwarded_for_verified_peers_stop_direct_forgery() {
        // RN2-005: with `only_from`, a client reaching the service directly is
        // keyed by its own address whatever it writes in the header.
        let verified = ForwardedFor::trusting(1).only_from("10.0.0.0".parse().unwrap(), 8);
        let mut keys = std::collections::HashSet::new();
        for i in 0..1_000u32 {
            let mut request = Request::get("/")
                .header("x-forwarded-for", format!("172.16.{}.{}", i >> 8, i & 255))
                .body(())
                .unwrap();
            let _previous = request
                .extensions_mut()
                .insert(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 66)));
            let _new = keys.insert(verified.extract(&RequestInfo::of(&request)));
        }
        assert_eq!(keys.len(), 1, "a direct client chose its key");
        // Through a proxy in the network, the proxy's report is used.
        let mut proxied = Request::get("/")
            .header("x-forwarded-for", "203.0.113.7")
            .body(())
            .unwrap();
        let _previous = proxied
            .extensions_mut()
            .insert(SocketAddr::from(([10, 1, 2, 3], 443)));
        assert_eq!(
            verified.extract(&RequestInfo::of(&proxied)),
            Some(Key::ip_prefix("203.0.113.7".parse().unwrap(), 32, 64))
        );
        // A mapped IPv6 peer matches its IPv4 network; no peer, no key.
        let mut mapped = Request::get("/")
            .header("x-forwarded-for", "203.0.113.7")
            .body(())
            .unwrap();
        let _previous = mapped
            .extensions_mut()
            .insert(IpAddr::V6(Ipv4Addr::new(10, 9, 9, 9).to_ipv6_mapped()));
        assert!(verified.extract(&RequestInfo::of(&mapped)).is_some());
        let bare = Request::get("/")
            .header("x-forwarded-for", "1.2.3.4")
            .body(())
            .unwrap();
        assert_eq!(verified.extract(&RequestInfo::of(&bare)), None);
        let v6 = ForwardedFor::trusting(1).only_from("fd00::".parse().unwrap(), 8);
        let mut from_v6 = Request::get("/")
            .header("x-forwarded-for", "1.2.3.4")
            .body(())
            .unwrap();
        let _previous = from_v6
            .extensions_mut()
            .insert("fd12::1".parse::<IpAddr>().unwrap());
        assert!(v6.extract(&RequestInfo::of(&from_v6)).is_some());
        assert!(!super::in_network(
            "1.2.3.4".parse().unwrap(),
            "fd00::".parse().unwrap(),
            8
        ));
    }

    #[test]
    fn test_only_from_compares_embedded_ipv4_as_ipv4() {
        // RN3-006: IPv4-compatible peers, IPv4-mapped networks with an IPv6
        // prefix length, and /0 networks.
        let ip = |text: &str| text.parse::<IpAddr>().unwrap();
        let net = |text: &str, len: u8| super::network(ip(text), len);
        let inside = |peer: &str, (n, len): (IpAddr, u8)| super::in_network(ip(peer), n, len);
        assert_eq!(net("::ffff:10.0.0.0", 104), (ip("10.0.0.0"), 8));
        assert_eq!(net("::10.0.0.0", 104), (ip("10.0.0.0"), 8));
        assert_eq!(net("10.0.0.0", 40), (ip("10.0.0.0"), 32));
        assert!(inside("10.200.0.1", net("::ffff:10.0.0.0", 104)));
        assert!(inside("::10.9.9.9", net("10.0.0.0", 8)));
        assert!(inside("::ffff:10.9.9.9", net("10.0.0.0", 8)));
        assert!(!inside("11.0.0.1", net("::ffff:10.0.0.0", 104)));
        assert!(inside("192.0.2.1", net("::", 0)));
        assert!(inside("2001:db8::1", net("0.0.0.0", 0)));
        assert!(!inside("192.0.2.1", net("fd00::", 8)));
        // Through the extractor: a compatible-form proxy peer is trusted.
        let verified = ForwardedFor::trusting(1).only_from(ip("::ffff:10.0.0.0"), 104);
        let mut request = Request::get("/")
            .header("x-forwarded-for", "203.0.113.7")
            .body(())
            .unwrap();
        let _previous = request.extensions_mut().insert(ip("::10.1.2.3"));
        assert_eq!(
            verified.extract(&RequestInfo::of(&request)),
            Some(Key::ip_prefix(ip("203.0.113.7"), 32, 64))
        );
    }

    #[test]
    fn test_forwarded_for_parses_bracketed_and_zoned_ipv6() {
        for (entry, expect) in [
            ("[2001:db8::1]", Some("2001:db8::1")),
            ("fe80::1%eth0", Some("fe80::1")),
            ("[fe80::1%25eth0]", Some("fe80::1")),
            ("unknown", None),
        ] {
            assert_eq!(
                super::parse_addr(entry),
                expect.map(|text| text.parse::<IpAddr>().unwrap()),
                "{entry}"
            );
        }
    }

    #[test]
    fn test_header_key_digests_long_values() {
        // RN2-006: kilobyte header values do not become kilobyte keys.
        let header = HeaderKey::new(HeaderName::from_static("x-api-key"));
        let long = "A".repeat(8 * 1024);
        let request = Request::get("/")
            .header("x-api-key", &long)
            .body(())
            .unwrap();
        let key = header.extract(&RequestInfo::of(&request)).unwrap();
        assert_eq!(key.len(), 16);
        assert_eq!(
            header.extract(&RequestInfo::of(&request)),
            Some(key.clone())
        );
        assert_ne!(key, Key::from(long.as_str()));
        // Different long values, different keys; 64 bytes is still verbatim.
        let other = Request::get("/")
            .header("x-api-key", "B".repeat(8 * 1024))
            .body(())
            .unwrap();
        assert_ne!(header.extract(&RequestInfo::of(&other)), Some(key));
        let at_limit = "c".repeat(64);
        let exact = Request::get("/")
            .header("x-api-key", &at_limit)
            .body(())
            .unwrap();
        assert_eq!(
            header.extract(&RequestInfo::of(&exact)),
            Some(Key::from(at_limit.as_str()))
        );
        assert!(format!("{header:?}").contains("HeaderKey"));
    }

    #[tokio::test]
    async fn test_anonymous_policies_and_zero_cost() {
        use super::Anonymous;

        // Reject: no charge, the configured status.
        let layer = RateLimitLayer::new(RateLimiter::per_minute(1), PeerIp::default())
            .with_anonymous(Anonymous::Reject(StatusCode::BAD_REQUEST));
        let mut service = layer.layer(echo());
        let response = status(&mut service, Request::new(String::new())).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers().get("retry-after").is_none());
        // Key: charged to a chosen key, which can have its own quota.
        let limiter = Arc::new(
            RateLimiter::builder()
                .per_minute(1)
                .quota_for(|key| (key.as_bytes() == b"anon").then(|| Quota::per_minute(3)))
                .build(),
        );
        let layer = RateLimitLayer::shared(Arc::clone(&limiter), PeerIp::default())
            .with_anonymous(Anonymous::Key(Key::from("anon")));
        let mut service = layer.layer(echo());
        for _ in 0..3 {
            let response = status(&mut service, Request::new(String::new())).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = status(&mut service, Request::new(String::new())).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // RN2-008: a zero cost still limits (it counts as one).
        let layer = RateLimitLayer::new(RateLimiter::per_minute(1), PeerIp::default()).with_cost(0);
        let mut service = layer.layer(echo());
        let first = status(&mut service, from_peer([10, 0, 0, 1])).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = status(&mut service, from_peer([10, 0, 0, 1])).await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn test_policy_header_reports_the_keys_own_override() {
        // RN2-009: a key under a per-key quota is described by that quota.
        let frozen = Arc::new(clock_lib::ManualClock::new());
        let vip = Key::ip_prefix("10.0.0.9".parse().unwrap(), 32, 64);
        let limiter = Arc::new(
            RateLimiter::builder()
                .per_minute(10)
                .quota_for(move |key| (key == &vip).then(|| Quota::per_minute(100)))
                .clock(frozen)
                .build(),
        );
        let layer = RateLimitLayer::shared(limiter, PeerIp::default())
            .with_policy("tier")
            .with_headers_on_success(true);
        let mut service = layer.layer(echo());
        let vip = status(&mut service, from_peer([10, 0, 0, 9])).await;
        assert_eq!(vip.headers()["ratelimit-limit"], "100");
        assert_eq!(vip.headers()["ratelimit-policy"], r#""tier";q=100;w=60"#);
        let free = status(&mut service, from_peer([10, 0, 0, 8])).await;
        assert_eq!(free.headers()["ratelimit-policy"], r#""tier";q=10;w=60"#);
    }

    #[test]
    fn test_anonymous_key_never_equals_an_extracted_key() {
        let anonymous = Key::anonymous();
        assert_ne!(anonymous, Key::from(""));
        assert_ne!(anonymous, Key::from(&[0u8; 16]));
    }
}
