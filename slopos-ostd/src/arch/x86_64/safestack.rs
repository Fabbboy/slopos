//! Safe BSP-init wrappers around the LLVM SafeStack runtime and the
//! AP boot trampoline.
//!
//! LLVM's SafeStack runtime needs no registration — the
//! `__safestack_pointer_address` symbol in [`crate::arch::x86_64::naked`] is
//! `#[unsafe(no_mangle)]`, so LLVM finds it via the linker. The wrappers exist
//! to give consumers a non-naked safe surface, keeping `#[unsafe(naked)]`
//! inside OSTD, and to run the side-effects under the one-shot BSP-init
//! protocol.

use crate::sync::BspToken;

/// AP boot trampoline ABI: a single pointer in `rdi` (x86-64 SysV).
///
/// Layout-equivalent to limine's `MpGotoFunction` (`fn(&MpInfo) -> !`), which
/// the kernel transmutes to in `boot/src/smp.rs::smp_init`. Named here so the
/// `unsafe extern` syntax stays interior to OSTD.
pub type ApTrampolineFn = unsafe extern "C" fn(*const ()) -> !;

mod sealed {
    pub trait Sealed {}
}

/// A fn-pointer type ABI-identical to [`ApTrampolineFn`]: x86-64 SysV, one
/// thin-pointer argument in `rdi`, divergent return.
///
/// The bound [`install_ap_trampoline_as`] needs, in place of a bare `F: Copy`
/// that would admit any pointer-sized `Copy` type: a size assert checks width,
/// only a trait checks *shape*. Sealed, so the two impls below are the whole
/// set.
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
/// A no-op today — LLVM resolves
/// [`super::naked::__safestack_pointer_address`] by symbol, with no runtime
/// registration step. Kept as the hook future side-effects attach to.
///
/// Single-writer: BSP only, witnessed by [`BspToken`].
pub fn install_safestack_runtime<'brand>(_token: &BspToken<'brand>) {
    // No-op today.
}

/// Return the AP boot trampoline's function pointer.
///
/// The trampoline is [`super::naked::ap_entry`], a `#[unsafe(naked)]` fn that
/// installs `IA32_GS_BASE` from `AP_PCR_PTRS[slot-1]` then direct-jumps into
/// the non-naked [`crate::boot::smp::ap_early_entry`]. Kernel boot code
/// transmutes the returned pointer to limine's `MpGotoFunction` — same x86-64
/// SysV ABI, both take `*const ()` in `rdi`.
///
/// Single-writer: BSP only, witnessed by [`BspToken`].
pub fn install_ap_trampoline<'brand>(_token: &BspToken<'brand>) -> ApTrampolineFn {
    super::naked::ap_entry
}

/// Like [`install_ap_trampoline`], but reinterprets the returned pointer as
/// caller-supplied fn-pointer type `F`, so boot code receives it already typed
/// against the bootloader API without spelling `unsafe` outside OSTD.
///
/// The obligation "`F` describes the same ABI" is [`ApTrampolineAbi`], sealed
/// and implemented only here, so it is discharged by whoever wrote the impl
/// rather than by the caller.
pub fn install_ap_trampoline_as<'brand, F: ApTrampolineAbi>(token: &BspToken<'brand>) -> F {
    const {
        assert!(core::mem::size_of::<F>() == core::mem::size_of::<ApTrampolineFn>());
    }
    let f = install_ap_trampoline(token);
    // SAFETY: the const assert guarantees layout-equal fn-pointer sizes, and
    // SysV passes a single pointer arg in rdi regardless of its `T`, so the
    // cast preserves the call ABI.
    unsafe { core::mem::transmute_copy::<ApTrampolineFn, F>(&f) }
}
