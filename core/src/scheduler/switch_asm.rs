//! Low-level context switching using Rust inline assembly with compile-time offsets.
//!
//! This module replaces the external `context_switch.s` assembly file with
//! Rust `naked` functions that use `offset_of!` for struct field access.
//! This eliminates ABI fragility - struct layout changes are caught at compile time.
//!
//! # SafeStack unsafe-stack pointer
//!
//! The SafeStack slot lives inside the `Task` struct
//! (`Task.unsafe_stack_sp`) rather than per-CPU.
//! `__safestack_pointer_address` returns `&current_task->unsafe_stack_sp`,
//! so the slot moves with the task by construction.  Context switch
//! therefore has no SafeStack-specific work to do — updating
//! `PCR.current_task` (which happens before `switch_registers` is
//! called) is what redirects future instrumented prologues to the
//! incoming task's slot.
//!
//! This avoids the class of races inherent to a per-CPU-slot scheme,
//! where a cached slot pointer on the safe stack would go stale
//! across CPU migrations.  See `safestack_rt.rs` for the design notes.

use core::arch::naked_asm;
use core::mem::offset_of;

use super::task_struct::SwitchContext;

/// Low-level register switch between two contexts.
///
/// Saves callee-saved registers to `prev` and loads them from `next`.
/// This is the core context switch primitive - FPU, CR3, and segments
/// are handled by the caller before/after this function.
///
/// # Safety
///
/// - Both contexts must be valid and properly initialized
/// - Must be called with interrupts disabled
/// - Must not be called recursively on the same CPU
/// - Caller must handle FPU state save/restore separately
#[unsafe(naked)]
pub extern "sysv64" fn switch_registers(prev: *mut SwitchContext, next: *const SwitchContext) {
    naked_asm!(
        // rdi = prev context pointer (nullable)
        // rsi = next context pointer
        //
        // The SafeStack unsafe-SP slot lives inside the Task struct
        // (`Task.unsafe_stack_sp`), addressable via
        // `gs:[CURRENT_TASK] + TASK_UNSAFE_STACK_SP_OFFSET` rather than
        // per-CPU.  Switching `PCR.current_task` (done by the caller
        // before invoking this fn) already redirects future
        // instrumented prologues at the new task's slot — there is no
        // per-CPU snapshot to save or restore here.

        // Test if prev is null (first switch from boot)
        "test rdi, rdi",
        "jz 2f",

        // Save callee-saved registers to prev context
        "mov [rdi + {off_rbx}], rbx",
        "mov [rdi + {off_r12}], r12",
        "mov [rdi + {off_r13}], r13",
        "mov [rdi + {off_r14}], r14",
        "mov [rdi + {off_r15}], r15",
        "mov [rdi + {off_rbp}], rbp",
        "mov [rdi + {off_rsp}], rsp",

        // Save RFLAGS via stack
        "pushfq",
        "pop QWORD PTR [rdi + {off_rflags}]",

        // Save return address as RIP (for debugging/new task setup)
        "mov rax, [rsp]",
        "mov [rdi + {off_rip}], rax",

        // Load callee-saved registers from next context
        "2:",
        "mov rbx, [rsi + {off_rbx}]",
        "mov r12, [rsi + {off_r12}]",
        "mov r13, [rsi + {off_r13}]",
        "mov r14, [rsi + {off_r14}]",
        "mov r15, [rsi + {off_r15}]",
        "mov rbp, [rsi + {off_rbp}]",

        // Switch stack and push return address BEFORE restoring RFLAGS.
        "mov rsp, [rsi + {off_rsp}]",

        // Restore RFLAGS with IF cleared — callers re-enable interrupts
        // explicitly after the switch returns.  See context_switch.s for
        // the full rationale: a pending IPI between popfq and ret would
        // see current_task pointing at the dispatched task while RSP is
        // still the idle stack, corrupting the dispatched task's context.
        "mov rax, [rsi + {off_rflags}]",
        "and rax, ~0x200",  // clear IF (bit 9)
        "push rax",
        "popfq",
        "ret",

        off_rbx = const offset_of!(SwitchContext, rbx),
        off_r12 = const offset_of!(SwitchContext, r12),
        off_r13 = const offset_of!(SwitchContext, r13),
        off_r14 = const offset_of!(SwitchContext, r14),
        off_r15 = const offset_of!(SwitchContext, r15),
        off_rbp = const offset_of!(SwitchContext, rbp),
        off_rsp = const offset_of!(SwitchContext, rsp),
        off_rflags = const offset_of!(SwitchContext, rflags),
        off_rip = const offset_of!(SwitchContext, rip),
    );
}

/// Entry trampoline for new kernel tasks.
///
/// When a new task is created, its stack is set up to "return" to this function.
/// The task's entry point is in r12, argument in r13 (set by SwitchContext::new_for_task).
#[unsafe(naked)]
pub extern "sysv64" fn task_entry_trampoline() {
    naked_asm!(
        // r12 = entry point function pointer (set by SwitchContext::new_for_task)
        // r13 = argument to pass (set by SwitchContext::new_for_task)

        // Move argument to first parameter register (rdi)
        "mov rdi, r13",

        // Call the task entry function
        "call r12",

        // If entry returns, call task exit handler
        "call {task_exit}",

        // Should never reach here
        "ud2",

        task_exit = sym super::scheduler::scheduler_task_exit_impl,
    );
}

/// Initialize context from current CPU state (for boot/kernel context).
///
/// Captures the current callee-saved registers so we can switch back to
/// this context later (e.g., return to kernel main after scheduler stops).
#[unsafe(naked)]
pub extern "sysv64" fn init_current_context(ctx: *mut SwitchContext) {
    naked_asm!(
        // rdi = context pointer

        // Save current callee-saved registers
        "mov [rdi + {off_rbx}], rbx",
        "mov [rdi + {off_r12}], r12",
        "mov [rdi + {off_r13}], r13",
        "mov [rdi + {off_r14}], r14",
        "mov [rdi + {off_r15}], r15",
        "mov [rdi + {off_rbp}], rbp",
        "mov [rdi + {off_rsp}], rsp",

        // Save RFLAGS
        "pushfq",
        "pop QWORD PTR [rdi + {off_rflags}]",

        // Save return address as RIP
        "mov rax, [rsp]",
        "mov [rdi + {off_rip}], rax",

        "ret",

        off_rbx = const offset_of!(SwitchContext, rbx),
        off_r12 = const offset_of!(SwitchContext, r12),
        off_r13 = const offset_of!(SwitchContext, r13),
        off_r14 = const offset_of!(SwitchContext, r14),
        off_r15 = const offset_of!(SwitchContext, r15),
        off_rbp = const offset_of!(SwitchContext, rbp),
        off_rsp = const offset_of!(SwitchContext, rsp),
        off_rflags = const offset_of!(SwitchContext, rflags),
        off_rip = const offset_of!(SwitchContext, rip),
    );
}

/// Save the current CPU's FPU/SIMD state to the given task's FpuState buffer.
///
/// Uses XSAVE64 with the active XCR0 mask.  The buffer must be 64-byte
/// aligned (guaranteed by `#[repr(C, align(64))]` on `FpuState`).
///
/// # Safety
/// - `fpu_state` must point to a valid, 64-byte-aligned `FpuState`.
/// - Must be called with interrupts disabled (the caller holds the
///   scheduler lock or is in a cli section).
#[inline]
pub unsafe fn fpu_save(fpu_state: *mut super::task_struct::FpuState) {
    let xcr0 = slopos_arch::cpu::xsave::active_xcr0();
    let lo = xcr0 as u32;
    let hi = (xcr0 >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "xsave64 [{}]",
            in(reg) fpu_state,
            in("eax") lo,
            in("edx") hi,
            // xsave clobbers memory at the destination
            options(nostack),
        );
    }
}

/// Restore FPU/SIMD state from the given task's FpuState buffer into the CPU.
///
/// Uses XRSTOR64 with the active XCR0 mask.
///
/// # Safety
/// Same requirements as `fpu_save`.
#[inline]
pub unsafe fn fpu_restore(fpu_state: *const super::task_struct::FpuState) {
    let xcr0 = slopos_arch::cpu::xsave::active_xcr0();
    let lo = xcr0 as u32;
    let hi = (xcr0 >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "xrstor64 [{}]",
            in(reg) fpu_state,
            in("eax") lo,
            in("edx") hi,
            clobber_abi("sysv64"),
            options(nostack, readonly),
        );
    }
}
