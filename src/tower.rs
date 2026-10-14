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

use crate::key::Key;
use crate::limiter::RateLimiter;
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
/// fewer entries (a request that bypassed a proxy, or a misconfiguration) or an
/// unparseable entry yields no key, and the request is charged to the shared
/// anonymous key — never to an address the client chose.
///
/// With `trusted_proxies == 0` the header is ignored and the peer address is
/// used, as [`PeerIp`].
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
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardedFor {
    trusted_proxies: usize,
    prefixes: Prefixes,
}

impl ForwardedFor {
    /// Trusts the rightmost `trusted_proxies` hops of `X-Forwarded-For`.
    #[must_use]
    pub const fn trusting(trusted_proxies: usize) -> Self {
        Self {
            trusted_proxies,
            prefixes: Prefixes::DEFAULT,
        }
    }

    /// Keys IPv4 clients by their first `v4` bits and IPv6 clients by their
    /// first `v6` bits (default: 32 and 64).
    #[must_use]
    pub const fn with_prefixes(mut self, v4: u8, v6: u8) -> Self {
        self.prefixes = Prefixes { v4, v6 };
        self
    }
}

impl KeyExtractor for ForwardedFor {
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        if self.trusted_proxies == 0 {
            return peer_addr(request.extensions).map(|addr| self.prefixes.key(addr));
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
/// or one with a port (`a.b.c.d:port`, `[v6]:port`).
fn parse_addr(text: &str) -> Option<IpAddr> {
    text.parse::<IpAddr>()
        .ok()
        .or_else(|| text.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
}

/// Keys a request by the value of one header — an API key, a tenant id, a
/// session token. The raw value bytes are the key, so use it with
/// [`Builder::digest_keys_longer_than`](crate::Builder::digest_keys_longer_than)
/// or [`Builder::max_key_len`](crate::Builder::max_key_len) when the header
/// comes from untrusted clients. A request without the header has no key.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderKey {
    name: HeaderName,
}

impl HeaderKey {
    /// Keys requests by the first value of the header `name`.
    #[must_use]
    pub const fn new(name: HeaderName) -> Self {
        Self { name }
    }
}

impl KeyExtractor for HeaderKey {
    fn extract(&self, request: &RequestInfo<'_>) -> Option<Key> {
        request
            .headers
            .get(&self.name)
            .map(|value| Key::from(value.as_bytes()))
    }
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
            }),
        }
    }

    /// Charges `cost` units per request instead of one.
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
        self.map_config(|config| config.cost = cost)
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
        let key = config
            .extractor
            .extract(&RequestInfo::of(&request))
            .unwrap_or_else(Key::anonymous);
        let decision = self.limiter.check_ref(key.as_key_ref(), config.cost);
        if decision.is_allow() {
            let headers = config.headers_on_success.then(|| {
                let mut headers = HeaderMap::new();
                let snapshot = self.limiter.peek_ref(key.as_key_ref());
                write_headers(&mut headers, snapshot, &self.limiter, config);
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
        write_headers(response.headers_mut(), snapshot, &self.limiter, config);
        ResponseFuture::limited(response)
    }
}

/// Writes the rate-limit headers for `snapshot` into `headers`.
fn write_headers<K, C: Clock + Clone>(
    headers: &mut HeaderMap,
    snapshot: Snapshot,
    limiter: &RateLimiter<C>,
    config: &Config<K>,
) {
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
        let quota = limiter.quota().ratelimit_policy(policy).to_string();
        if let Ok(value) = HeaderValue::from_str(&quota) {
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
    use tower::{Layer, ServiceExt, service_fn};

    use super::{ForwardedFor, HeaderKey, KeyExtractor, PeerIp, RateLimitLayer, RequestInfo};
    use crate::{Key, RateLimiter};

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
        let response = status(&mut service, from_peer([10, 0, 0, 1])).await;
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
    fn test_anonymous_key_never_equals_an_extracted_key() {
        let anonymous = Key::anonymous();
        assert_ne!(anonymous, Key::from(""));
        assert_ne!(anonymous, Key::from(&[0u8; 16]));
    }
}
