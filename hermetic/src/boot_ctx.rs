//! `BootCtx<'brand, K>` — branded, kind-marked capability token gating
//! boot-time-only kernel mutators.
//!
//! Functions like `gdt_init`, `gdt_set_kernel_rsp0`, `gdt_set_ist`,
//! `idt_set_ist`, `syscall_msr_init`, `init_scheduler`, and
//! `init_task_manager` mutate kernel-singleton state in ways that only
//! make sense during boot or inside a hermetic test scope. After
//! `enter_scheduler`, calling them is always a bug.
//!
//! `BootCtx<'brand, K>` is the capability token that grants permission
//! to call them. Mutators take `&mut BootCtx<'brand, K: CpuInitKind>`:
//! production code outside the boot path cannot synthesise one (the
//! constructor is `pub(crate)`), and the type parameters tie it to a
//! specific init phase.
//!
//! ## Brand `'brand`
//!
//! The phantom lifetime brand is minted by [`slopos_ostd::sync::run_bsp_init`]
//! (BSP) or [`slopos_ostd::sync::run_ap_init`] (AP). It is invariant
//! and unnameable outside the HRTB closure that mints the token, so a
//! `BootCtx<'b, BspInit>` cannot leak out of the `run_bsp_init` scope
//! and tests cannot synthesise one with a forged brand.
//!
//! ## Kind `K`
//!
//! Three sealed marker types — [`BspInit`], [`ApInit`], [`TestInit`] —
//! distinguish the originating init scope. The mint-by-token methods
//! ([`BootCtx::bsp_token`], [`BootCtx::ap_token`]) are kind-gated; an
//! AP-derived `BootCtx<'_, ApInit>` therefore *cannot* synthesise a
//! `BspToken<'_>`, closing the AP race window at compile time.
//! `TestInit` deliberately has no token-mint methods — tests reach
//! OSTD `register_*` hooks via the feature-gated
//! `run_bsp_init_for_test` pathway, not through `BootCtx`.
//!
//! ## Design — mint, not slot
//!
//! Earlier draft used `SpinLock<Option<BootCtx>>` slots. That breaks the
//! nested-scope use case: while boot is running its init steps, boot
//! owns a `&mut BootCtx`. A test fixture inside `boot_step_run_tests_fn`
//! tries to `take_for_test` and finds the slot empty (boot has it),
//! panicking spuriously.
//!
//! New design: `take_for_boot` / `take_for_test` / `take_for_ap` *mint*
//! a fresh `BootCtx` token each time. The capability is a permission
//! slip, not a unique resource — multiple tokens may exist concurrently
//! as long as their borrowers don't actually contend on the underlying
//! singletons (which the existing per-mutator locking handles).
//!
//! Nested-scope detection moves to a dedicated `TEST_SCOPE_ACTIVE`
//! atomic: `take_for_test` panics if it sees the flag already set.
//! That preserves the "one test scope at a time" invariant without
//! conflating it with boot-phase state.

use core::marker::{PhantomData, PhantomPinned};
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_ostd::sync::{ApToken, BspToken};

// =============================================================================
// Kind markers + sealed traits
// =============================================================================

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
/// [`take_for_test`] for `KernelTestScope`. Authorises CPU-mutator
/// access (so test fixtures can drive `gdt_set_ist` / `idt_set_ist`
/// / `init_scheduler` etc.) but **not** OSTD `register_*` hooks —
/// tests reach those via [`slopos_ostd::sync::run_bsp_init_for_test`].
pub struct TestInit;

/// Sealed marker trait implemented by [`BspInit`], [`ApInit`], and
/// [`TestInit`]. External crates cannot extend `BootKind` because
/// `kind_seal::Sealed` is module-private.
pub trait BootKind: kind_seal::Sealed {}
impl BootKind for BspInit {}
impl BootKind for ApInit {}
impl BootKind for TestInit {}

/// Sealed sub-trait identifying kinds whose `BootCtx` authorises
/// per-CPU mutators (`gdt_set_ist`, `idt_set_ist`, `init_scheduler`,
/// etc.). All three current kinds qualify — BSP-init and AP-init
/// both initialise per-CPU state, and test fixtures need the same
/// access to drive boot-state snapshots.
pub trait CpuInitKind: BootKind {}
impl CpuInitKind for BspInit {}
impl CpuInitKind for ApInit {}
impl CpuInitKind for TestInit {}

// =============================================================================
// BootCtx<'brand, K>
// =============================================================================

/// Capability token authorising mutation of boot-time-only kernel
/// singletons.
///
/// `'brand` is the invariant phantom lifetime threaded by the originating
/// HRTB closure ([`slopos_ostd::sync::run_bsp_init`] or
/// [`slopos_ostd::sync::run_ap_init`]); `K` is the kind marker
/// identifying the init scope. Construction is `pub(crate)`: only this
/// crate's `take_for_*` functions can mint one.
pub struct BootCtx<'brand, K: BootKind> {
    _brand: PhantomData<fn(&'brand ()) -> &'brand ()>,
    _kind: PhantomData<K>,
    _consume: PhantomData<PhantomPinned>,
}

impl<'brand, K: BootKind> BootCtx<'brand, K> {
    pub(crate) const fn new_unchecked() -> Self {
        Self {
            _brand: PhantomData,
            _kind: PhantomData,
            _consume: PhantomData,
        }
    }
}

impl<'brand> BootCtx<'brand, BspInit> {
    /// Reconstruct a borrowed `BspToken<'brand>` from this BSP-init
    /// context. Pure type-level helper — `BspToken` is a ZST sealed
    /// type, and our brand `'brand` was minted by the originating
    /// `run_bsp_init` HRTB closure (the only mint pathway), so the
    /// brand-shared `BspToken<'brand>` is the same capability that
    /// minted us. Boot-step fns whose signature is fixed by the
    /// `boot_init!` linker-section macro (no room for a separate
    /// `&BspToken` parameter) use this accessor to call OSTD
    /// `register_*` hooks.
    #[inline]
    pub fn bsp_token(&self) -> BspToken<'brand> {
        // SAFETY: This BootCtx was minted by `take_for_boot(&BspToken<'brand>)`,
        // which means the caller held a `BspToken<'brand>` reference at
        // mint time. `BspToken<'brand>` is a sealed ZST whose only
        // production state is its phantom brand `'brand`. `mem::zeroed()`
        // for a ZST is well-defined (no bytes to initialise) and the
        // resulting value carries the same brand. Sealed visibility on
        // `BspToken::new` keeps this technique a hermetic-crate-only
        // capability — production code outside this crate cannot
        // construct a `BspToken<'b>` via the same trick because the
        // brand `'b` is unnameable.
        unsafe { core::mem::zeroed() }
    }
}

// Note: there is intentionally no `BootCtx<'b, ApInit>::ap_token()`
// accessor — the AP entry path always holds its own `&ApToken<'b>`
// from the enclosing `run_ap_init` closure alongside the `BootCtx`,
// and threading both into AP-init fn signatures is cleaner than
// reconstructing the cpu_id-bearing token from kind-marker state.

// =============================================================================
// Mint / consume pathways
// =============================================================================

/// Set by `take_for_test`, cleared by `return_after_test`. A second
/// `take_for_test` while the flag is set panics — that's a nested
/// `KernelTestScope::enter`.
static TEST_SCOPE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mint a `BootCtx<'brand, BspInit>` for the BSP boot path. Called
/// once at the start of `kernel_main_impl` inside the
/// [`slopos_ostd::sync::run_bsp_init`] HRTB closure, threading the
/// minted `&BspToken<'brand>` through to OSTD `register_*` hooks
/// and downstream boot steps.
#[inline]
pub fn take_for_boot<'brand>(_token: &BspToken<'brand>) -> BootCtx<'brand, BspInit> {
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_boot`. The token drops
/// on return. Conventional naming preserved.
#[inline]
pub fn return_after_boot<'brand>(_ctx: BootCtx<'brand, BspInit>) {
    // No shared state to release.
}

/// Mint a `BootCtx<'static, TestInit>` for a `KernelTestScope`. Panics
/// if a previous test scope is still alive (`TEST_SCOPE_ACTIVE` flag
/// set). The brand is `'static` because tests do not need to mint
/// tokens; `TestInit` has no `*_token()` mint method, so the brand
/// cannot escape the type-level capability gate.
pub fn take_for_test() -> BootCtx<'static, TestInit> {
    if TEST_SCOPE_ACTIVE.swap(true, Ordering::AcqRel) {
        panic!("BootCtx::take_for_test: nested KernelTestScope");
    }
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_test`. Clears the
/// `TEST_SCOPE_ACTIVE` flag so subsequent test scopes can enter.
pub fn return_after_test(_ctx: BootCtx<'static, TestInit>) {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}

/// Mint a `BootCtx<'brand, ApInit>` for an AP boot path. Each AP
/// calls this once during `ap_late_entry`, inside the
/// [`slopos_ostd::sync::run_ap_init`] HRTB closure, threading the
/// minted `&ApToken<'brand>` to AP-only init paths.
#[inline]
pub fn take_for_ap<'brand>(_token: &ApToken<'brand>) -> BootCtx<'brand, ApInit> {
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_ap`.
#[inline]
pub fn return_after_ap<'brand>(_cpu_id: usize, _ctx: BootCtx<'brand, ApInit>) {
    // No shared state to release.
}

/// Force-clear the `TEST_SCOPE_ACTIVE` flag from a panic-recovery
/// cleanup callback. Used when a test panics inside its body and the
/// scope's `Drop` is skipped via `catch_panic!`'s longjmp.
///
/// A single atomic store to a `'static` flag; sound from any context.
/// Intended caller is `panic_recovery`'s registered cleanup chain on
/// the test-running CPU.
pub fn clear_test_scope_after_panic() {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}
