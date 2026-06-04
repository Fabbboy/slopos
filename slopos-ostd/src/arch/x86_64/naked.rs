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
/// It returns the address of the *slot* holding the current SafeStack
/// data-stack pointer; LLVM caches that address on the safe stack across
/// the function's loads/stores.
///
/// **The slot is selected from the running `RSP`, not from the current
/// task.** The data stack a context needs is a property of *which stack is
/// executing*, and the CPU reports that in hardware via `RSP`:
///
/// - `RSP` inside the IST/exception safe-stack region
///   (`[SAFESTACK_IST_REGION_BASE, +SPAN)`) → the code is an exception /
///   IRQ-on-IST / NMI / MCE handler, so return `&PCR.ist_unsafe_sp`, the
///   per-CPU exception data-stack pointer (a dedicated, mapped,
///   guard-paged data stack primed by `ist_stacks`).
/// - otherwise → task / kernel-thread / syscall / IRQ-on-task-stack /
///   early-boot context, so return `&current_task->abi.unsafe_stack_sp`
///   exactly as before.
///
/// This is the *root* fix for "an exception handler writes its
/// address-taken locals (the `[core::fmt::Argument; N]` array a
/// `klog!`/`panic!` builds) onto the interrupted task's small data stack":
/// the IST mechanism switches the *safe* stack (`RSP`) but not the *data*
/// stack, so resolving the data stack from `current_task` routes the
/// handler onto the wrong stack. Deriving it from `RSP` instead makes the
/// selection correct in every context by construction — no per-boundary
/// save/restore, nothing that can drift, and (because `RSP` is re-read
/// every prologue) it is automatically right across context switches,
/// nesting, and SMP migration.
///
/// Naked because the function must avoid self-recursion: a non-naked fn
/// compiled with the sanitizer enabled would itself emit a prologue that
/// calls `__safestack_pointer_address` before returning. It clobbers only
/// `rax` and flags (the pointer-address ABI runs mid-prologue and must not
/// disturb any other register).
///
/// # Safety
///
/// Only called by LLVM-emitted prologues. The asm assumes:
/// - `gs:[SELF_REF]` (offset 0) holds this CPU's PCR base and
///   `gs:[CURRENT_TASK]` a valid `*mut Task` — both set by
///   `boot/limine_entry.s` (BSP) / [`ap_entry`] (AP) before any
///   instrumented Rust runs on that CPU.
/// - `PCR.ist_unsafe_sp` is primed to the exception data-stack top by
///   `ist_stacks` before any IST selector is installed (i.e. before any
///   `RSP` can land in the IST region).
/// - The `Task` struct embeds `abi: TaskAbi` at offset 0 (offset_of! razor
///   in `slopos-core::scheduler::task_struct`).
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "sysv64" fn __safestack_pointer_address() -> *mut *mut u8 {
    naked_asm!(
        // Select the data-stack slot from the running stack (RSP).
        "mov rax, rsp",
        "sub rax, {ist_base}",            // rax = rsp - IST_REGION_BASE
        "cmp rax, {ist_span}",            // unsigned: rsp in [BASE, BASE+SPAN)?
        "jb 2f",                          // yes -> IST/exception context
        // Common path: task / kernel-thread / syscall / IRQ-on-task / boot.
        // Return &current_task->abi.unsafe_stack_sp.
        "mov rax, gs:[{off_current_task}]",
        "add rax, {off_sp}",
        "ret",
        // IST/exception context. Return &PCR.ist_unsafe_sp; PCR base is
        // gs:[self_ref] (offset 0), which equals the gs base itself.
        "2:",
        "mov rax, gs:[{off_self_ref}]",
        "add rax, {off_ist_sp}",
        "ret",
        ist_base = const pcr::SAFESTACK_IST_REGION_BASE,
        ist_span = const pcr::SAFESTACK_IST_REGION_SPAN,
        off_current_task = const pcr_offsets::CURRENT_TASK,
        off_sp = const TASK_UNSAFE_STACK_SP_OFFSET,
        off_self_ref = const pcr_offsets::SELF_REF,
        off_ist_sp = const pcr_offsets::IST_UNSAFE_SP,
    )
}

/// Fatal-fault entry trampoline (Reliable Abort Core): switch BOTH SafeStack
/// stacks to this CPU's emergency stacks, then tail-call the diverging reporter
/// `f`. The safe entry point for the (forbid-unsafe) panic orchestration —
/// calling this naked `extern "sysv64" fn` needs no `unsafe` at the call site.
///
/// Hardware IST switches only `RSP`; the SafeStack DATA stack has no hardware
/// switch, so both are moved here, in one naked entry, before any instrumented
/// frame can run on the (possibly near-overflow) interrupted stacks:
///
/// 1. `RSP` ← `PCR.panic_safe_sp` (the emergency SAFE stack).
/// 2. Because that stack is OUTSIDE the IST region, every subsequent
///    `__safestack_pointer_address` resolves the data-SP to
///    `current_task->unsafe_stack_sp`; we point THAT at `PCR.panic_unsafe_sp`
///    (the emergency DATA stack) with a raw store, so panic `core::fmt` runs
///    with guaranteed headroom on a dedicated guard-paged data stack.
///
/// `f` is `-> !`: there is no return, hence no SafeStack epilogue to undo the
/// data-SP store (the exact hazard that makes a helper-fn or RAII guard wrong
/// here — see `reset_ist_unsafe_sp`). Naked, because an instrumented prologue
/// would touch the suspect data stack before the switch. A null `current_task`
/// is tolerated (the data-SP repoint is skipped). Clobbers rax/rcx/rdx; `f`
/// (rdi) is preserved across the jump.
///
/// # Preconditions (upheld by the panic path)
/// - interrupts disabled,
/// - the per-CPU emergency stacks primed by `ist_stacks` at bringup,
/// - `f` never returns.
#[unsafe(naked)]
pub extern "sysv64" fn run_on_emergency_stacks(f: extern "sysv64" fn() -> !) -> ! {
    naked_asm!(
        "mov rax, gs:[{off_self_ref}]",          // rax = PCR base
        "mov rsp, [rax + {off_panic_safe}]",     // RSP = emergency safe-stack top (16-aligned)
        // SysV requires RSP ≡ 8 (mod 16) at a function's entry (the state a
        // CALL leaves after pushing its 8-byte return address). We tail-JMP, so
        // adjust by 8 manually; without this the reporter's first SSE spill
        // (movaps) lands misaligned and #GPs.
        "sub rsp, 8",
        "mov rcx, gs:[{off_current_task}]",      // rcx = current_task ptr
        "test rcx, rcx",
        "jz 2f",                                 // null task -> skip data-SP repoint
        "mov rdx, [rax + {off_panic_unsafe}]",   // rdx = emergency data-stack top
        "mov [rcx + {off_sp}], rdx",             // current_task->unsafe_stack_sp = emergency top
        "2:",
        "jmp rdi",                               // tail-call f (diverges)
        off_self_ref = const pcr_offsets::SELF_REF,
        off_panic_safe = const pcr_offsets::PANIC_SAFE_SP,
        off_current_task = const pcr_offsets::CURRENT_TASK,
        off_panic_unsafe = const pcr_offsets::PANIC_UNSAFE_SP,
        off_sp = const TASK_UNSAFE_STACK_SP_OFFSET,
    )
}
