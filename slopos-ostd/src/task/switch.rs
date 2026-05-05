//! Low-level context switching using Rust naked functions.
//!
//! Sole context-switch implementation in the kernel.  Uses
//! `offset_of!` for compile-time field offsets, so renames in
//! [`super::task::TaskContext`] surface as build errors rather than
//! silent corruption.
//!
//! # Task exit hook
//!
//! [`task_entry_trampoline`] calls a registered task-exit function
//! after the entry point returns. The hook is registered exactly once
//! at boot via [`register_task_exit_hook`]. Until registered, hitting
//! the trampoline's exit edge produces a kernel panic (the entry was
//! supposed to be `-> !` and never return).

use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::mem::{MaybeUninit, offset_of};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::task::task::TaskContext;

// ---------------------------------------------------------------------------
// switch_registers / init_current_context.
// ---------------------------------------------------------------------------

/// Low-level register switch between two contexts.
///
/// Saves callee-saved registers to `prev` and loads them from `next`.
/// FPU, CR3, and segments are handled by the caller before/after.
///
/// # Safety
///
/// - Both contexts must be valid and properly initialised.
/// - Must be called with interrupts disabled.
/// - Must not be called recursively on the same CPU.
/// - Caller handles FPU state save/restore separately.
/// - Inv. 8 — the calling CPU is the sole accessor of both contexts.
#[unsafe(naked)]
pub extern "sysv64" fn switch_registers(prev: *mut TaskContext, next: *const TaskContext) {
    naked_asm!(
        // rdi = prev context pointer (nullable)
        // rsi = next context pointer

        // Test if prev is null (first switch from boot).
        "test rdi, rdi",
        "jz 2f",

        // Save callee-saved registers to prev context.
        "mov [rdi + {off_rbx}], rbx",
        "mov [rdi + {off_r12}], r12",
        "mov [rdi + {off_r13}], r13",
        "mov [rdi + {off_r14}], r14",
        "mov [rdi + {off_r15}], r15",
        "mov [rdi + {off_rbp}], rbp",
        "mov [rdi + {off_rsp}], rsp",

        // Save RFLAGS via stack.
        "pushfq",
        "pop QWORD PTR [rdi + {off_rflags}]",

        // Save return address as RIP.
        "mov rax, [rsp]",
        "mov [rdi + {off_rip}], rax",

        // Load callee-saved registers from next context.
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
        // explicitly after the switch returns. A pending IPI between
        // popfq and ret would see the per-CPU current_task pointing at
        // the dispatched task while RSP is still the idle stack,
        // corrupting the dispatched task's context.
        "mov rax, [rsi + {off_rflags}]",
        "and rax, ~0x200",  // clear IF (bit 9)
        "push rax",
        "popfq",
        "ret",

        off_rbx = const offset_of!(TaskContext, rbx),
        off_r12 = const offset_of!(TaskContext, r12),
        off_r13 = const offset_of!(TaskContext, r13),
        off_r14 = const offset_of!(TaskContext, r14),
        off_r15 = const offset_of!(TaskContext, r15),
        off_rbp = const offset_of!(TaskContext, rbp),
        off_rsp = const offset_of!(TaskContext, rsp),
        off_rflags = const offset_of!(TaskContext, rflags),
        off_rip = const offset_of!(TaskContext, rip),
    );
}

/// Initialise context from current CPU state (for boot/kernel context).
///
/// Captures the current callee-saved registers so the scheduler can
/// switch back to this context later (e.g., return to kernel main
/// after the scheduler stops).
///
/// # Safety
///
/// `ctx` must point to a writable, properly-aligned [`TaskContext`].
#[unsafe(naked)]
pub extern "sysv64" fn init_current_context(ctx: *mut TaskContext) {
    naked_asm!(
        // rdi = context pointer

        "mov [rdi + {off_rbx}], rbx",
        "mov [rdi + {off_r12}], r12",
        "mov [rdi + {off_r13}], r13",
        "mov [rdi + {off_r14}], r14",
        "mov [rdi + {off_r15}], r15",
        "mov [rdi + {off_rbp}], rbp",
        "mov [rdi + {off_rsp}], rsp",

        "pushfq",
        "pop QWORD PTR [rdi + {off_rflags}]",

        "mov rax, [rsp]",
        "mov [rdi + {off_rip}], rax",

        "ret",

        off_rbx = const offset_of!(TaskContext, rbx),
        off_r12 = const offset_of!(TaskContext, r12),
        off_r13 = const offset_of!(TaskContext, r13),
        off_r14 = const offset_of!(TaskContext, r14),
        off_r15 = const offset_of!(TaskContext, r15),
        off_rbp = const offset_of!(TaskContext, rbp),
        off_rsp = const offset_of!(TaskContext, rsp),
        off_rflags = const offset_of!(TaskContext, rflags),
        off_rip = const offset_of!(TaskContext, rip),
    );
}

// ---------------------------------------------------------------------------
// Task exit hook (one-shot registration).
// ---------------------------------------------------------------------------

/// Function the task-entry trampoline calls when the entry point
/// returns. The kernel scheduler registers a function that performs
/// task termination + reschedule.
pub type TaskExitHook = extern "sysv64" fn() -> !;

struct ExitHookSlot(UnsafeCell<MaybeUninit<TaskExitHook>>);
// SAFETY: writes are gated by `EXIT_HOOK_INSTALLED.swap(true, AcqRel)`
// (one-shot); reads happen after the flag is observed Acquire.
unsafe impl Sync for ExitHookSlot {}

static EXIT_HOOK_SLOT: ExitHookSlot = ExitHookSlot(UnsafeCell::new(MaybeUninit::uninit()));
static EXIT_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the task-exit hook.
///
/// # Safety
///
/// `hook` must be a valid function pointer that does not return. It
/// will be invoked from the trampoline with no arguments and is
/// expected to perform task termination + reschedule.
pub unsafe fn register_task_exit_hook(hook: TaskExitHook) {
    let was_installed = EXIT_HOOK_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_task_exit_hook called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*EXIT_HOOK_SLOT.0.get()).write(hook);
    }
}

/// Internal: dispatch to the registered task-exit hook. Called from
/// the entry trampoline after the entry function returns.
///
/// # Safety
///
/// Must only be called from the entry trampoline after the entry
/// function has returned. Diverges (`!`).
extern "sysv64" fn dispatch_task_exit() -> ! {
    if !EXIT_HOOK_INSTALLED.load(Ordering::Acquire) {
        panic!("slopos_ostd::task: task entry returned with no exit hook registered");
    }
    // SAFETY: paired Release in `register_task_exit_hook`.
    let hook = unsafe { *(*EXIT_HOOK_SLOT.0.get()).as_ptr() };
    hook()
}

// ---------------------------------------------------------------------------
// task_entry_trampoline.
// ---------------------------------------------------------------------------

/// Entry trampoline for new kernel tasks.
///
/// When a new task is created, its [`TaskContext::new_for_task`] sets
/// `rip` to this trampoline, `r12` to the entry point, and `r13` to
/// the argument. On first dispatch, [`switch_registers`] returns into
/// this stub which calls `entry(arg)` and then the registered exit
/// hook.
///
/// # Safety
///
/// Reachable only via [`switch_registers`] dispatching a context built
/// by [`TaskContext::new_for_task`].
#[unsafe(naked)]
pub extern "sysv64" fn task_entry_trampoline() {
    naked_asm!(
        // r12 = entry point function pointer
        // r13 = argument

        // Move argument to first parameter register.
        "mov rdi, r13",

        // Call the task entry function.
        "call r12",

        // If entry returns, dispatch to the exit hook.
        "call {task_exit}",

        // Should never reach here.
        "ud2",

        task_exit = sym dispatch_task_exit,
    );
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_exit_hook_for_test() {
    EXIT_HOOK_INSTALLED.store(false, Ordering::Release);
}
