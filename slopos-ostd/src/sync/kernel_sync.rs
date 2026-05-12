//! Kernel-only `Send`/`Sync` newtype + BSP-init capability witness.
//!
//! # `KernelSync<T>`
//!
//! [`KernelSync<T>`] wraps a value that is *not* automatically `Send +
//! Sync` but is safe to share across CPUs in kernel context because
//! every access is mediated by an outer lock, by the single-CPU
//! task-ownership invariant (Inv. 8 — single-CPU task ownership), or
//! by the BSP-only-after-init invariant for one-shot-initialised
//! globals. Replaces the proliferation of ad-hoc
//! `unsafe impl Send for X {} unsafe impl Sync for X {}` markers across
//! the kernel — the unsafe is centralised here, and consumer crates
//! stay safe.
//!
//! Typical pattern: a struct holds one specific field whose type is
//! `!Send` or `!Sync` (a raw pointer, an `UnsafeCell<T>` over a `!Sync`
//! payload). Wrap **just that field** in [`KernelSync<T>`]; the parent
//! struct then auto-derives `Send + Sync` via field composition. This
//! keeps the unsafe surface scoped to the actual source of unsafety
//! rather than being a struct-wide blanket marker.
//!
//! Consumer crates wrap their offending field/global in
//! `KernelSync<T>`; this file owns the unsafe.
//!
//! # `BspToken` and [`run_bsp_init`]
//!
//! [`BspToken`] is a sealed capability witness. Its constructor is
//! `pub(crate)`, so external crates cannot fabricate one even with
//! `unsafe {}`. The sole public mint pathway is [`run_bsp_init`],
//! which guards against double-mint via a process-global
//! [`InitFlag`] and hands a borrowed `&BspToken` to its callback.
//! Token references therefore exist only for the dynamic extent of the
//! BSP-init callback — statically impossible to obtain after SMP
//! bringup.
//!
//! `pub unsafe fn register_with_ostd()` /
//! `pub unsafe fn register_*` hooks in mm/, kernel-services/, drivers/
//! can switch to `pub fn register_*(token: &BspToken, …)` once the
//! kernel's BSP-init path is routed through `run_bsp_init`.

use core::ops::{Deref, DerefMut};

use crate::sync::init_flag::InitFlag;

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
/// The constructor is `pub(crate)`, so external crates cannot fabricate
/// one even via `unsafe {}`. The only public way to obtain a borrowed
/// `&BspToken` is through [`run_bsp_init`], which guarantees single-shot
/// minting via an [`InitFlag`].
///
/// Use this in registration-hook signatures that today require
/// `pub unsafe fn register_*`: change to
/// `pub fn register_*(token: &BspToken, ...)` — the
/// "caller-must-be-on-BSP" obligation moves from the unsafe-fn decl
/// into the token's existence proof.
#[derive(Debug)]
pub struct BspToken {
    _seal: (),
}

impl BspToken {
    /// Mint a fresh BSP token.
    ///
    /// Visibility is `pub(crate)` so external crates cannot fabricate a
    /// token even by writing `unsafe { BspToken::new() }` — they would
    /// have to forge OSTD-internal code, which the build rejects.
    ///
    /// # Safety
    ///
    /// The internal caller must be on the BSP, before any AP has been
    /// bootstrapped. Tokens minted after SMP bringup violate the
    /// single-writer contract every consumer relies on. The single
    /// production mint site is [`run_bsp_init`].
    #[inline]
    pub(crate) const unsafe fn new() -> Self {
        Self { _seal: () }
    }
}

// ---------------------------------------------------------------------------
// BSP-init mint pathway
// ---------------------------------------------------------------------------

/// Process-global one-shot guard for `run_bsp_init`. Toggled monotonically
/// `false → true` by `InitFlag::init_once()`.
static BSP_TOKEN_MINTED: InitFlag = InitFlag::new();

/// Enter the BSP-init phase: mint a [`BspToken`], pass it to `f`, return
/// `f`'s result.
///
/// # Single-shot
///
/// The first invocation succeeds. Every subsequent invocation **panics**
/// — this matches OSTD's house style for one-shot registries (cf.
/// `register_kernel_master_pml4`, `register_frame_allocator`,
/// `register_io_mem_mapper`, etc.). After the callback returns, the
/// token reference is destroyed; no copy can leak out.
///
/// # Intended use
///
/// The kernel's BSP-init path calls
/// `run_bsp_init(|t| { register_a(t); register_b(t); ... })` to fan the
/// borrowed token to every `register_*(token: &BspToken, ...)` hook.
/// Outside of that boot-time call there is no path to obtain a
/// `&BspToken`.
///
/// # Panics
///
/// Panics if invoked more than once in the lifetime of the process.
#[inline]
pub fn run_bsp_init<R>(f: impl FnOnce(&BspToken) -> R) -> R {
    if !BSP_TOKEN_MINTED.init_once() {
        panic!("run_bsp_init: BSP token already minted; one-shot violated");
    }
    // SAFETY: the InitFlag::init_once swap above succeeded, so we are
    // the unique caller in this process and (by contract) we are on
    // the BSP before any AP has been bootstrapped.
    let token = unsafe { BspToken::new() };
    f(&token)
}

/// Reset the one-shot BSP-token guard so that `run_bsp_init` can be
/// re-entered. **Test-only.** Gated by the `test-helpers` Cargo feature
/// (auto-enabled for `cargo test -p slopos-ostd`) so production code
/// cannot link this symbol.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_bsp_token_for_tests() {
    BSP_TOKEN_MINTED.reset();
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
