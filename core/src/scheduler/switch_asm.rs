//! Low-level context switching using Rust inline assembly with compile-time offsets.
//!
//! This module replaces the external `context_switch.s` assembly file with
//! Rust `naked` functions that use `offset_of!` for struct field access.
//! This eliminates ABI fragility - struct layout changes are caught at compile time.

use core::arch::naked_asm;
use core::mem::offset_of;

use slopos_abi::task::SwitchContext;

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
        // rdi = prev context pointer
        // rsi = next context pointer

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

        // Load RFLAGS
        "push QWORD PTR [rsi + {off_rflags}]",
        "popfq",

        // Switch stack (this is the actual context switch point)
        "mov rsp, [rsi + {off_rsp}]",

        // Return (pops return address from new stack)
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
/// The task's entry point is in r12, argument in r13 (set by SwitchContextBuilder).
#[unsafe(naked)]
pub extern "sysv64" fn task_entry_trampoline() {
    naked_asm!(
        // r12 = entry point function pointer (set by SwitchContextBuilder)
        // r13 = argument to pass (set by SwitchContextBuilder)

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
