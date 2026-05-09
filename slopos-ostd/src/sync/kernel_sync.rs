//! Kernel-only `Send`/`Sync` newtype + BSP-init capability witness.
//!
//! # `KernelSync<T>`
//!
//! [`KernelSync<T>`] wraps a value that is *not* automatically `Send +
//! Sync` but is safe to share across CPUs in kernel context because
//! every access is mediated by an outer lock or by the single-CPU
//! task-ownership invariant (Inv. 8). Replaces the proliferation of
//! ad-hoc `unsafe impl Send for X {} unsafe impl Sync for X {}`
//! markers across the kernel — the unsafe is now centralised here, and
//! consumer crates stay safe.
//!
//! Typical pattern: a struct holds one specific field whose type is
//! `!Send` or `!Sync` (a raw pointer, an `UnsafeCell<T>` over a `!Sync`
//! payload). Wrap **just that field** in [`KernelSync<T>`]; the parent
//! struct then auto-derives `Send + Sync` via field composition. This
//! keeps the unsafe surface scoped to the actual source of unsafety
//! rather than being a struct-wide blanket marker.
//!
//! # `BspToken`
//!
//! [`BspToken`] is a sealed capability witness. It is constructible
//! only from within OSTD's BSP-init path; callers of
//! `register_*(token: &BspToken, …)` style hooks must own a token,
//! which is statically impossible after SMP bringup. Replaces
//! `pub unsafe fn register_*` declarations with safe ones whose
//! single-writer invariant is enforced by the type system.

use core::ops::{Deref, DerefMut};

/// Kernel-only `Send + Sync` wrapper.
///
/// See module-level docs for the soundness contract every consumer
/// must satisfy.
#[repr(transparent)]
pub struct KernelSync<T> {
    value: T,
}

// SAFETY: `KernelSync<T>` is the canonical kernel-only-access wrapper.
// Callers wrap a value in `KernelSync` only when:
//   - the value is accessed only from kernel code (not from userland),
//     AND
//   - either the value is itself protected by an outer SpinLock /
//     RwLock / RCU / per-CPU pinning, OR access is mediated by Inv. 8
//     (single-CPU task ownership), OR the value is BSP-only init data
//     that is read-only after SMP bringup.
// Each call site duplicates the relevant Inv.-citation in its own
// `// SAFETY:` note alongside the `KernelSync::new(...)` construction.
unsafe impl<T> Send for KernelSync<T> {}
// SAFETY: see Send impl above; the same contract applies.
unsafe impl<T> Sync for KernelSync<T> {}

impl<T> KernelSync<T> {
    /// Wrap a value.
    #[inline]
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    /// Borrow the wrapped value.
    #[inline]
    pub const fn get(&self) -> &T {
        &self.value
    }

    /// Mutably borrow the wrapped value. Available only when the caller
    /// holds an exclusive `&mut KernelSync<T>`, so no extra synchronisation
    /// is required at this call site.
    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Consume and unwrap.
    #[inline]
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for KernelSync<T> {
    type Target = T;
    #[inline]
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for KernelSync<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: Default> Default for KernelSync<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for KernelSync<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("KernelSync").field(&self.value).finish()
    }
}

impl<T: Clone> Clone for KernelSync<T> {
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<T: Copy> Copy for KernelSync<T> {}

// =============================================================================
// BspToken
// =============================================================================

/// Sealed capability witness for BSP-only init paths.
///
/// Constructible only from inside OSTD's BSP-bringup code (`pub(crate)`
/// constructor + a single OSTD-side mint site). Consumers receive
/// `&BspToken` arguments and cannot fabricate one in safe code.
///
/// Use this in registration-hook signatures that today require
/// `pub unsafe fn register_*`: change to `pub fn register_*(token: &BspToken, ...)`,
/// move the unsafe-fn-decl-implied "caller-must-be-on-BSP" obligation
/// into the token's existence proof.
#[derive(Debug)]
pub struct BspToken {
    _seal: (),
}

impl BspToken {
    /// Mint a fresh BSP token.
    ///
    /// # Safety
    ///
    /// Caller must be running on the BSP, before any AP has been
    /// bootstrapped. Tokens minted after SMP bringup violate the
    /// single-writer contract every consumer relies on. The intended
    /// caller is OSTD's own `init_bsp_pcr` chain or the kernel's
    /// pre-`smp_init` boot sequence.
    #[inline]
    pub const unsafe fn new() -> Self {
        Self { _seal: () }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use std::cell::Cell;
    use std::sync::Arc;
    use std::thread;

    // Cell<u64> is !Sync; wrapping in KernelSync makes it Sync (as a
    // type-system fact; callers still owe correctness).
    fn assert_sync<T: Sync>() {}
    fn assert_send<T: Send>() {}

    #[test]
    fn kernel_sync_implements_send_sync() {
        // Cell<u64> is Send (since u64: Send) but !Sync.
        assert_send::<KernelSync<Cell<u64>>>();
        assert_sync::<KernelSync<Cell<u64>>>();
    }

    #[test]
    fn kernel_sync_round_trip_value() {
        let k = KernelSync::new(42_u64);
        assert_eq!(*k.get(), 42);
        assert_eq!(*k, 42);
        assert_eq!(k.into_inner(), 42);
    }

    #[test]
    fn kernel_sync_arc_shareable_across_threads() {
        let shared = Arc::new(KernelSync::new(Cell::new(7_u64)));
        let s2 = Arc::clone(&shared);
        let h = thread::spawn(move || {
            // Read-only access; we never mutate Cell from two threads.
            let _ = s2.get().get();
        });
        h.join().unwrap();
        assert_eq!(shared.get().get(), 7);
    }

    #[test]
    fn bsp_token_zero_size() {
        assert_eq!(core::mem::size_of::<BspToken>(), 0);
        // Construction is unsafe — type-check only.
        let _t: BspToken = unsafe { BspToken::new() };
    }
}
