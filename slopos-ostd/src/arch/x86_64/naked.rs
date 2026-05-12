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
use crate::cpu::x86_64::pcr::offsets as pcr_offsets;
use crate::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;

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

/// LLVM SafeStack pointer-address callback.
///
/// LLVM emits a call to this symbol on every instrumented function's
/// prologue under `-Zsanitizer=safestack -C llvm-args=-safestack-use-pointer-address`.
/// The function returns `&current_task->abi.unsafe_stack_sp` — a
/// heap-stable address inside the running task's allocation. LLVM's
/// pointer-address mode caches the returned pointer on the safe stack
/// across multiple loads/stores in a function; embedding the slot
/// inside the Task struct (rather than a per-CPU PCR field) makes
/// the cached pointer survive CPU migration by construction.
///
/// Naked because the function must avoid self-recursion: a non-naked
/// fn compiled with the sanitizer enabled would itself emit a
/// prologue that calls `__safestack_pointer_address` before
/// returning.
///
/// # Safety
///
/// Only called by LLVM-emitted prologues. The asm assumes:
/// - `gs:[pcr_offsets::CURRENT_TASK]` holds a valid `*mut Task` (set
///   by `boot/limine_entry.s` for the BSP and by [`ap_entry`] for
///   APs, before any instrumented Rust runs on that CPU).
/// - The `Task` struct embeds `abi: TaskAbi` at offset 0 (enforced
///   by an `offset_of!` razor inside `slopos-core::scheduler::task_struct`).
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "sysv64" fn __safestack_pointer_address() -> *mut *mut u8 {
    naked_asm!(
        // rax = current_task (AtomicPtr<()> load on x86-64 is a plain mov).
        "mov rax, gs:[{off_current_task}]",
        // rax = &current_task->abi.unsafe_stack_sp
        "add rax, {off_sp}",
        "ret",
        off_current_task = const pcr_offsets::CURRENT_TASK,
        off_sp = const TASK_UNSAFE_STACK_SP_OFFSET,
    )
}
