//! Low-level context switching using Rust naked functions.
//!
//! Sole context-switch implementation in the kernel. Field offsets come from
//! `offset_of!`, so renames in [`super::task::TaskContext`] surface as build
//! errors rather than silent corruption.

use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::mem::{MaybeUninit, offset_of};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::cpu::x86_64::pcr;
use crate::sync::BspToken;
use crate::task::abi::TASK_UNSAFE_STACK_SP_OFFSET;
use crate::task::cell::SwitchWindow;
use crate::task::kernel_task::TaskInner;
use crate::task::task::TaskContext;

/// Low-level register switch between two contexts.
///
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
pub unsafe extern "sysv64" fn switch_registers(prev: *mut TaskContext, next: *const TaskContext) {
    naked_asm!(
        // rdi = prev context pointer (nullable)
        // rsi = next context pointer
        "test rdi, rdi",
        "jz 2f",

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

        "2:",
        "mov rbx, [rsi + {off_rbx}]",
        "mov r12, [rsi + {off_r12}]",
        "mov r13, [rsi + {off_r13}]",
        "mov r14, [rsi + {off_r14}]",
        "mov r15, [rsi + {off_r15}]",
        "mov rbp, [rsi + {off_rbp}]",

        "mov rsp, [rsi + {off_rsp}]",

        // RFLAGS is restored with IF cleared; callers re-enable interrupts after
        // the switch returns. A pending IPI between popfq and ret would see the
        // per-CPU current_task naming the dispatched task while RSP is still the
        // idle stack.
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
/// `preempt_count` is logically owned by the task but cached in the per-CPU
/// PCR (see [`TaskContext`]). This is the *only* switch entry point the
/// scheduler should use: it brackets the raw [`switch_registers`] swap with a
/// save of the live count into `prev` and a load of `next`'s into the PCR, so a
/// preempt/lock guard's increment and its matching decrement balance against
/// the same logical counter even if the task migrates while the guard is held.
///
/// `prev` may be null for the first switch out of the boot context.
///
/// Has a safe signature so the scheduler crate — which forbids `unsafe` — can
/// call it; the soundness preconditions are caller obligations documented here
/// rather than encoded in the type:
///
/// - Must be called with interrupts disabled so the count swap and the
///   register switch are atomic with respect to this CPU.
/// - The calling CPU must be the sole accessor of both contexts (Inv. 8).
/// - `next` must point to a valid, initialised [`TaskContext`]; `prev`
///   must be null or point to a valid, exclusively-owned context.
pub fn switch_context(prev: *mut TaskContext, next: *const TaskContext) {
    // Always-on: an IRQ interleaving the count swap and the register switch
    // corrupts the per-task preempt_count, and only surfaces frames later as an
    // unrelated over/underflow panic.
    assert!(
        !crate::cpu::x86_64::interrupts::are_interrupts_enabled(),
        "switch_context called with interrupts enabled"
    );
    // Single-instruction gs-relative accesses keep every preempt-count touch
    // migration-atomic by construction rather than by the assert above.
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

    // SAFETY: `switch_registers` has this function's own documented
    // preconditions — IRQs off, sole accessor, both contexts valid — forwarded
    // by the caller.
    unsafe { switch_registers(prev, next) };
}

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
/// that authorise writing another task's register-adjacent state.
///
/// # Memory ordering
///
/// Each `Release` store orders only what precedes it, so neither covers the
/// `saved_kernel_return_ctx` copy that follows it. What orders both slots
/// across a migration is the dispatch handshake either side of this call:
/// `dispatch`'s `set_current_task` and the switch tail's `Release` store on
/// `on_cpu`, against the incoming dispatcher's `Acquire` loads. Do not tighten
/// or drop that handshake on the strength of the `Release`s here.
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
    // SAFETY: this runs on a CPU whose GS_BASE already names its PCR and whose
    // `PCR.ist_unsafe_sp` is primed — both established by `boot/limine_entry.s`
    // (BSP) / `ap_entry` (AP) before any instrumented Rust runs.
    unsafe {
        crate::arch::x86_64::naked::__safestack_pointer_address()
            .cast::<u8>()
            .cast_const()
    }
}

/// Open the switch window over both endpoints of a context switch, publish the
/// incoming task through `publish`, and lend the window to `prepare`.
///
/// [`SwitchWindow::new`] is `unsafe` and the scheduler crate is
/// `#![forbid(unsafe_code)]`, so OSTD proves the precondition once, here. This
/// is the *only* place a `SwitchWindow` is constructed in the kernel.
///
/// `prev` is `None` for the first switch out of the boot context.
///
/// # Every borrow the switch needs is established before this call
///
/// `publish` and `prepare` capture already-formed references. Neither may mint
/// a guard, form a new task borrow, or create any other address-taken local:
/// `prepare` straddles the register switch, so a frame it opened would hit the
/// data-stack mismatch described below one frame lower. Taking `prev` and
/// `next` as references rather than pointers enforces that for the endpoints.
///
/// `CurrentTask::get` in particular must not be called inside or after
/// `publish`: by then the PCR names the *incoming* task, so the guard would be
/// a correct guard over the wrong task.
///
/// # Why the publication is an argument
///
/// A task runs on two stacks, and they switch at different instants: the
/// *data* stack carrying every address-taken local follows `PCR.current_task`,
/// which the dispatcher republishes, while the *safe* stack (`RSP`) follows
/// [`switch_registers`] several frames later. A frame created between those
/// instants reserves data-stack space from the *incoming* task and releases it
/// from whichever CPU next dispatches the calling task — an unordered write
/// that lays a foreign frame over the owner's live ones.
///
/// This function owns such a frame: both [`SwitchWindow`]s are address-taken
/// and must outlive the switch, so taking the publication as an argument is
/// what keeps that frame on the outgoing task's data stack. The assertion
/// below enforces the ordering rather than trusting each call site.
///
/// The interrupts-off check is always-on and deliberately one frame earlier
/// than the identical assertion in [`switch_context`]: by the time that one
/// runs, `prepare` has already saved the outgoing FPU state and swapped the
/// PCR round-trip slots, so an interleaving IRQ has had its chance at them.
///
/// # Panics
///
/// If interrupts are enabled, or if `next` has already been published as this
/// CPU's current task.
#[inline]
pub fn run_switch<K, U, R>(
    prev: Option<&TaskInner<K, U>>,
    next: &TaskInner<K, U>,
    publish: impl FnOnce(),
    prepare: impl FnOnce(Option<&SwitchWindow<'_, K, U>>, &SwitchWindow<'_, K, U>) -> R,
) -> R
where
    TaskInner<K, U>: crate::task::PcrTaskType,
{
    assert!(
        !crate::cpu::x86_64::interrupts::are_interrupts_enabled(),
        "run_switch called with interrupts enabled"
    );
    // The data stack this frame allocated from must not already be the
    // incoming task's. Always-on: the failure it guards is silent cross-task
    // memory corruption whose symptom surfaces frames or seconds later.
    assert!(
        !core::ptr::eq(
            current_data_stack_slot(),
            core::ptr::from_ref(next)
                .cast::<u8>()
                .wrapping_add(TASK_UNSAFE_STACK_SP_OFFSET),
        ),
        "run_switch entered with the incoming task already published: its \
         SafeStack frame would be released against the wrong task"
    );
    // SAFETY: this CPU is performing the switch with interrupts disabled
    // (asserted above), so the window cannot be re-entered on this CPU. The
    // dispatch-reference half holds for both endpoints by different routes: the
    // ready queue hands the dispatcher its reference on the incoming task, and
    // a CPU's idle task is pinned by `task_is_dispatch_pinned`'s idle disjunct.
    // The publication below adds the current-task disjunct for the incoming
    // endpoint.
    let next_window = unsafe { SwitchWindow::new(next) };

    publish();

    // `on_cpu` is deliberately not asserted: the ready-task dispatch sets it
    // before the switch and the idle switch does not, so the PCR publication is
    // the only fact both paths share.
    debug_assert!(
        core::ptr::eq(
            pcr::get_current_task()
                .cast::<TaskInner<K, U>>()
                .cast_const(),
            core::ptr::from_ref(next),
        ),
        "run_switch: publish did not install the incoming task as current"
    );

    let Some(prev) = prev else {
        return prepare(None, &next_window);
    };
    // SAFETY: as above, for the outgoing endpoint. The dispatcher holds its
    // dispatch reference until the switch tail parks it.
    let prev_window = unsafe { SwitchWindow::new(prev) };
    prepare(Some(&prev_window), &next_window)
}

/// Function the task-entry trampoline calls when the entry point returns.
pub type TaskExitHook = extern "sysv64" fn() -> !;

struct ExitHookSlot(UnsafeCell<MaybeUninit<TaskExitHook>>);
// SAFETY: writes are gated by `EXIT_HOOK_INSTALLED.swap(true, AcqRel)`
// (one-shot); reads happen after the flag is observed Acquire.
unsafe impl Sync for ExitHookSlot {}

static EXIT_HOOK_SLOT: ExitHookSlot = ExitHookSlot(UnsafeCell::new(MaybeUninit::uninit()));
static EXIT_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// One-shot wiring point for the task-exit hook. The `&BspToken<'brand>`
/// witnesses BSP-only init; `hook` is invoked from the trampoline with no
/// arguments and must not return.
pub fn register_task_exit_hook<'brand>(_token: &BspToken<'brand>, hook: TaskExitHook) {
    let was_installed = EXIT_HOOK_INSTALLED.swap(true, Ordering::AcqRel);
    assert!(!was_installed, "register_task_exit_hook called twice");
    // SAFETY: the swap above transitioned us from "uninstalled" to
    // "installed" exclusively; no other writer can be racing.
    unsafe {
        (*EXIT_HOOK_SLOT.0.get()).write(hook);
    }
}

/// The function a user task lands on the first time it is dispatched.
///
/// The scheduler seeds its address as a synthetic return address on the new
/// task's kernel stack, so it needs the address as a value rather than a call.
/// Registration replaces a `#[unsafe(no_mangle)]` symbol that existed only
/// because `core` depends on `sched`, so `sched` cannot name `core`'s items
/// directly.
pub type UserTaskEntry = extern "sysv64" fn() -> !;

static USER_TASK_ENTRY: AtomicUsize = AtomicUsize::new(0);

/// One-shot wiring point for the user-task first-run entry. The
/// `&BspToken<'brand>` witnesses BSP-only init.
pub fn register_user_task_entry<'brand>(_token: &BspToken<'brand>, entry: UserTaskEntry) {
    let addr = entry as usize;
    let previous = USER_TASK_ENTRY.swap(addr, Ordering::AcqRel);
    assert!(
        previous == 0 || previous == addr,
        "register_user_task_entry called twice with different entries",
    );
}

/// Address of the registered user-task first-run entry.
///
/// Panics if no entry has been registered — a user task built before the
/// registration would return into address zero on first dispatch.
pub fn user_task_entry_addr() -> u64 {
    let addr = USER_TASK_ENTRY.load(Ordering::Acquire);
    assert!(
        addr != 0,
        "slopos_ostd::task: user task built with no first-run entry registered",
    );
    addr as u64
}

/// Internal: dispatch to the registered task-exit hook.
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

/// Entry trampoline for new kernel tasks.
///
/// [`TaskContext::new_for_task`] sets `rip` to this trampoline, `r12` to the
/// entry point, and `r13` to the argument.
///
/// # Safety
///
/// Reachable only via [`switch_registers`] dispatching a context built
/// by [`TaskContext::new_for_task`].
#[unsafe(naked)]
pub unsafe extern "sysv64" fn task_entry_trampoline() {
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
