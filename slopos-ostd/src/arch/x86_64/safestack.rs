//! Safe BSP-init wrappers around the LLVM SafeStack runtime and the
//! AP boot trampoline.
//!
//! Today's LLVM SafeStack runtime needs no runtime registration — the
//! `__safestack_pointer_address` symbol in [`crate::arch::x86_64::naked`]
//! is `#[unsafe(no_mangle)]`, so LLVM finds it via the linker. The
//! wrappers here exist to:
//!
//! 1. Document the BSP-init contract once instead of repeating the
//!    "this symbol is load-bearing, don't rename it" rationale at
//!    every call site.
//! 2. Give consumers a non-naked safe surface so the
//!    `#[unsafe(naked)]` keyword stays inside OSTD per the zero-unsafe
//!    enforcement boundary.
//! 3. Wire in [`BspToken`] so future side-effects (bootstrap-task
//!    SP seeding, optional safestack disable, …) ride the
//!    one-shot BSP-init protocol.

use crate::sync::BspToken;

/// Install the LLVM SafeStack runtime hook.
///
/// Today's implementation is a no-op — LLVM resolves
/// [`super::naked::__safestack_pointer_address`] by symbol, with no
/// runtime registration step. The fn exists to give kernel boot code
/// an explicit hook so that:
///
/// - The presence of the symbol is documented at the kernel call
///   site (consumers stop discovering it accidentally via "what
///   symbol is LLVM emitting calls to?").
/// - Future runtime side-effects (e.g. seeding bootstrap-task SP
///   slots, or an optional `cfg(no_safestack)` no-op fallback) can
///   attach here without churning the kernel-side caller.
///
/// Single-writer: BSP only. The [`BspToken`] argument is the same
/// capability witness used by [`crate::sync::run_bsp_init`] —
/// consumers acquire one inside the one-shot BSP-init pathway.
pub fn install_safestack_runtime(_token: &BspToken) {
    // No-op today. See module docs.
}

/// Return the AP boot trampoline's function pointer.
///
/// The trampoline is [`super::naked::ap_entry`], a `#[unsafe(naked)]`
/// fn that installs `IA32_GS_BASE` from `AP_PCR_PTRS[slot-1]` then
/// direct-jumps into the non-naked
/// [`crate::boot::smp::ap_early_entry`]. Kernel boot code transmutes
/// the returned pointer to limine's `MpGotoFunction` (same x86-64
/// SysV ABI, both take `*const ()` as `rdi`) and passes it to
/// `limine::mp::Cpu::bootstrap`.
///
/// Single-writer: BSP only. The [`BspToken`] argument enforces the
/// one-shot BSP-init protocol; future use-cases (e.g. swapping the
/// trampoline for a different SafeStack-disable variant) can attach
/// side-effects without churning the kernel-side caller.
pub fn install_ap_trampoline(_token: &BspToken) -> unsafe extern "C" fn(*const ()) -> ! {
    super::naked::ap_entry
}
