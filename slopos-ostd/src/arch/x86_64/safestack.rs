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

/// AP boot trampoline ABI: a single pointer in `rdi` (x86-64 SysV).
///
/// Layout-equivalent to limine's `MpGotoFunction` (`fn(&MpInfo) -> !`),
/// which the kernel transmutes to in `boot/src/smp.rs::smp_init`. Named
/// here so callers never have to spell out the raw fn-pointer signature
/// at the transmute site — keeping the `unsafe extern` syntax interior
/// to OSTD.
pub type ApTrampolineFn = unsafe extern "C" fn(*const ()) -> !;

mod sealed {
    pub trait Sealed {}
}

/// A fn-pointer type ABI-identical to [`ApTrampolineFn`]: x86-64 SysV, one
/// thin-pointer argument in `rdi`, divergent return.
///
/// The bound [`install_ap_trampoline_as`] needs, in place of a bare
/// `F: Copy` that would admit any pointer-sized `Copy` type. A size assert
/// checks width; only an implemented-here-and-nowhere-else trait checks
/// *shape*. Sealed, so a consumer crate cannot widen it — the two impls
/// below are the whole set, and each is a signature the SysV ABI makes
/// register-for-register identical to the trampoline's.
///
/// `T` is `Sized` by default, and that is load-bearing: `&T` for an unsized
/// `T` is a fat pointer and would not fit `rdi` alone.
pub trait ApTrampolineAbi: Copy + sealed::Sealed {}

impl sealed::Sealed for ApTrampolineFn {}
impl ApTrampolineAbi for ApTrampolineFn {}

// Covers limine's `MpGotoFunction = unsafe extern "C" fn(&MpInfo) -> !`.
impl<T> sealed::Sealed for unsafe extern "C" fn(&T) -> ! {}
impl<T> ApTrampolineAbi for unsafe extern "C" fn(&T) -> ! {}

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
pub fn install_safestack_runtime<'brand>(_token: &BspToken<'brand>) {
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
pub fn install_ap_trampoline<'brand>(_token: &BspToken<'brand>) -> ApTrampolineFn {
    super::naked::ap_entry
}

/// Like [`install_ap_trampoline`], but reinterprets the returned pointer
/// as caller-supplied fn-pointer type `F`. Lets kernel boot code receive
/// the trampoline already typed against the bootloader API without re-doing
/// the transmute on the caller side (and therefore without spelling
/// `unsafe` outside OSTD).
///
/// The obligation "`F` describes the same ABI" is [`ApTrampolineAbi`],
/// which is sealed and implemented only in this module — so it is
/// discharged by whoever wrote the impl, not by whoever calls this. The
/// size assert stays as belt-and-braces.
pub fn install_ap_trampoline_as<'brand, F: ApTrampolineAbi>(token: &BspToken<'brand>) -> F {
    const {
        assert!(core::mem::size_of::<F>() == core::mem::size_of::<ApTrampolineFn>());
    }
    let f = install_ap_trampoline(token);
    // SAFETY: const assert above guarantees layout-equal fn-pointer
    // sizes; SysV ABI passes a single pointer arg in rdi regardless of
    // the pointer's `T` type, so the cast preserves the call ABI.
    unsafe { core::mem::transmute_copy::<ApTrampolineFn, F>(&f) }
}
