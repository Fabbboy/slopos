//! Naked-asm AP boot trampoline.
//!
//! Limine fires each AP directly into [`ap_entry`] with `rdi` pointing
//! at the bootloader-published `MpInfo` whose `extra_argument`
//! (offset 24) the BSP set to the 1-based AP slot. The trampoline
//! installs `IA32_GS_BASE` from [`AP_PCR_PTRS`]`[slot - 1]` then
//! direct-jumps (PC-relative, intra-OSTD) into the non-naked
//! [`ap_early_entry`].
//!
//! Naked because the first instruction of any non-naked safestack-
//! instrumented fn would fetch `gs:[0]` — which is still zero on the
//! AP at this point.
//!
//! Intra-crate jmp target. An earlier attempt jumped through a
//! runtime-stamped `AtomicPtr<()>` to a kernel-side symbol; that
//! relocation pattern caused TCG-only crashes in `utest_fork` /
//! `utest_io_capture`. The pattern adopted here — modelled on
//! Asterinas — always tail-jumps from naked code to a `pub(crate)`
//! Rust symbol in the same crate, then hands off cross-crate via a
//! plain Rust function call after the trampoline returns. See the
//! doc-comment on [`crate::boot::smp::ap_early_entry`] for the full
//! rationale.
//!
//! [`AP_PCR_PTRS`]: crate::cpu::x86_64::pcr::AP_PCR_PTRS

use core::arch::naked_asm;

use crate::boot::smp::ap_early_entry;
use crate::cpu::x86_64::pcr;

/// AP boot trampoline. Pass to `limine::mp::Cpu::bootstrap` as the AP
/// entry symbol (needs an unsafe transmute on the kernel side to
/// `MpGotoFunction`; the two signatures share x86-64 SysV ABI).
///
/// # Safety
///
/// Only called by the bootloader as the AP entry; never invoked
/// directly from Rust. `rdi` must point at a valid `MpInfo` whose
/// `extra_argument` is a 1-based AP slot in the range
/// `1..=MAX_STATIC_APS`, and [`AP_PCR_PTRS`]`[slot - 1]` must already
/// be primed.
///
/// [`AP_PCR_PTRS`]: crate::cpu::x86_64::pcr::AP_PCR_PTRS
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_entry(_cpu_info: *const ()) -> ! {
    naked_asm!(
        // rdi = &MpInfo, preserved across this trampoline.
        //
        // Read 1-based slot from `MpInfo.extra @ +24`, decrement to
        // 0-based, look up PCR pointer in `AP_PCR_PTRS`, write
        // `IA32_GS_BASE`.
        "mov rax, [rdi + {extra_offset}]",
        "dec rax",                              // zero-based slot index
        "lea rcx, [rip + {ap_pcr_ptrs}]",
        "mov rax, [rcx + rax*8]",               // rax = &AP_PCRS[slot-1]
        // WRMSR IA32_GS_BASE = rax. Splits 64-bit value into EDX:EAX
        // (rax already holds low 32 in eax; rdx gets high 32).
        "mov rdx, rax",
        "shr rdx, 32",
        "mov ecx, {ia32_gs_base}",
        "wrmsr",
        // PC-relative direct jmp into the in-OSTD Rust handler.
        // Resolved at link time; no `AtomicPtr` indirection, no
        // cross-crate relocation hazard.
        "jmp {ap_early_entry}",
        extra_offset = const 24,
        ia32_gs_base = const 0xC000_0101_u32,
        ap_pcr_ptrs = sym pcr::AP_PCR_PTRS,
        ap_early_entry = sym ap_early_entry,
    )
}
