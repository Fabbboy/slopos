//! `BootCtx` — capability token gating boot-time-only kernel mutators.
//!
//! Functions like `gdt_init`, `gdt_set_kernel_rsp0`, `gdt_set_ist`,
//! `idt_set_ist`, `syscall_msr_init`, `init_scheduler`, and
//! `init_task_manager` mutate kernel-singleton state in ways that make
//! sense only during boot or inside a hermetic test scope. After
//! `enter_scheduler`, calling them is always a bug.
//!
//! `BootCtx` is the capability token that grants permission to call
//! them. Mutators take `&mut BootCtx` as their first argument:
//! production code outside the hermetic crate cannot synthesise one
//! (the constructor is `pub(crate)`), so the call doesn't compile.
//!
//! ## Design — mint, not slot
//!
//! Earlier draft used `IrqMutex<Option<BootCtx>>` slots. That breaks the
//! nested-scope use case: while boot is running its init steps, boot
//! owns a `&mut BootCtx`. A test fixture inside `boot_step_run_tests_fn`
//! tries to `take_for_test` and finds the slot empty (boot has it),
//! panicking spuriously.
//!
//! New design: `take_for_boot` / `take_for_test` *mint* a fresh
//! `BootCtx` token each time. The capability is a permission slip, not
//! a unique resource — multiple tokens may exist concurrently as long
//! as their borrowers don't actually contend on the underlying
//! singletons (which the existing per-mutator locking handles).
//!
//! Nested-scope detection moves to a dedicated `TEST_SCOPE_ACTIVE`
//! atomic: `take_for_test` panics if it sees the flag already set.
//! That preserves the "one test scope at a time" invariant without
//! conflating it with boot-phase state.

use core::marker::{PhantomData, PhantomPinned};
use core::sync::atomic::{AtomicBool, Ordering};

/// Capability token authorising mutation of boot-time-only kernel
/// singletons.
///
/// Construction is `pub(crate)`: only this crate's `take_for_*`
/// functions can mint one. Move-only (`!Copy`, `!Clone`) via
/// `PhantomData<PhantomPinned>`.
pub struct BootCtx {
    _consume: PhantomData<PhantomPinned>,
}

impl BootCtx {
    pub(crate) const fn new_unchecked() -> Self {
        Self {
            _consume: PhantomData,
        }
    }
}

/// Set by `take_for_test`, cleared by `return_after_test`. A second
/// `take_for_test` while the flag is set panics — that's a nested
/// `KernelTestScope::enter`.
static TEST_SCOPE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mint a `BootCtx` for the boot path. Called once at the start of
/// `kernel_main_impl`. Conventional naming preserved for readability;
/// no shared state to coordinate.
pub fn take_for_boot() -> BootCtx {
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_boot`. The token drops
/// on return. Conventional naming preserved.
pub fn return_after_boot(_ctx: BootCtx) {
    // No shared state to release.
}

/// Mint a `BootCtx` for a `KernelTestScope`. Panics if a previous
/// test scope is still alive (`TEST_SCOPE_ACTIVE` flag set).
pub fn take_for_test() -> BootCtx {
    if TEST_SCOPE_ACTIVE.swap(true, Ordering::AcqRel) {
        panic!("BootCtx::take_for_test: nested KernelTestScope");
    }
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_test`. Clears the
/// `TEST_SCOPE_ACTIVE` flag so subsequent test scopes can enter.
pub fn return_after_test(_ctx: BootCtx) {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}

/// Mint a `BootCtx` for an AP boot path. Each AP calls this once
/// during `ap_entry_rust`. The `cpu_id` parameter is kept for
/// API symmetry; no per-AP state to coordinate.
pub fn take_for_ap(_cpu_id: usize) -> BootCtx {
    BootCtx::new_unchecked()
}

/// Consume the `BootCtx` returned by `take_for_ap`.
pub fn return_after_ap(_cpu_id: usize, _ctx: BootCtx) {
    // No shared state to release.
}

/// Force-clear the `TEST_SCOPE_ACTIVE` flag from a panic-recovery
/// cleanup callback. Used when a test panics inside its body and the
/// scope's `Drop` is skipped via `catch_panic!`'s longjmp.
///
/// # Safety
/// Only callable from `panic_recovery`'s registered cleanup chain on
/// the test-running CPU.
pub unsafe fn clear_test_scope_after_panic() {
    TEST_SCOPE_ACTIVE.store(false, Ordering::Release);
}
