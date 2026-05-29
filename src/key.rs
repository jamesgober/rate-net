//! The per-key identity a limit is tracked against.

use std::net::IpAddr;

/// An opaque identity a rate limit is tracked against.
///
/// A `Key` is whatever distinguishes one caller (or tenant, route, or resource)
/// from another: an IP address, a user id, an API token, an endpoint name. It is
/// stored as an owned byte string, and two keys are equal exactly when their
/// bytes are equal — so the identity is the byte content, not the source type.
///
/// You rarely name `Key` directly. The check methods accept
/// `impl Into<Key>`, and this module provides `From` conversions for the common
/// identity types, so `limiter.check("user:42")` and
/// `limiter.check(ip)` both work.
///
/// # Examples
///
/// ```
/// use rate_net::Key;
/// use std::net::{IpAddr, Ipv4Addr};
///
/// // The source type does not matter — only the bytes do.
/// let from_str: Key = "203.0.113.7".into();
/// let from_ip: Key = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)).into();
/// // (these are different byte encodings, hence different keys)
/// assert_ne!(from_str, from_ip);
///
/// let a: Key = "tenant:acme".into();
/// let b: Key = String::from("tenant:acme").into();
/// assert_eq!(a, b);
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key(Box<[u8]>);

impl Key {
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
        &self.0
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Self(value.as_bytes().into())
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        Self(value.into_bytes().into_boxed_slice())
    }
}

impl From<&[u8]> for Key {
    fn from(value: &[u8]) -> Self {
        Self(value.into())
    }
}

impl From<Vec<u8>> for Key {
    fn from(value: Vec<u8>) -> Self {
        Self(value.into_boxed_slice())
    }
}

impl From<u64> for Key {
    fn from(value: u64) -> Self {
        Self(Box::new(value.to_be_bytes()) as Box<[u8]>)
    }
}

impl From<IpAddr> for Key {
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(addr) => Self(Box::new(addr.octets()) as Box<[u8]>),
            IpAddr::V6(addr) => Self(Box::new(addr.octets()) as Box<[u8]>),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Key;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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
}
