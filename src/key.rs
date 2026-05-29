//! The per-key identity a limit is tracked against.

use core::fmt;
use core::hash::{Hash, Hasher};
use std::net::IpAddr;

/// Keys up to this many bytes are stored inline, with no heap allocation. It
/// covers the common identities — an IPv6 address (16 bytes), a `u64` id (8),
/// and short string keys — so the steady-state check path allocates nothing.
const INLINE_CAP: usize = 23;

#[derive(Clone)]
enum Repr {
    /// Bytes stored inline; `len` of them are live.
    Inline { buf: [u8; INLINE_CAP], len: u8 },
    /// Bytes too long to inline, on the heap.
    Heap(Box<[u8]>),
}

/// An opaque identity a rate limit is tracked against.
///
/// A `Key` is whatever distinguishes one caller (or tenant, route, or resource)
/// from another: an IP address, a user id, an API token, an endpoint name. It is
/// stored as an owned byte string — inline for keys up to a couple dozen bytes,
/// on the heap beyond that — and two keys are equal exactly when their bytes are
/// equal, so the identity is the byte content, not the source type.
///
/// You rarely name `Key` directly. The check methods accept `impl Into<Key>`,
/// and this module provides `From` conversions for the common identity types, so
/// `limiter.check("user:42")` and `limiter.check(ip)` both work. Small keys are
/// stored inline, which is what keeps an existing-key check allocation-free.
///
/// # Examples
///
/// ```
/// use rate_net::Key;
///
/// let a: Key = "tenant:acme".into();
/// let b: Key = String::from("tenant:acme").into();
/// assert_eq!(a, b);
/// assert_eq!(a.as_bytes(), b"tenant:acme");
/// ```
#[derive(Clone)]
pub struct Key(Repr);

impl Key {
    /// Builds a key from raw bytes, storing them inline when they fit.
    fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.len() <= INLINE_CAP {
            let mut buf = [0u8; INLINE_CAP];
            buf[..bytes.len()].copy_from_slice(bytes);
            // `bytes.len() <= INLINE_CAP <= u8::MAX`, so the cast cannot truncate.
            Self(Repr::Inline {
                buf,
                len: bytes.len() as u8,
            })
        } else {
            Self(Repr::Heap(bytes.into()))
        }
    }

    /// Borrows the raw bytes that make up the key.
    ///
    /// # Examples
    ///
    /// ```
    /// use rate_net::Key;
    ///
    /// let key: Key = "abc".into();
    /// assert_eq!(key.as_bytes(), b"abc");
    /// ```
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.0 {
            Repr::Inline { buf, len } => &buf[..*len as usize],
            Repr::Heap(bytes) => bytes,
        }
    }
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Key").field(&self.as_bytes()).finish()
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Self::from_bytes(value.as_bytes())
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        if value.len() <= INLINE_CAP {
            Self::from_bytes(value.as_bytes())
        } else {
            Self(Repr::Heap(value.into_bytes().into_boxed_slice()))
        }
    }
}

impl From<&[u8]> for Key {
    fn from(value: &[u8]) -> Self {
        Self::from_bytes(value)
    }
}

impl From<Vec<u8>> for Key {
    fn from(value: Vec<u8>) -> Self {
        if value.len() <= INLINE_CAP {
            Self::from_bytes(&value)
        } else {
            Self(Repr::Heap(value.into_boxed_slice()))
        }
    }
}

impl From<u64> for Key {
    fn from(value: u64) -> Self {
        Self::from_bytes(&value.to_be_bytes())
    }
}

impl From<IpAddr> for Key {
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(addr) => Self::from_bytes(&addr.octets()),
            IpAddr::V6(addr) => Self::from_bytes(&addr.octets()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{INLINE_CAP, Key, Repr};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn is_inline(key: &Key) -> bool {
        matches!(key.0, Repr::Inline { .. })
    }

    #[test]
    fn test_str_and_string_are_equal_for_same_bytes() {
        let a: Key = "user:42".into();
        let b: Key = String::from("user:42").into();
        assert_eq!(a, b);
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

        let v6: Key = IpAddr::V6(Ipv6Addr::LOCALHOST).into();
        assert_eq!(v6.as_bytes().len(), 16);
    }

    #[test]
    fn test_small_keys_are_stored_inline() {
        // The common identities all fit inline (no heap allocation).
        assert!(is_inline(&Key::from("user:42")));
        assert!(is_inline(&Key::from(1u64)));
        assert!(is_inline(&Key::from(IpAddr::V6(Ipv6Addr::LOCALHOST))));
        // Exactly at the inline boundary.
        let at_cap = vec![b'x'; INLINE_CAP];
        assert!(is_inline(&Key::from(at_cap.as_slice())));
    }

    #[test]
    fn test_large_keys_spill_to_heap_and_compare_equal() {
        let long = vec![b'y'; INLINE_CAP + 1];
        let from_slice: Key = long.as_slice().into();
        let from_vec: Key = long.clone().into();
        assert!(!is_inline(&from_slice));
        assert_eq!(from_slice, from_vec);
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
    }
}
