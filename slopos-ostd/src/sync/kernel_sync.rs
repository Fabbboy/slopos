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
//! BSP-init witnesses in this module: [`BspToken`] and [`ApToken`],
//! each carrying an invariant phantom lifetime `'brand` minted by an
//! HRTB closure (`run_bsp_init` / `run_ap_init`). The brand is unforgeable
//! outside the closure body: `for<'b> FnOnce(&BspToken<'b>) -> R` requires
//! `R` to handle every choice of `'b`, so the token reference cannot
//! escape. The 14+ register/install hooks elsewhere in OSTD adopt
//! `pub fn register_*<'b>(token: &BspToken<'b>, …)` so the
//! "caller-must-be-on-BSP" obligation lives in the type system, not in
//! a `# Safety` doc paragraph.

use core::marker::PhantomData;
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

impl<T> KernelSync<core::cell::UnsafeCell<T>> {
    /// Mutable per-CPU access to a `KernelSync<UnsafeCell<T>>` slot.
    /// The wrapper pattern combines `Sync`-via-`KernelSync` with
    /// interior mutability via `UnsafeCell`; per-CPU IRQs-off
    /// single-writer discipline (held by the caller) is what makes
    /// `&mut T` sound to expose.
    ///
    /// # Safety contract on the caller
    ///
    /// - Interrupts are disabled on the current CPU for the lifetime
    ///   of the returned `&mut T`.
    /// - No other code path will alias `*self.value.get()` on this
    ///   CPU (this is the normal per-CPU storage idiom: only the
    ///   CPU that owns the slot ever touches it, and the slot's
    ///   `cpu_id` index is fixed at compile-time).
    ///
    /// Safe to call: the contract is documented and the only
    /// `unsafe` is the one-line `&mut *get()` reborrow folded here.
    #[inline]
    pub fn cell_get_mut(&self) -> &mut T {
        // SAFETY: caller upholds the per-CPU discipline above; the
        // resulting `&mut T` lifetime is bounded by the call site.
        unsafe { &mut *self.value.get() }
    }

    /// Read-only sibling of [`Self::cell_get_mut`]. Same caller
    /// contract, but the returned `&T` is shareable across `f` so
    /// cross-CPU diagnostic snapshots (read-only) are permitted.
    #[inline]
    pub fn cell_get(&self) -> &T {
        // SAFETY: caller upholds the per-CPU discipline.
        unsafe { &*self.value.get() }
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
// BspToken<'brand> / ApToken<'brand>
// =============================================================================

/// Sealed capability witness for BSP-only init paths, carrying an
/// invariant phantom lifetime `'brand`.
///
/// Construction is `pub(crate)` *and* gated by lifetime: external code
/// has no syntax to name the Skolem `'brand` minted inside
/// [`run_bsp_init`]'s HRTB closure, so even within OSTD nothing can
/// fabricate a `BspToken<'static>` or a `BspToken` at any nameable
/// lifetime — the only `&BspToken<'b>` references in existence live
/// for the dynamic extent of the closure that received them.
///
/// The `fn(&'brand ()) -> &'brand ()` PhantomData is the canonical
/// Rust invariance gadget: arguments are contravariant, returns
/// covariant, so the same lifetime in both positions becomes
/// invariant. A `BspToken<'long>` cannot reborrow as `BspToken<'short>`.
#[derive(Copy, Clone)]
pub struct BspToken<'brand> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: PhantomData<*mut ()>,
}

// The token is a pure capability: a ZST with no runtime cost to pass.
const _: () = assert!(core::mem::size_of::<BspToken<'static>>() == 0);

impl<'brand> core::fmt::Debug for BspToken<'brand> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BspToken<'_>")
    }
}

impl<'brand> BspToken<'brand> {
    /// Reconstruct an owned `BspToken<'brand>` from a borrowed witness
    /// of the same brand. Sound because `BspToken<'brand>` is a sealed
    /// ZST whose only state is its phantom brand — the brand already
    /// matches the witness, and a ZST has no bytes to forge. Safe
    /// surface so capability-passing layers (e.g. `slopos_hermetic`'s
    /// `BootCtx`) can synthesise an owned token without `unsafe`.
    #[inline]
    pub const fn from_witness(_w: &BspToken<'brand>) -> Self {
        Self {
            _brand: PhantomData,
            _not_send: PhantomData,
        }
    }
}

/// Sealed capability witness for per-AP init paths.
///
/// Same brand discipline as [`BspToken`]: the closure body of
/// [`run_ap_init`] is the only mint site, the `'brand` is unforgeable
/// outside it, and APs cannot synthesise a `BspToken<'_>` (different
/// type, different mint pathway, type-checker rejects coercion).
///
/// Carries the AP's 1-based slot index for diagnostic and
/// per-CPU dispatch consumers via [`CpuInitWitness::cpu_id`].
pub struct ApToken<'brand> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: PhantomData<*mut ()>,
    cpu_id: usize,
}

// Capability plus its diagnostic slot index — exactly one word.
const _: () = assert!(core::mem::size_of::<ApToken<'static>>() == core::mem::size_of::<usize>());

impl<'brand> core::fmt::Debug for ApToken<'brand> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApToken")
            .field("cpu_id", &self.cpu_id)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// CpuInitWitness — sealed trait implemented by BspToken + ApToken
// ---------------------------------------------------------------------------

mod witness_seal {
    pub trait Sealed {}
}

/// Common capability witness for per-CPU init routines that run once
/// on the BSP and once on each AP (`install_syscall_msrs`, `idt_load`,
/// `enable_supervisor_features`, the xsave/SSE/PCID enablers,
/// `ist_bind_current_cpu`, …).
///
/// The supertrait `witness_seal::Sealed` is private to this module, so
/// external crates cannot add new impls — only [`BspToken`] and
/// [`ApToken`] satisfy it. Functions taking `<W: CpuInitWitness>`
/// monomorphise into exactly two specialisations.
pub trait CpuInitWitness: witness_seal::Sealed {
    /// CPU slot this witness authorises. BSP returns 0.
    fn cpu_id(&self) -> usize;
    /// `true` iff this is the BSP witness.
    fn is_bsp(&self) -> bool;
}

impl<'brand> witness_seal::Sealed for BspToken<'brand> {}
impl<'brand> witness_seal::Sealed for ApToken<'brand> {}

impl<'brand> CpuInitWitness for BspToken<'brand> {
    #[inline]
    fn cpu_id(&self) -> usize {
        0
    }
    #[inline]
    fn is_bsp(&self) -> bool {
        true
    }
}

impl<'brand> CpuInitWitness for ApToken<'brand> {
    #[inline]
    fn cpu_id(&self) -> usize {
        self.cpu_id
    }
    #[inline]
    fn is_bsp(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Mint pathways
// ---------------------------------------------------------------------------

/// Process-global one-shot guard for `run_bsp_init`.
static BSP_TOKEN_MINTED: InitFlag = InitFlag::new();

/// Hard upper bound on per-AP mint slots. Matches `MAX_CPUS` in
/// `boot/src/smp.rs`; `task::bootstrap::MAX_STATIC_APS` (16) is the
/// soft cap actually exercised at boot.
pub const MAX_APS: usize = 256;

/// Per-AP one-shot guard, indexed by `cpu_id`. Slot 0 is the BSP and
/// is unused (the BSP guard is `BSP_TOKEN_MINTED`).
static AP_TOKEN_MINTED: [InitFlag; MAX_APS] = [const { InitFlag::new() }; MAX_APS];

/// Enter the BSP-init phase: mint a [`BspToken`] bound to a fresh
/// Skolem `'brand`, pass it to `f`, return `f`'s result.
///
/// The HRTB `for<'b> FnOnce(&BspToken<'b>) -> R` makes `'b` a Skolem
/// lifetime — there is no syntax for the closure body to *return* any
/// value mentioning `'b`, and no syntax for the caller to *bind* it.
/// `R` is therefore independent of `'b`; the token reference is
/// destroyed with the closure frame.
///
/// # Single-shot
///
/// First call succeeds; subsequent calls panic. Defense-in-depth: the
/// type system already prevents leaking the prior `&BspToken<'_>` out
/// of its closure, so a second mint cannot collide with a still-live
/// reference. The panic catches well-intentioned but mistaken re-init.
///
/// # Panics
///
/// Panics if invoked more than once in the lifetime of the process.
#[inline]
pub fn run_bsp_init<R, F>(f: F) -> R
where
    F: for<'brand> FnOnce(&BspToken<'brand>) -> R,
{
    if !BSP_TOKEN_MINTED.init_once() {
        panic!("run_bsp_init: BSP token already minted; one-shot violated");
    }
    let token: BspToken<'_> = BspToken {
        _brand: PhantomData,
        _not_send: PhantomData,
    };
    f(&token)
}

/// Enter an AP's init phase: mint an [`ApToken`] bound to a fresh
/// Skolem `'brand`, pass it to `f`, return `f`'s result.
///
/// Same brand discipline as [`run_bsp_init`]. Per-AP one-shot via
/// `AP_TOKEN_MINTED[cpu_id]`.
///
/// # Panics
///
/// - if `cpu_id == 0` or `cpu_id >= MAX_APS`;
/// - if this slot's [`InitFlag`] has already minted.
#[inline]
pub fn run_ap_init<R, F>(cpu_id: usize, f: F) -> R
where
    F: for<'brand> FnOnce(&ApToken<'brand>) -> R,
{
    assert!(
        cpu_id > 0 && cpu_id < MAX_APS,
        "run_ap_init: cpu_id {} out of range (1..{})",
        cpu_id,
        MAX_APS
    );
    if !AP_TOKEN_MINTED[cpu_id].init_once() {
        panic!(
            "run_ap_init: AP {} token already minted; one-shot violated",
            cpu_id
        );
    }
    let token: ApToken<'_> = ApToken {
        _brand: PhantomData,
        _not_send: PhantomData,
        cpu_id,
    };
    f(&token)
}

// ---------------------------------------------------------------------------
// Test mint pathway (feature-gated; never linked in production)
// ---------------------------------------------------------------------------

/// Test-only mint helper. Resets the BSP-init guard, then enters
/// [`run_bsp_init`]. Tests obtain a `&BspToken<'_>` they can pass to
/// OSTD `register_*` hooks. Production builds cannot link this — the
/// `test-helpers` feature is auto-enabled only for `cargo test -p
/// slopos-ostd`.
///
/// The reset + mint pair is not atomic: callers inside the *lib* test
/// binary must hold
/// [`crate::test_support::global_lock::lock_global_test_state`] (the
/// in-tree pattern is the owning module's `isolate()` helper acquiring
/// it). Integration-test binaries serialise themselves per-process and
/// may call this bare. This helper deliberately does not take the lock
/// itself — it is routinely called *inside* `isolate()` bodies that
/// already hold it, and the lock is not reentrant.
#[cfg(any(test, feature = "test-helpers"))]
pub fn run_bsp_init_for_test<R, F>(f: F) -> R
where
    F: for<'brand> FnOnce(&BspToken<'brand>) -> R,
{
    reset_bsp_token_for_tests();
    run_bsp_init(f)
}

/// Test-only mint helper for AP-init witnesses.
#[cfg(any(test, feature = "test-helpers"))]
pub fn run_ap_init_for_test<R, F>(cpu_id: usize, f: F) -> R
where
    F: for<'brand> FnOnce(&ApToken<'brand>) -> R,
{
    reset_ap_token_for_tests(cpu_id);
    run_ap_init(cpu_id, f)
}

/// Reset the one-shot BSP-token guard *and* every per-AP guard so
/// `run_bsp_init` / `run_ap_init` can be re-entered. **Test-only.**
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_bsp_token_for_tests() {
    BSP_TOKEN_MINTED.reset();
    for slot in AP_TOKEN_MINTED.iter() {
        slot.reset();
    }
}

/// Reset a single per-AP guard. **Test-only.**
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_ap_token_for_tests(cpu_id: usize) {
    assert!(cpu_id > 0 && cpu_id < MAX_APS);
    AP_TOKEN_MINTED[cpu_id].reset();
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use std::cell::Cell;
    use std::sync::Arc;
    use std::thread;

    fn assert_sync<T: Sync>() {}
    fn assert_send<T: Send>() {}

    #[test]
    fn kernel_sync_implements_send_sync() {
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
            let _ = s2.get().get();
        });
        h.join().unwrap();
        assert_eq!(shared.get().get(), 7);
    }

    // Token sizes (BspToken ZST, ApToken one word) are pinned by
    // `const _` asserts beside the type definitions; no runtime
    // duplicates here.

    /// Serialises the token tests: the mint guards are process-global
    /// one-shots shared with other lib-test modules (e.g. `irq::line`
    /// minting via `run_bsp_init_for_test`), so reset + mint must not
    /// interleave across test threads. The guard releases on unwind,
    /// covering the `should_panic` tests below.
    fn serial() -> crate::test_support::global_lock::GlobalTestStateGuard {
        let g = crate::test_support::global_lock::lock_global_test_state();
        reset_bsp_token_for_tests();
        g
    }

    #[test]
    fn run_bsp_init_passes_token_to_closure() {
        let _g = serial();
        let r = run_bsp_init(|t| {
            assert_eq!(core::mem::size_of_val(t), 0);
            assert!(t.is_bsp());
            assert_eq!(t.cpu_id(), 0);
            7_u32
        });
        assert_eq!(r, 7);
    }

    #[test]
    fn run_ap_init_carries_cpu_id() {
        let _g = serial();
        let r = run_ap_init(3, |t| {
            assert!(!t.is_bsp());
            assert_eq!(t.cpu_id(), 3);
            9_u32
        });
        assert_eq!(r, 9);
    }

    #[test]
    #[should_panic(expected = "BSP token already minted")]
    fn run_bsp_init_double_call_panics() {
        let _g = serial();
        run_bsp_init(|_| {});
        run_bsp_init(|_| {});
    }

    #[test]
    #[should_panic(expected = "AP 5 token already minted")]
    fn run_ap_init_double_call_panics() {
        let _g = serial();
        run_ap_init(5, |_| {});
        run_ap_init(5, |_| {});
    }

    #[test]
    #[should_panic(expected = "cpu_id 0 out of range")]
    fn run_ap_init_rejects_cpu_zero() {
        let _g = serial();
        run_ap_init(0, |_| {});
    }

    #[test]
    fn cpu_init_witness_dispatch() {
        let _g = serial();
        fn check<W: CpuInitWitness>(w: &W, expected_id: usize, expect_bsp: bool) {
            assert_eq!(w.cpu_id(), expected_id);
            assert_eq!(w.is_bsp(), expect_bsp);
        }
        run_bsp_init(|t| check(t, 0, true));
        run_ap_init(2, |t| check(t, 2, false));
    }
}
