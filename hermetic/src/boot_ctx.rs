//! `BootCtx<'brand, K>` — capability token gating boot-time-only kernel
//! singleton mutators (`gdt_init`, `gdt_set_ist`, `idt_set_ist`,
//! `syscall_msr_init`, `init_scheduler`, …); calling those after
//! `enter_scheduler` is always a bug.
//!
//! The mint-by-token methods are kind-gated, so an AP-derived
//! `BootCtx<'_, ApInit>` cannot synthesise a `BspToken<'_>` — the AP race
//! window is closed at compile time.
//!
//! `take_for_*` mint a fresh token per call rather than moving one out of a
//! slot, so a test fixture running inside a boot step can hold one while
//! boot holds its own; "one test scope at a time" is carried instead by the
//! `TEST_SCOPE_ACTIVE` flag.

use core::marker::{PhantomData, PhantomPinned};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::{ApToken, BspToken};

mod kind_seal {
    pub trait Sealed {}
    impl Sealed for super::BspInit {}
    impl Sealed for super::ApInit {}
    impl Sealed for super::TestInit {}
}

/// BSP-init kind marker. `BootCtx<'b, BspInit>` is minted by
/// [`take_for_boot`] and authorises BSP-only mutators plus
/// [`BootCtx::bsp_token`] reconstruction.
pub struct BspInit;

/// AP-init kind marker. `BootCtx<'b, ApInit>` is minted by
/// [`take_for_ap`] and authorises AP-only mutators plus
/// [`BootCtx::ap_token`] reconstruction.
pub struct ApInit;

/// Test-init kind marker. `BootCtx<'_, TestInit>` is minted by
/// [`take_for_test`] and authorises CPU mutators but **not** OSTD
/// `register_*` hooks — tests reach those via
/// [`slopos_ostd::sync::run_bsp_init_for_test`].
pub struct TestInit;

/// Marker trait for boot-init kinds. Sealed: `kind_seal::Sealed` is
/// module-private, so external crates cannot add a kind.
pub trait BootKind: kind_seal::Sealed {}
impl BootKind for BspInit {}
impl BootKind for ApInit {}
impl BootKind for TestInit {}

/// Kinds whose `BootCtx` authorises per-CPU mutators (`gdt_set_ist`,
/// `idt_set_ist`, `init_scheduler`, …).
pub trait CpuInitKind: BootKind {}
impl CpuInitKind for BspInit {}
impl CpuInitKind for ApInit {}
impl CpuInitKind for TestInit {}

/// Capability token authorising mutation of boot-time-only kernel
/// singletons.
///
/// `'brand` is invariant and minted by the originating HRTB closure
/// ([`slopos_ostd::sync::run_bsp_init`] or [`slopos_ostd::sync::run_ap_init`]),
/// so it is unnameable outside that scope and cannot be forged; `K` names
/// the init scope. Construction is `pub(crate)`: only this crate's
/// `take_for_*` functions can mint one.
pub struct BootCtx<'brand, K: BootKind> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _kind: PhantomData<K>,
    _consume: PhantomData<PhantomPinned>,
    /// BSP-init witness of the same brand, so `bsp_token` can hand back an
    /// owned token without `unsafe`. `None` for non-`BspInit` kinds;
    /// `take_for_boot` is the only constructor of a `BspInit` BootCtx and
    /// always stores `Some`.
    _token: Option<BspToken<'brand>>,
}

impl<'brand, K: BootKind> BootCtx<'brand, K> {
    pub(crate) const fn new_unchecked(token: Option<BspToken<'brand>>) -> Self {
        Self {
            _brand: PhantomData,
            _kind: PhantomData,
            _consume: PhantomData,
            _token: token,
        }
    }
}

impl<'brand> BootCtx<'brand, BspInit> {
    /// Reconstruct a borrowed `BspToken<'brand>`: the brand was minted by
    /// the originating `run_bsp_init` closure, so this is the same
    /// capability. Boot-step fns whose signature is fixed by the
    /// `boot_init!` linker-section macro have no room for a separate
    /// `&BspToken` parameter and reach OSTD `register_*` hooks through here.
    #[inline]
    pub fn bsp_token(&self) -> BspToken<'brand> {
        self._token
            .expect("BootCtx<'_, BspInit> constructed without a BspToken")
    }
}

// No `BootCtx<'b, ApInit>::ap_token()`: the AP entry path already holds its
// own `&ApToken<'b>` from the enclosing `run_ap_init` closure.

/// Set by `take_for_test`, cleared by `return_after_test`; a second
/// `take_for_test` while it is set panics on the nested `KernelTestScope`.
static TEST_SCOPE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mint the BSP boot path's `BootCtx`. Called once at the start of
/// `kernel_main_impl`, inside the [`slopos_ostd::sync::run_bsp_init`]
/// HRTB closure.
#[inline]
pub fn take_for_boot<'brand>(token: &BspToken<'brand>) -> BootCtx<'brand, BspInit> {
    BootCtx::new_unchecked(Some(*token))
}

/// Consume the `BootCtx` returned by `take_for_boot`; the token drops on
/// return.
#[inline]
pub fn return_after_boot<'brand>(_ctx: BootCtx<'brand, BspInit>) {
    // No shared state to release.
}

/// Mint a `KernelTestScope`'s `BootCtx`. Panics if a previous test scope is
/// still alive. The brand is `'static` because `TestInit` has no
/// `*_token()` mint method, so it cannot escape the capability gate.
pub fn take_for_test() -> BootCtx<'static, TestInit> {
    if TEST_SCOPE_ACTIVE.swap(true, Ordering::AcqRel) {
        panic!("BootCtx::take_for_test: nested KernelTestScope");
    }
    BootCtx::new_unchecked(None)
}

/// Consume the `BootCtx` returned by `take_for_test`, clearing
/// `TEST_SCOPE_ACTIVE` so the next test scope can enter.
pub fn return_after_test(_ctx: BootCtx<'static, TestInit>) {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}

/// Mint an AP boot path's `BootCtx`. Called once per AP during
/// `ap_late_entry`, inside the [`slopos_ostd::sync::run_ap_init`] HRTB
/// closure.
#[inline]
pub fn take_for_ap<'brand>(_token: &ApToken<'brand>) -> BootCtx<'brand, ApInit> {
    BootCtx::new_unchecked(None)
}

/// Consume the `BootCtx` returned by `take_for_ap`.
#[inline]
pub fn return_after_ap<'brand>(_cpu_id: usize, _ctx: BootCtx<'brand, ApInit>) {
    // No shared state to release.
}

/// Force-clear `TEST_SCOPE_ACTIVE` when a panic kept the scope's own `Drop`
/// from reaching it. Intended caller is `panic_recovery`'s registered
/// cleanup chain on the test-running CPU.
pub fn clear_test_scope_after_panic() {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}
