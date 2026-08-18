//! Kernel-only `Send`/`Sync` newtype + BSP/AP-init capability witnesses.
//!
//! [`KernelSync<T>`] centralises the kernel's `unsafe impl Send`/`Sync`:
//! wrapping is sound only where access is mediated by an outer lock, by
//! Inv. 8 (single-CPU task ownership), or by BSP-only-after-init read-only
//! data. Wrap the offending field, not the parent struct.
//!
//! [`BspToken`] / [`ApToken`] are sealed capability witnesses whose
//! invariant `'brand` is minted only inside [`run_bsp_init`] /
//! [`run_ap_init`]'s HRTB closure, so a witness cannot escape it and the
//! "caller must be on the BSP" obligation lives in the type system.

use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use crate::sync::init_flag::InitFlag;

/// Kernel-only `Send + Sync` wrapper; the module docs carry the soundness
/// contract every consumer must satisfy.
#[repr(transparent)]
pub struct KernelSync<T> {
    value: T,
}

// SAFETY: the wrapper's contract — kernel-only access, mediated by an outer
// lock, by Inv. 8 (single-CPU task ownership), or by BSP-only-after-init
// read-only data — is discharged at each `KernelSync::new` site, which cites
// the invariant it relies on.
unsafe impl<T> Send for KernelSync<T> {}
// SAFETY: see Send impl above; the same contract applies.
unsafe impl<T> Sync for KernelSync<T> {}

impl<T> KernelSync<T> {
    #[inline]
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    #[inline]
    pub const fn get(&self) -> &T {
        &self.value
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.value
    }

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

/// Sealed capability witness for BSP-only init paths, carrying an
/// invariant phantom lifetime `'brand`.
///
/// Construction is `pub(crate)` *and* gated by lifetime: nothing outside
/// [`run_bsp_init`]'s HRTB closure has syntax to name the Skolem `'brand`,
/// and the `fn(&'brand ()) -> &'brand ()` phantom makes it invariant, so a
/// `BspToken<'long>` cannot reborrow as `BspToken<'short>`.
#[derive(Copy, Clone)]
pub struct BspToken<'brand> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: PhantomData<*mut ()>,
}

const _: () = assert!(core::mem::size_of::<BspToken<'static>>() == 0);

impl<'brand> core::fmt::Debug for BspToken<'brand> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BspToken<'_>")
    }
}

impl<'brand> BspToken<'brand> {
    /// Reconstruct an owned `BspToken<'brand>` from a borrowed witness of
    /// the same brand: the token is a sealed ZST whose only state is that
    /// brand, so there is nothing to forge and no `unsafe` is needed by
    /// capability-passing layers such as `slopos_hermetic`'s `BootCtx`.
    #[inline]
    pub const fn from_witness(_w: &BspToken<'brand>) -> Self {
        Self {
            _brand: PhantomData,
            _not_send: PhantomData,
        }
    }
}

/// Sealed capability witness for per-AP init paths; same brand discipline
/// as [`BspToken`], with [`run_ap_init`]'s closure body the only mint site.
/// Carries the AP's 1-based slot index, exposed via
/// [`CpuInitWitness::cpu_id`].
pub struct ApToken<'brand> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: PhantomData<*mut ()>,
    cpu_id: usize,
}

const _: () = assert!(core::mem::size_of::<ApToken<'static>>() == core::mem::size_of::<usize>());

impl<'brand> core::fmt::Debug for ApToken<'brand> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApToken")
            .field("cpu_id", &self.cpu_id)
            .finish()
    }
}

mod witness_seal {
    pub trait Sealed {}
}

/// Common capability witness for per-CPU init routines that run once on
/// the BSP and once on each AP. The private `witness_seal::Sealed`
/// supertrait keeps the impls to exactly [`BspToken`] and [`ApToken`].
pub trait CpuInitWitness: witness_seal::Sealed {
    /// CPU slot this witness authorises. BSP returns 0.
    fn cpu_id(&self) -> usize;
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

static BSP_TOKEN_MINTED: InitFlag = InitFlag::new();

/// Hard upper bound on per-AP mint slots; matches `MAX_CPUS` in
/// `boot/src/smp.rs`. `task::bootstrap::MAX_STATIC_APS` (16) is the soft
/// cap actually exercised at boot.
pub const MAX_APS: usize = 256;

/// Per-AP one-shot guard indexed by `cpu_id`; slot 0 is unused (the BSP
/// guard is `BSP_TOKEN_MINTED`).
static AP_TOKEN_MINTED: [InitFlag; MAX_APS] = [const { InitFlag::new() }; MAX_APS];

/// Enter the BSP-init phase: mint a [`BspToken`] bound to a fresh
/// Skolem `'brand`, pass it to `f`, return `f`'s result.
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

/// Test-only: reset the BSP-init guard, then enter [`run_bsp_init`].
///
/// The reset + mint pair is not atomic, so lib-test callers must hold
/// [`crate::test_support::global_lock::lock_global_test_state`]. This
/// helper does not take it itself: it is called inside `isolate()` bodies
/// that already hold the non-reentrant lock.
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

    /// Serialises the token tests: the mint guards are process-global
    /// one-shots shared with other lib-test modules, so reset + mint must
    /// not interleave across test threads.
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
