//! SemVer guard: implementations a 1.0 downstream crate could legally write
//! must keep compiling.
//!
//! `&T` and `Box<T>` are fundamental types, so under the orphan rules a crate
//! depending on rate-net 1.0 may implement `Limiter` for `&TheirType` and
//! `Box<TheirType>`. A blanket `impl<L: Limiter> Limiter for &L` (or `Box<L>`)
//! in rate-net would make those impls conflict (E0119) — a breaking change.
//! This file *is* such a downstream crate: if a blanket impl is reintroduced,
//! it stops compiling.

#![cfg(feature = "std")]

use rate_net::{Decision, Key, Limiter};

/// A downstream limiter type.
struct AlwaysAllow;

impl Limiter for AlwaysAllow {
    fn check_n(&self, _key: impl Into<Key>, _n: u32) -> Decision {
        Decision::Allow
    }
}

/// Legal in 1.0: `&AlwaysAllow` is a local type for the orphan rules.
impl Limiter for &AlwaysAllow {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        (**self).check_n(key, n)
    }
}

/// Legal in 1.0: `Box<AlwaysAllow>` is a local type for the orphan rules.
impl Limiter for Box<AlwaysAllow> {
    fn check_n(&self, key: impl Into<Key>, n: u32) -> Decision {
        (**self).check_n(key, n)
    }
}

fn admit<L: Limiter>(limiter: L) -> bool {
    limiter.check("k").is_allow()
}

#[test]
fn test_downstream_impls_for_references_and_boxes_still_compile() {
    let limiter = AlwaysAllow;
    assert!(admit(&limiter));
    assert!(admit(Box::new(AlwaysAllow)));
    assert!(admit(AlwaysAllow));
}
