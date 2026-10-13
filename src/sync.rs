//! Synchronisation primitives, swapped for `loom`'s under `cfg(loom)`.
//!
//! Every atomic and lock the limiter's own state uses is imported from here, so
//! a `RUSTFLAGS="--cfg loom"` build model-checks the production code itself —
//! the real store, the real algorithm state — rather than a hand-written copy of
//! it. Outside `cfg(loom)` these are plain re-exports with no cost.

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(loom)]
pub(crate) use loom::sync::{Mutex, MutexGuard};

#[cfg(not(loom))]
pub(crate) use core::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
pub(crate) use std::sync::{Mutex, MutexGuard};

use std::sync::PoisonError;

/// Recovers a mutex guard even if a previous holder panicked. A poisoned limiter
/// lock should degrade gracefully — keep limiting — rather than propagate a
/// panic into every caller.
#[inline]
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
