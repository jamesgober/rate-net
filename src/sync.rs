//! Synchronisation primitives, swapped for `loom`'s under `cfg(loom)`.
//!
//! Every atomic and lock the limiter's own state uses is imported from here, so
//! a `RUSTFLAGS="--cfg loom"` build model-checks the production code itself —
//! the real store, the real algorithm state — rather than a hand-written copy of
//! it. Outside `cfg(loom)` these are plain re-exports with no cost.
//!
//! The atomics are available in every build with 64-bit atomics (the `no_std`
//! [`Gcra`](crate::Gcra) needs nothing else); the locks only with `std`.

#[cfg(all(loom, feature = "std"))]
pub(crate) use loom::sync::atomic::AtomicBool;
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(all(loom, feature = "std", feature = "algorithms"))]
pub(crate) use loom::sync::{Mutex, MutexGuard};
#[cfg(all(loom, feature = "std"))]
pub(crate) use loom::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(all(not(loom), feature = "std"))]
pub(crate) use core::sync::atomic::AtomicBool;
#[cfg(all(not(loom), target_has_atomic = "64"))]
pub(crate) use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(all(not(loom), feature = "std", feature = "algorithms"))]
pub(crate) use std::sync::{Mutex, MutexGuard};
#[cfg(all(not(loom), feature = "std"))]
pub(crate) use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "std")]
use std::sync::PoisonError;

/// Recovers a read guard even if a previous holder panicked. A poisoned limiter
/// lock should degrade gracefully — keep limiting — rather than propagate a
/// panic into every caller.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

/// Recovers a write guard even if a previous holder panicked. See [`read`].
#[cfg(feature = "std")]
#[inline]
pub(crate) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// Recovers a mutex guard even if a previous holder panicked. See [`read`].
#[cfg(all(feature = "std", feature = "algorithms"))]
#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
