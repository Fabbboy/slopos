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

use crate::cpu::x86_64::pcr;
use crate::sync::BspToken;
use crate::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;
use crate::task::cell::SwitchWindow;
use crate::task::kernel_task::TaskInner;
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

/// Context switch with per-task preempt-count save/restore.
///
/// `preempt_count` is logically owned by the task but cached in the
/// per-CPU PCR (see [`TaskContext`]). This is the *only* switch entry
/// point the scheduler should use: it saves the live per-CPU count into
/// the outgoing task's context and loads the incoming task's saved count
/// into the PCR, bracketing the raw [`switch_registers`] register swap.
/// That makes a preempt/lock guard's increment and its matching
/// decrement balance against the same logical counter even if the task
/// migrates to a different CPU while the guard is held.
///
/// `prev` may be null for the first switch out of the boot context.
///
/// Has a safe signature (mirroring [`switch_registers`]) so the scheduler
/// crate — which forbids `unsafe` — can call it; the soundness
/// preconditions are caller obligations documented here rather than
/// encoded in the type:
///
/// - Must be called with interrupts disabled so the count swap and the
///   register switch are atomic with respect to this CPU.
/// - The calling CPU must be the sole accessor of both contexts (Inv. 8).
/// - `next` must point to a valid, initialised [`TaskContext`]; `prev`
///   must be null or point to a valid, exclusively-owned context.
pub fn switch_context(prev: *mut TaskContext, next: *const TaskContext) {
    // Always-on enforcement of the IRQs-off precondition above: a switch
    // with interrupts enabled lets an IRQ interleave with the count swap
    // and the register switch, corrupting the per-task preempt_count in
    // a way that only surfaces frames later as an unrelated
    // over/underflow panic. Fail at the violation, not at the symptom.
    assert!(
        !crate::cpu::x86_64::interrupts::are_interrupts_enabled(),
        "switch_context called with interrupts enabled"
    );
    // Single-instruction gs-relative count accesses: with IRQs off
    // (asserted above) the pointer-based form would also be correct,
    // but the gs ops keep every preempt-count touch in the kernel
    // migration-atomic by construction.
    let live = pcr::preempt_count_get();
    if !prev.is_null() {
        // SAFETY: caller guarantees `prev` is a valid, exclusively-owned
        // context for the duration of the switch.
        unsafe {
            (*prev).preempt_count = live as u64;
        }
    }
    // SAFETY: caller guarantees `next` is a valid context.
    let restored = unsafe { (*next).preempt_count } as u32;
    pcr::preempt_count_set(restored);

    // `switch_registers` has a safe signature (its preconditions match
    // ours and are documented on its own item); forward the swap.
    switch_registers(prev, next);
}

// ---------------------------------------------------------------------------
// run_switch — the sole `SwitchWindow` construction site.
// ---------------------------------------------------------------------------

/// Mirror the per-CPU user-mode round-trip slots across a context switch:
/// save the PCR's onto `prev`, then load `next`'s into the PCR.
///
/// `pcr.user_ctx_ptr` and `pcr.kernel_return_ctx` are written by
/// [`crate::user::mode`]'s round-trip trampoline before `iretq` and read by
/// `__ostd_user_return` on the next user→kernel `SYSCALL`. The slots are
/// per-CPU but the data they carry belongs to the round trip in flight on the
/// *running task*, so a task scheduled in between the two would otherwise send
/// the original task's trampoline to the wrong saved RIP/RSP.
///
/// Takes the switch windows rather than raw pointers: they are the witnesses
/// that authorise writing another task's register-adjacent state, and taking
/// them is what removed the last unwitnessed write into a task cell.
///
/// # Memory ordering
///
/// Four orderings, all preserved from the raw form, and weaker than they look
/// — which is the part worth writing down:
///
/// 1. `pcr.user_ctx_ptr` load (`Acquire`) — a per-CPU slot read by the CPU
///    that wrote it.
/// 2. `prev.saved_user_ctx_ptr` store (`Release`) — this orders writes that
///    *precede* it. The `saved_kernel_return_ctx` copy below is **after** it
///    and therefore **not** ordered by it.
/// 3. `next.saved_user_ctx_ptr` load (`Acquire`).
/// 4. `pcr.user_ctx_ptr` store (`Release`) — same asymmetry, mirrored.
///
/// What actually orders both slots across a migration is the dispatch
/// handshake either side of this call: `dispatch`'s `set_current_task` and the
/// switch tail's `Release` store on `on_cpu`, against the incoming
/// dispatcher's `Acquire` loads. Do not tighten or drop that handshake on the
/// strength of the `Release`s here — they do not cover the copies.
///
/// # Preconditions
///
/// Interrupts disabled, and the caller is the CPU performing the switch — both
/// of which the windows already stand for.
#[inline]
pub fn pcr_round_trip_swap<K, U>(
    prev: Option<&SwitchWindow<'_, K, U>>,
    next: &SwitchWindow<'_, K, U>,
) {
    let pcr = pcr::current_pcr_local();
    let Some(pcr) = pcr else { return };

    if let Some(prev) = prev {
        let task = prev.task();
        task.saved_user_ctx_ptr
            .store(pcr.user_ctx_ptr.load(Ordering::Acquire), Ordering::Release);
        // SAFETY: both pointers address a `KernelReturnContext` that outlives
        // this call — the PCR's is this CPU's own slot and the task's is
        // authorised by the window — and the two are distinct objects, so the
        // copy cannot overlap.
        unsafe {
            core::ptr::copy_nonoverlapping(
                pcr.kernel_return_ctx.get(),
                task.saved_kernel_return_ctx.get_ptr(prev),
                1,
            );
        }
    }

    let task = next.task();
    pcr.user_ctx_ptr.store(
        task.saved_user_ctx_ptr.load(Ordering::Acquire),
        Ordering::Release,
    );
    // SAFETY: as above, with the direction reversed.
    unsafe {
        core::ptr::copy_nonoverlapping(
            task.saved_kernel_return_ctx.get_ptr(next).cast_const(),
            pcr.kernel_return_ctx.get(),
            1,
        );
    }
}

/// Address of the data-stack pointer slot the running context allocates from,
/// resolved exactly as a SafeStack-instrumented prologue resolves it.
#[inline]
fn current_data_stack_slot() -> *const u8 {
    crate::arch::x86_64::naked::__safestack_pointer_address()
        .cast::<u8>()
        .cast_const()
}

/// Open the switch window over both endpoints of a context switch, publish the
/// incoming task through `publish`, and lend the window to `prepare`.
///
/// [`SwitchWindow::new`] is `unsafe` and the scheduler crate is
/// `#![forbid(unsafe_code)]`, so the window cannot be opened there. This is the
/// inversion: OSTD proves the precondition once, here, and hands the scheduler
/// a witness it could not have forged. It is the *only* place a `SwitchWindow`
/// is constructed in the kernel.
///
/// Takes raw pointers rather than references because the scheduler holds
/// `*mut Task`; forming the borrows at the call site would mean another
/// unconstrained-lifetime helper, which is what this migration removes. The
/// borrows live inside, bounded by the closure. `prev` may be null for the
/// first switch out of the boot context. Returns `None` if `next` is null, in
/// which case `publish` never runs.
///
/// # Why the publication is an argument
///
/// A task runs on two stacks. The *safe* stack is `RSP`, swapped atomically by
/// [`switch_registers`]. The *data* stack carries every address-taken local,
/// and which one is in use is decided by `PCR.current_task`: SafeStack's
/// `__safestack_pointer_address` resolves the data-stack pointer slot to
/// `current_task->abi.unsafe_stack_sp` in every instrumented prologue. The two
/// therefore switch at *different instants* — the data stack when the
/// dispatcher republishes the PCR, the safe stack several frames later at the
/// register swap.
///
/// A frame created between those two instants takes its data-stack space from
/// the *incoming* task, and gives it back only when the calling task is
/// dispatched again — from whichever CPU picks that task up, because the
/// cached slot address travels with the frame on the calling task's kernel
/// stack. The reservation is therefore released by a CPU that no longer owns
/// that data stack, with nothing ordering it against the CPU that does: the
/// owner's next instrumented prologue reads a pointer raised back above its
/// live frames and lays a foreign frame over them. The first visible symptom
/// is a half-overwritten pointer somewhere unrelated, so the corruption is
/// found nowhere near where it was caused.
///
/// This function owns such a frame — the two [`SwitchWindow`]s are
/// address-taken, so they live on the data stack, and they must outlive the
/// switch. Taking the publication as an argument is what keeps that frame on
/// the outgoing task's data stack, where its release pairs with its
/// allocation. The assertion below enforces the ordering rather than trusting
/// each call site to keep it.
///
/// The interrupts-off check is an always-on `assert!`, not a `debug_assert!`,
/// and deliberately one frame earlier than the identical assertion in
/// [`switch_context`]. By the time that one runs, `prepare` has already saved
/// the outgoing FPU state and swapped the PCR round-trip slots, so an IRQ that
/// interleaved has had its chance to corrupt them. Catching the violation here
/// catches it before the first write.
///
/// # Panics
///
/// If interrupts are enabled, or if `next` has already been published as this
/// CPU's current task.
#[inline]
pub fn run_switch<K, U, R>(
    prev: *mut TaskInner<K, U>,
    next: *mut TaskInner<K, U>,
    publish: impl FnOnce(),
    prepare: impl FnOnce(Option<&SwitchWindow<'_, K, U>>, &SwitchWindow<'_, K, U>) -> R,
) -> Option<R> {
    assert!(
        !crate::cpu::x86_64::interrupts::are_interrupts_enabled(),
        "run_switch called with interrupts enabled"
    );
    if next.is_null() {
        return None;
    }
    // The data stack this frame allocated from must not already be the
    // incoming task's — see the ordering rationale above. Always-on, because
    // the failure it guards is silent cross-task memory corruption whose
    // symptom surfaces frames or seconds later.
    assert!(
        !core::ptr::eq(
            current_data_stack_slot(),
            next.cast::<u8>()
                .cast_const()
                .wrapping_add(TASK_UNSAFE_STACK_SP_OFFSET),
        ),
        "run_switch entered with the incoming task already published: its \
         SafeStack frame would be released against the wrong task"
    );
    // SAFETY: `next` is non-null and dispatch-pinned by the caller's dispatch
    // reference for the whole call — the ready queue hands that reference to
    // the dispatcher, and a CPU's idle task is pinned by its PCR slot. The
    // publication below adds `task_is_dispatch_pinned`'s second disjunct
    // ("some CPU names this task as its current") on top of it; the reference
    // the caller already holds is what covers the span before that.
    let next_ref = unsafe { &*next };
    // SAFETY: this CPU is performing the switch (it is the one running this
    // code with interrupts disabled, asserted above), it holds both endpoints'
    // dispatch references for the whole call, and the IRQs-off state means the
    // window cannot be re-entered on this CPU.
    let next_window = unsafe { SwitchWindow::new(next_ref) };

    publish();

    // Deliberately NOT asserted: `on_cpu`. The two switch paths differ — the
    // ready-task dispatch sets it before the switch, the idle switch does not
    // — so requiring it here would be a false invariant, and asserting it cost
    // a boot panic to discover. The PCR publication is the fact both paths
    // share.
    debug_assert!(
        core::ptr::eq(
            pcr::get_current_task()
                .cast::<TaskInner<K, U>>()
                .cast_const(),
            next.cast_const(),
        ),
        "run_switch: publish did not install the incoming task as current"
    );

    if prev.is_null() {
        return Some(prepare(None, &next_window));
    }
    // SAFETY: as above, for the outgoing endpoint. The dispatcher holds its
    // dispatch reference until the switch tail parks it.
    let prev_window = unsafe { SwitchWindow::new(&*prev) };
    Some(prepare(Some(&prev_window), &next_window))
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

/// One-shot wiring point for the task-exit hook. The
/// `&BspToken<'brand>` witnesses BSP-only init; `hook` must be a valid
/// function pointer that does not return — it is invoked from the
/// trampoline with no arguments and is expected to perform task
/// termination + reschedule.
pub fn register_task_exit_hook<'brand>(_token: &BspToken<'brand>, hook: TaskExitHook) {
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

        // Enter the task body with interrupts enabled. The context switch
        // restores RFLAGS with IF cleared (the dispatch runs IRQs-off to
        // protect the register/preempt-count swap), so a fresh kernel
        // thread must re-enable them here — a non-blocking poll loop would
        // otherwise run IRQs-off forever, deaf to timer ticks and
        // TLB-shootdown IPIs.
        "sti",

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
