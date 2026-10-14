//! The per-key identity a limit is tracked against.

use core::fmt;
use core::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

/// Keys up to this many bytes are stored inline, with no heap allocation. It
/// covers the common identities — an IPv6 address (16 bytes), an IPv6 prefix
/// key (17), a `u64` id (8), and short string keys — so the steady-state check
/// path allocates nothing.
const INLINE_CAP: usize = 22;

/// What a key's bytes mean. Part of the key's identity: two keys are equal only
/// if their kinds *and* bytes are equal, so a tagged numeric id can never
/// collide with an attacker-chosen string that happens to share its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
enum Kind {
    /// Untagged bytes: every `From` conversion. Raw keys of different source
    /// types share one namespace (the 1.0 behaviour).
    Raw,
    /// A numeric id, from [`Key::id`].
    Id,
    /// A canonical IP address, from [`Key::ip`].
    Ip,
    /// A masked IP prefix plus its length, from the `*_prefix` constructors.
    IpPrefix,
    /// A keyed 128-bit digest standing in for a key longer than the limiter's
    /// digest threshold (`Builder::digest_keys_longer_than`). Never produced by
    /// a public constructor, so no caller-built key can equal one.
    Digest,
}

#[derive(Clone)]
enum Repr {
    /// Bytes stored inline; `len` of them are live.
    Inline {
        kind: Kind,
        len: u8,
        buf: [u8; INLINE_CAP],
    },
    /// Bytes too long to inline, on the heap. Shared, so cloning a long key
    /// (passing `&key` to a check, retrying in a wait loop) never allocates.
    Heap { kind: Kind, bytes: Arc<[u8]> },
}

/// An opaque identity a rate limit is tracked against.
///
/// A `Key` is whatever distinguishes one caller (or tenant, route, or resource)
/// from another: an IP address, a user id, an API token, an endpoint name. It is
/// stored as an owned byte string — inline for keys up to a couple dozen bytes,
/// on the heap beyond that — plus a *kind* tag.
///
/// You rarely name `Key` directly. The check methods accept `impl Into<Key>`,
/// and this module provides `From` conversions for the common identity types, so
/// `limiter.check("user:42")` and `limiter.check(ip)` both work. Small keys are
/// stored inline, which is what keeps an existing-key check allocation-free.
///
/// # Raw and tagged keys
///
/// The `From` conversions produce **raw** keys: the identity is the bytes, not
/// the source type, so `Key::from(1u64)` equals `Key::from(&[0, 0, 0, 0, 0, 0,
/// 0, 1][..])`, and an IPv4 address equals the 4-byte string with the same
/// bytes. That is fine for a limiter that holds one kind of key. A limiter that
/// mixes kinds — numeric user ids next to attacker-chosen strings — should use
/// the **tagged** constructors, whose kind is part of the identity, so no
/// string can ever collide with them:
///
/// - [`Key::id`] — a numeric id.
/// - [`Key::ip`] — an IP address, canonicalised (an IPv4-mapped IPv6 address
///   is the same key as the IPv4 address).
/// - [`Key::ipv4_prefix`], [`Key::ipv6_prefix`], [`Key::ip_prefix`] — an
///   address masked to a network prefix. Keying IPv6 clients by their `/64`
///   stops one host from rotating through the 2^64 addresses it owns to dodge a
///   per-address limit.
///
/// # Privacy
///
/// Keys are often caller identities, so `Key`'s `Debug` shows only its kind and
/// length, never the bytes. [`as_bytes`](Self::as_bytes) is the explicit way
/// to read them.
///
/// # Examples
///
/// ```
/// use rate_net::Key;
/// use std::net::{IpAddr, Ipv6Addr};
///
/// let a: Key = "tenant:acme".into();
/// let b: Key = String::from("tenant:acme").into();
/// assert_eq!(a, b);
/// assert_eq!(a.as_bytes(), b"tenant:acme");
///
/// // Tagged keys never collide with raw ones that share their bytes.
/// assert_ne!(Key::id(42), Key::from(42u64));
///
/// // Every address in one /64 maps to the same key.
/// let one: Ipv6Addr = "2001:db8:1:2::1".parse().unwrap();
/// let two: Ipv6Addr = "2001:db8:1:2:ffff::9".parse().unwrap();
/// assert_eq!(Key::ipv6_prefix(one, 64), Key::ipv6_prefix(two, 64));
/// ```
#[derive(Clone)]
pub struct Key(Repr);

impl Key {
    /// Builds a key of `kind` from raw bytes, storing them inline when they fit.
    fn with_kind(kind: Kind, bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_CAP {
            let mut buf = [0u8; INLINE_CAP];
            buf[..bytes.len()].copy_from_slice(bytes);
            // `bytes.len() <= INLINE_CAP <= u8::MAX`, so the cast cannot truncate.
            Self(Repr::Inline {
                kind,
                len: bytes.len() as u8,
                buf,
            })
        } else {
            Self(Repr::Heap {
                kind,
                bytes: Arc::from(bytes),
            })
        }
    }

    /// The borrowed view the store looks keys up by.
    #[inline]
    pub(crate) fn as_key_ref(&self) -> KeyRef<'_> {
        KeyRef {
            kind: self.kind(),
            bytes: self.as_bytes(),
        }
    }

    /// A tagged key for a numeric id — a user, tenant, or account number.
    ///
    /// Unlike `Key::from(u64)`, the result can never equal a string or byte key,
    /// so ids and untrusted strings can share one limiter safely.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    ///
    /// assert_eq!(Key::id(7), Key::id(7));
    /// assert_ne!(Key::id(7), Key::from(&7u64.to_be_bytes()[..]));
    /// ```
    #[must_use]
    pub fn id(id: u64) -> Self {
        Self::with_kind(Kind::Id, &id.to_be_bytes())
    }

    /// A tagged key for one IP address, canonicalised: an IPv4-mapped IPv6
    /// address (`::ffff:a.b.c.d`) is the same key as the IPv4 address, so a
    /// dual-stack listener cannot hand one client two allowances.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    /// use std::net::IpAddr;
    ///
    /// let v4: IpAddr = "203.0.113.7".parse().unwrap();
    /// let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
    /// assert_eq!(Key::ip(v4), Key::ip(mapped));
    /// assert_ne!(Key::ip(v4), Key::from("203.0.113.7"));
    /// ```
    #[must_use]
    pub fn ip(addr: IpAddr) -> Self {
        match canonical(addr) {
            IpAddr::V4(v4) => Self::with_kind(Kind::Ip, &v4.octets()),
            IpAddr::V6(v6) => Self::with_kind(Kind::Ip, &v6.octets()),
        }
    }

    /// A tagged key for the IPv4 network containing `addr`: the address masked
    /// to its first `prefix_len` bits (clamped to `32`). Keys with different
    /// prefix lengths never collide.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    /// use std::net::Ipv4Addr;
    ///
    /// let a = Ipv4Addr::new(198, 51, 100, 7);
    /// let b = Ipv4Addr::new(198, 51, 100, 200);
    /// assert_eq!(Key::ipv4_prefix(a, 24), Key::ipv4_prefix(b, 24));
    /// assert_ne!(Key::ipv4_prefix(a, 32), Key::ipv4_prefix(b, 32));
    /// ```
    #[must_use]
    pub fn ipv4_prefix(addr: Ipv4Addr, prefix_len: u8) -> Self {
        let len = prefix_len.min(32);
        let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
        let masked = (u32::from(addr) & mask).to_be_bytes();
        let mut bytes = [0u8; 5];
        bytes[..4].copy_from_slice(&masked);
        bytes[4] = len;
        Self::with_kind(Kind::IpPrefix, &bytes)
    }

    /// A tagged key for the IPv6 network containing `addr`: the address masked
    /// to its first `prefix_len` bits (clamped to `128`).
    ///
    /// An IPv6 host is usually assigned a whole `/64`, so limiting per address
    /// lets one client rotate through billions of addresses to reset its
    /// allowance. Keying by `ipv6_prefix(addr, 64)` limits the subnet as one
    /// caller — the standard defence. Keys with different prefix lengths never
    /// collide.
    ///
    /// An IPv4 client reaching a dual-stack socket appears as an IPv4-mapped
    /// address (`::ffff:a.b.c.d`), and every such address shares its first 96
    /// bits — masking one to `/64` would put every IPv4 client in one key. So an
    /// IPv4-mapped or IPv4-compatible address is keyed by its IPv4 address
    /// instead, exactly as `Key::ipv4_prefix(v4, 32)`: one IPv4 address is the
    /// counterpart of one IPv6 subnet. Use [`ip_prefix`](Self::ip_prefix) to
    /// choose a different IPv4 prefix.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    /// use std::net::Ipv6Addr;
    ///
    /// let a: Ipv6Addr = "2001:db8:aa:bb::1".parse().unwrap();
    /// let b: Ipv6Addr = "2001:db8:aa:bb:1234:5678:9abc:def0".parse().unwrap();
    /// let other: Ipv6Addr = "2001:db8:aa:bc::1".parse().unwrap();
    /// assert_eq!(Key::ipv6_prefix(a, 64), Key::ipv6_prefix(b, 64));
    /// assert_ne!(Key::ipv6_prefix(a, 64), Key::ipv6_prefix(other, 64));
    /// ```
    #[must_use]
    pub fn ipv6_prefix(addr: Ipv6Addr, prefix_len: u8) -> Self {
        if let Some(v4) = embedded_ipv4(addr) {
            return Self::ipv4_prefix(v4, 32);
        }
        let len = prefix_len.min(128);
        let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
        let masked = (u128::from(addr) & mask).to_be_bytes();
        let mut bytes = [0u8; 17];
        bytes[..16].copy_from_slice(&masked);
        bytes[16] = len;
        Self::with_kind(Kind::IpPrefix, &bytes)
    }

    /// A tagged network key for any address: IPv4 (including IPv4-mapped IPv6)
    /// masked to `v4_prefix_len`, IPv6 masked to `v6_prefix_len`.
    ///
    /// `Key::ip_prefix(addr, 32, 64)` is the usual per-client key for a
    /// dual-stack service: exact IPv4 addresses, IPv6 `/64`s.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    /// use std::net::IpAddr;
    ///
    /// let client: IpAddr = "2001:db8::42".parse().unwrap();
    /// let neighbour: IpAddr = "2001:db8::99".parse().unwrap();
    /// assert_eq!(Key::ip_prefix(client, 32, 64), Key::ip_prefix(neighbour, 32, 64));
    /// ```
    #[must_use]
    pub fn ip_prefix(addr: IpAddr, v4_prefix_len: u8, v6_prefix_len: u8) -> Self {
        match canonical(addr) {
            IpAddr::V4(v4) => Self::ipv4_prefix(v4, v4_prefix_len),
            IpAddr::V6(v6) => Self::ipv6_prefix(v6, v6_prefix_len),
        }
    }

    /// Borrows the raw bytes that make up the key (without its kind tag).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    ///
    /// let key: Key = "abc".into();
    /// assert_eq!(key.as_bytes(), b"abc");
    /// assert_eq!(Key::id(1).as_bytes(), &[0, 0, 0, 0, 0, 0, 0, 1]);
    /// ```
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            Repr::Inline { buf, len, .. } => &buf[..usize::from(*len)],
            Repr::Heap { bytes, .. } => bytes,
        }
    }

    /// The number of bytes in the key (without its kind tag).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    ///
    /// assert_eq!(Key::from("abc").len(), 3);
    /// assert_eq!(Key::id(7).len(), 8);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    /// Whether the key has no bytes (the empty string or slice).
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    ///
    /// assert!(Key::from("").is_empty());
    /// assert!(!Key::from("a").is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    /// The key's kind tag.
    const fn kind(&self) -> Kind {
        match &self.0 {
            Repr::Inline { kind, .. } | Repr::Heap { kind, .. } => *kind,
        }
    }
}

/// The IPv4 address embedded in an IPv4-mapped (`::ffff:a.b.c.d`) or
/// IPv4-compatible (`::a.b.c.d`) IPv6 address. The unspecified address `::` and
/// loopback `::1` are genuine IPv6 addresses, not compatible ones.
fn embedded_ipv4(addr: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = addr.to_ipv4_mapped() {
        return Some(v4);
    }
    let bits = u128::from(addr);
    if bits >> 32 == 0 && bits > 1 {
        // The top 96 bits are zero, so the low 32 are the whole address.
        return Some(Ipv4Addr::from(bits as u32));
    }
    None
}

/// `addr` with any embedded IPv4 address unwrapped, so one client never has
/// two identities.
fn canonical(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => embedded_ipv4(v6).map_or(addr, IpAddr::V4),
        IpAddr::V4(_) => addr,
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_key_ref() == other.as_key_ref()
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_key_ref().hash(state);
    }
}

/// A borrowed key: its kind and its bytes, without owning them.
///
/// The store finds an existing key by this view, so a check never has to build
/// (and, for a long key, allocate) an owned [`Key`]; one is built only when the
/// key is first stored. It hashes and compares exactly like the `Key` it
/// stands for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyRef<'a> {
    kind: Kind,
    bytes: &'a [u8],
}

impl<'a> KeyRef<'a> {
    /// A raw (untagged) key over `bytes` — the view of `Key::from(bytes)`.
    #[inline]
    pub(crate) const fn raw(bytes: &'a [u8]) -> Self {
        Self {
            kind: Kind::Raw,
            bytes,
        }
    }

    /// The view of a digest key over its 16 digest bytes.
    #[inline]
    pub(crate) const fn digest(digest: &'a [u8; 16]) -> Self {
        Self {
            kind: Kind::Digest,
            bytes: digest,
        }
    }

    /// The key's bytes.
    #[inline]
    pub(crate) const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The owned key this view stands for.
    pub(crate) fn to_key(self) -> Key {
        Key::with_kind(self.kind, self.bytes)
    }
}

impl Hash for KeyRef<'_> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.bytes.hash(state);
    }
}

impl fmt::Debug for Key {
    /// Shows the kind and length only — keys are often caller identities, so
    /// their bytes never reach a log through `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("kind", &self.kind())
            .field("len", &self.as_bytes().len())
            .finish_non_exhaustive()
    }
}

impl From<&Key> for Key {
    fn from(value: &Key) -> Self {
        value.clone()
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Self::with_kind(Kind::Raw, value.as_bytes())
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        Self::with_kind(Kind::Raw, value.as_bytes())
    }
}

impl From<&String> for Key {
    fn from(value: &String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<&[u8]> for Key {
    fn from(value: &[u8]) -> Self {
        Self::with_kind(Kind::Raw, value)
    }
}

impl From<Vec<u8>> for Key {
    fn from(value: Vec<u8>) -> Self {
        Self::with_kind(Kind::Raw, &value)
    }
}

impl<const N: usize> From<[u8; N]> for Key {
    /// A raw key of the array's bytes — a digest, a UUID's 16 bytes, a fixed
    /// identifier — equal to `Key::from(&array[..])`.
    fn from(value: [u8; N]) -> Self {
        Self::with_kind(Kind::Raw, &value)
    }
}

impl<const N: usize> From<&[u8; N]> for Key {
    /// A raw key of the array's bytes, so a byte-string literal (`b"..."`)
    /// works directly.
    fn from(value: &[u8; N]) -> Self {
        Self::with_kind(Kind::Raw, value)
    }
}

impl From<u64> for Key {
    /// A raw key of the id's 8 big-endian bytes. Prefer [`Key::id`] when the
    /// limiter also holds string keys.
    fn from(value: u64) -> Self {
        Self::with_kind(Kind::Raw, &value.to_be_bytes())
    }
}

impl From<IpAddr> for Key {
    /// A raw key of the address octets (4 or 16 bytes). Prefer [`Key::ip`] or
    /// [`Key::ip_prefix`] for client keys.
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(addr) => Self::from(addr),
            IpAddr::V6(addr) => Self::from(addr),
        }
    }
}

impl From<Ipv4Addr> for Key {
    /// A raw key of the 4 address octets, equal to `Key::from(IpAddr::V4(..))`.
    fn from(value: Ipv4Addr) -> Self {
        Self::with_kind(Kind::Raw, &value.octets())
    }
}

impl From<Ipv6Addr> for Key {
    /// A raw key of the 16 address octets, equal to `Key::from(IpAddr::V6(..))`.
    fn from(value: Ipv6Addr) -> Self {
        Self::with_kind(Kind::Raw, &value.octets())
    }
}

impl From<SocketAddr> for Key {
    /// A raw key of the peer's IP address. The port is dropped: a client opens
    /// a new port per connection, so a port in the key would give it a fresh
    /// allowance every time.
    fn from(value: SocketAddr) -> Self {
        Self::from(value.ip())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{INLINE_CAP, Key, KeyRef, Repr};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn is_inline(key: &Key) -> bool {
        matches!(key.0, Repr::Inline { .. })
    }

    #[test]
    fn test_str_and_string_are_equal_for_same_bytes() {
        let a: Key = "user:42".into();
        let b: Key = String::from("user:42").into();
        let s = String::from("user:42");
        let c: Key = (&s).into();
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.as_bytes(), b"user:42");
    }

    #[test]
    fn test_distinct_content_is_distinct() {
        let a: Key = "a".into();
        let b: Key = "b".into();
        assert_ne!(a, b);
    }

    #[test]
    fn test_byte_slice_and_vec_round_trip() {
        let bytes: &[u8] = &[1, 2, 3, 4];
        let a: Key = bytes.into();
        let b: Key = vec![1u8, 2, 3, 4].into();
        assert_eq!(a, b);
        assert_eq!(a.as_bytes(), bytes);
        assert_eq!(Key::from(&a), a);
    }

    #[test]
    fn test_u64_uses_big_endian_bytes() {
        let key: Key = 1u64.into();
        assert_eq!(key.as_bytes(), &[0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn test_ip_addresses_encode_their_octets() {
        let v4: Key = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)).into();
        assert_eq!(v4.as_bytes(), &[10, 0, 0, 1]);
        assert_eq!(v4, Key::from(Ipv4Addr::new(10, 0, 0, 1)));

        let v6: Key = IpAddr::V6(Ipv6Addr::LOCALHOST).into();
        assert_eq!(v6.as_bytes().len(), 16);
        assert_eq!(v6, Key::from(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn test_socket_address_keys_by_ip_only() {
        let a: SocketAddr = "203.0.113.9:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        assert_eq!(Key::from(a), Key::from(b));
        assert_eq!(Key::from(a), Key::from(a.ip()));
    }

    #[test]
    fn test_raw_keys_of_different_types_share_a_namespace() {
        // The documented 1.0 behaviour, kept for compatibility.
        assert_eq!(Key::from(1u64), Key::from(&[0u8, 0, 0, 0, 0, 0, 0, 1][..]));
        assert_eq!(Key::from(Ipv4Addr::new(97, 98, 99, 100)), Key::from("abcd"));
    }

    #[test]
    fn test_tagged_keys_never_collide_with_raw_or_each_other() {
        let id = Key::id(0x6162_6364_6566_6768);
        assert_ne!(id, Key::from("abcdefgh"));
        assert_ne!(id, Key::from(0x6162_6364_6566_6768u64));
        let ip = Key::ip(IpAddr::V4(Ipv4Addr::new(97, 98, 99, 100)));
        assert_ne!(ip, Key::from("abcd"));
        assert_ne!(ip, Key::from(Ipv4Addr::new(97, 98, 99, 100)));
        assert_ne!(
            Key::ipv4_prefix(Ipv4Addr::new(97, 98, 99, 100), 32),
            ip,
            "a prefix key is its own kind"
        );
        assert_eq!(Key::id(5), Key::id(5));
    }

    #[test]
    fn test_ip_key_canonicalises_mapped_ipv4() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 0, 2, 1).to_ipv6_mapped());
        assert_eq!(Key::ip(v4), Key::ip(mapped));
        assert_eq!(Key::ip_prefix(mapped, 24, 64), Key::ip_prefix(v4, 24, 64));
        // The raw conversion keeps the 1.0 bytes.
        assert_ne!(Key::from(v4), Key::from(mapped));
    }

    #[test]
    fn test_ipv6_prefix_keys_embedded_ipv4_by_address() {
        // Two unrelated IPv4 clients seen through a dual-stack socket must not
        // share one /64 key.
        let a = Ipv4Addr::new(198, 51, 100, 7);
        let b = Ipv4Addr::new(203, 0, 113, 9);
        assert_ne!(
            Key::ipv6_prefix(a.to_ipv6_mapped(), 64),
            Key::ipv6_prefix(b.to_ipv6_mapped(), 64)
        );
        assert_eq!(
            Key::ipv6_prefix(a.to_ipv6_mapped(), 64),
            Key::ipv4_prefix(a, 32)
        );
        // IPv4-compatible addresses too, and every IP constructor agrees.
        assert_eq!(
            Key::ipv6_prefix(a.to_ipv6_compatible(), 64),
            Key::ipv4_prefix(a, 32)
        );
        assert_eq!(
            Key::ip(IpAddr::V6(a.to_ipv6_compatible())),
            Key::ip(IpAddr::V4(a))
        );
        assert_eq!(
            Key::ip_prefix(IpAddr::V6(a.to_ipv6_compatible()), 24, 64),
            Key::ipv4_prefix(a, 24)
        );
        // `::` and `::1` stay IPv6.
        assert_ne!(
            Key::ipv6_prefix(Ipv6Addr::LOCALHOST, 128),
            Key::ipv4_prefix(Ipv4Addr::new(0, 0, 0, 1), 32)
        );
        assert_ne!(
            Key::ip(IpAddr::V6(Ipv6Addr::UNSPECIFIED)),
            Key::ip(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        );
    }

    #[test]
    fn test_prefix_keys_mask_and_encode_length() {
        let a = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 3, 4, 5, 6);
        let b = Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0xffff, 0, 0, 1);
        let c = Ipv6Addr::new(0x2001, 0xdb8, 1, 3, 3, 4, 5, 6);
        assert_eq!(Key::ipv6_prefix(a, 64), Key::ipv6_prefix(b, 64));
        assert_ne!(Key::ipv6_prefix(a, 64), Key::ipv6_prefix(c, 64));
        assert_eq!(Key::ipv6_prefix(a, 48), Key::ipv6_prefix(c, 48));
        // Same masked bytes, different lengths: different keys.
        assert_ne!(Key::ipv6_prefix(a, 48), Key::ipv6_prefix(a, 47));
        // /0 is everything; lengths past the address width clamp.
        assert_eq!(Key::ipv6_prefix(a, 0), Key::ipv6_prefix(c, 0));
        assert_eq!(Key::ipv6_prefix(a, 200), Key::ipv6_prefix(a, 128));
        assert_ne!(Key::ipv6_prefix(a, 128), Key::ipv6_prefix(b, 128));

        let x = Ipv4Addr::new(10, 1, 2, 3);
        let y = Ipv4Addr::new(10, 1, 2, 250);
        assert_eq!(Key::ipv4_prefix(x, 24), Key::ipv4_prefix(y, 24));
        assert_ne!(Key::ipv4_prefix(x, 32), Key::ipv4_prefix(y, 32));
        assert_eq!(Key::ipv4_prefix(x, 0), Key::ipv4_prefix(y, 0));
        assert_eq!(Key::ipv4_prefix(x, 99), Key::ipv4_prefix(x, 32));
        assert_eq!(Key::ipv4_prefix(x, 24).as_bytes(), &[10, 1, 2, 0, 24]);
        assert_eq!(
            Key::ip_prefix(IpAddr::V6(a), 32, 64),
            Key::ipv6_prefix(a, 64)
        );
    }

    #[test]
    fn test_debug_redacts_the_bytes() {
        let key = Key::from("secret-api-token");
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("len: 16"), "{rendered}");
        assert!(format!("{:?}", Key::id(1)).contains("Id"));
    }

    #[test]
    fn test_small_keys_are_stored_inline() {
        // The common identities all fit inline (no heap allocation).
        assert!(is_inline(&Key::from("user:42")));
        assert!(is_inline(&Key::from(1u64)));
        assert!(is_inline(&Key::id(1)));
        assert!(is_inline(&Key::from(IpAddr::V6(Ipv6Addr::LOCALHOST))));
        assert!(is_inline(&Key::ipv6_prefix(Ipv6Addr::LOCALHOST, 64)));
        // Exactly at the inline boundary.
        let at_cap = vec![b'x'; INLINE_CAP];
        assert!(is_inline(&Key::from(at_cap.as_slice())));
        assert!(is_inline(&Key::from(at_cap)));
    }

    #[test]
    fn test_arrays_are_raw_byte_keys() {
        let bytes = [7u8; 16];
        assert_eq!(Key::from(bytes), Key::from(&bytes[..]));
        assert_eq!(Key::from(&bytes), Key::from(&bytes[..]));
        assert_eq!(Key::from(b"abc"), Key::from("abc"));
        assert_eq!(Key::from(b"abc").len(), 3);
        assert!(Key::from(b"").is_empty());
    }

    #[test]
    fn test_key_ref_hashes_and_compares_like_its_key() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash<T: Hash>(value: &T) -> u64 {
            let mut h = DefaultHasher::new();
            value.hash(&mut h);
            h.finish()
        }

        let long = "a-key-long-enough-to-live-on-the-heap";
        for key in [
            Key::from("short"),
            Key::from(long),
            Key::id(9),
            Key::ip(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        ] {
            let view = key.as_key_ref();
            assert_eq!(hash(&key), hash(&view));
            assert_eq!(view.to_key(), key);
            assert_eq!(view.bytes(), key.as_bytes());
        }
        assert_eq!(KeyRef::raw(long.as_bytes()).to_key(), Key::from(long));
        assert_eq!(hash(&KeyRef::raw(long.as_bytes())), hash(&Key::from(long)));
        let digest = [1u8; 16];
        assert_ne!(KeyRef::digest(&digest).to_key(), Key::from(digest));
    }

    #[test]
    fn test_cloning_a_long_key_shares_its_bytes() {
        let long = Key::from("a-key-long-enough-to-live-on-the-heap");
        let copy = long.clone();
        assert_eq!(long.as_bytes().as_ptr(), copy.as_bytes().as_ptr());
    }

    #[test]
    fn test_key_stays_compact() {
        assert!(core::mem::size_of::<Key>() <= 32);
    }

    #[test]
    fn test_large_keys_spill_to_heap_and_compare_equal() {
        let long = vec![b'y'; INLINE_CAP + 1];
        let from_slice: Key = long.as_slice().into();
        let from_vec: Key = long.clone().into();
        let from_string: Key = String::from_utf8(long.clone()).unwrap_or_default().into();
        assert!(!is_inline(&from_slice));
        assert!(!is_inline(&from_vec));
        assert_eq!(from_slice, from_vec);
        assert_eq!(from_slice, from_string);
        assert_eq!(from_slice.as_bytes(), long.as_slice());
    }

    #[test]
    fn test_inline_and_heap_hash_consistently_by_bytes() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash(key: &Key) -> u64 {
            let mut h = DefaultHasher::new();
            key.hash(&mut h);
            h.finish()
        }

        // Same bytes always take the same representation, but confirm equal keys
        // hash equal regardless.
        let a: Key = "short".into();
        let b: Key = String::from("short").into();
        assert_eq!(hash(&a), hash(&b));
        assert_ne!(hash(&Key::id(1)), hash(&Key::from(1u64)));
    }
}
