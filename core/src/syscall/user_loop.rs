//! Kernel-side wrapper that drives the OSTD `UserMode::execute()`
//! round-trip on every user task.
//!
//! [`user_task_first_run`] is the function the scheduler dispatches
//! into for any new (or forked / cloned) user task; it then calls
//! [`user_task_loop`], which loops on `UserMode::execute()` and
//! dispatches each user→kernel return straight into
//! [`crate::syscall::dispatch::syscall_handle`] with a
//! `*mut UserContext` carrier — handlers take the OSTD context
//! directly, no synthetic-frame adapter required.
//!
//! The address-space passed to `UserMode::new` is a single global
//! placeholder: CR3 is still controlled by the legacy paging code
//! outside OSTD, so the `&VmSpace` argument is never functionally
//! activated.  Per-process `VmSpace`s will replace the placeholder
//! once the OSTD paging migration lands.
//!
//! Per the OSTD `UserModeBackend` contract, the trampoline writes the
//! captured user state back through `pcr.user_ctx_ptr`, which we
//! point at the per-task [`UserContext`] field on the `Task` struct.
//!
//! # Stack discipline
//!
//! [`user_task_loop`]'s safe-stack frame is *load-bearing across
//! user-mode entries*: every iteration of its loop assumes its locals
//! survive the iretq → user → SYSCALL round trip.  But `TSS.RSP0`
//! points at the per-task kernel-stack top, so IRQ pushes from user
//! mode land on the same stack.  To keep the supervisor's frame from
//! being clobbered, the task-creation path
//! ([`crate::scheduler::task::task_lifecycle::build_user_task_entry_frame`])
//! seeds [`crate::scheduler::task_struct::SwitchContext::rsp`] far
//! below `kernel_stack_top` (`SUPERVISOR_RESERVE` bytes lower).  The
//! top region is reserved for IRQ pushes and the IRQ-handler chain;
//! the supervisor lives in the bottom region.  See
//! `task_lifecycle::SUPERVISOR_RESERVE` for the full rationale and the
//! IRQ vs. supervisor budget sizing.

use slopos_arch::InterruptFrame;
use slopos_ostd::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::sync::once_lock::OnceLock;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::user::mode::{ReturnReason, UserMode};

// The legacy `slopos_arch::InterruptFrame` import remains for
// `init_user_ctx_from_parent_frame`, which is still consumed by
// `task_clone`'s legacy-frame synthesis path. The syscall-dispatch
// flow no longer touches `InterruptFrame`.

use crate::scheduler::scheduler::scheduler_get_current_task;
use crate::scheduler::task_struct::Task;

/// Single shared `VmSpace` handle used by every user task.
///
/// The trusted-side `UserModeBackend` activates the supplied
/// `&VmSpace` only as a no-op for now (CR3 stays under legacy-kernel
/// control), so a single shared handle is sufficient.  Each user task
/// will adopt its own `KArc<VmSpace>` once the per-process address-
/// space migration into OSTD lands.
static GLOBAL_VM_SPACE: OnceLock<KArc<VmSpace>> = OnceLock::new();

fn placeholder_vm_space() -> &'static KArc<VmSpace> {
    GLOBAL_VM_SPACE.call_once(|| {
        let space = VmSpace::new()
            .expect("placeholder_vm_space: VmSpace::new failed at first user task entry");
        KArc::try_new(space).expect("placeholder_vm_space: KArc allocation failed")
    });
    GLOBAL_VM_SPACE
        .get()
        .expect("placeholder_vm_space: OnceLock not populated after call_once")
}

/// Entry point used by the scheduler for new user tasks.  The kernel
/// stack is set up so `switch_registers` rets here on first dispatch;
/// see [`crate::scheduler::task::task_lifecycle::build_user_task_entry_frame`].
///
/// `extern "C"` because the address is taken and stored as a `u64` on
/// the kernel stack as a synthetic return address.
#[unsafe(no_mangle)]
pub extern "C" fn user_task_first_run() -> ! {
    let task = scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        panic!("user_task_first_run: scheduler_get_current_task returned null");
    }
    user_task_loop(task)
}

fn user_task_loop(task: *mut Task) -> ! {
    let space = placeholder_vm_space();
    loop {
        // `task` is the currently running task; nothing else mutates
        // `task->user_ctx` while it is the running task. The
        // `task_user_ctx_mut` accessor null-checks `task` once and
        // hands back a `&mut UserContext` whose lifetime is the loop
        // iteration scope.
        let ctx_ref = crate::scheduler::task::task_user_ctx_mut(task)
            .expect("user_task_loop: scheduler dispatched null task");
        let ctx_ptr: *mut UserContext = ctx_ref as *mut UserContext;

        let reason = {
            let user_mode = UserMode::new(ctx_ref, space);
            user_mode.execute()
        };

        match reason {
            ReturnReason::Syscall(_n) => {
                // Hand the per-task UserContext straight to the syscall
                // dispatcher; the handler signature now takes
                // `*mut UserContext`, no adapter required.
                crate::syscall::dispatch::syscall_handle(ctx_ptr);
            }
            ReturnReason::Exception(info) => {
                // Only the SYSCALL path is currently routed through
                // `__ostd_user_return`; exception vectors land in the
                // legacy IDT dispatch and never produce a
                // `ReturnReason::Exception` here.  Reaching this branch
                // means a future migration re-pointed an exception
                // vector at the OSTD trampoline without wiring the
                // kernel-side handler.
                panic!(
                    "user_task_loop: unexpected ReturnReason::Exception(vector={}, error={:#x}, fault={:#x}); \
                     only SYSCALL is wired through the OSTD round-trip",
                    info.vector, info.error_code, info.fault_addr,
                );
            }
            ReturnReason::Interrupt(vec) => {
                panic!(
                    "user_task_loop: unexpected ReturnReason::Interrupt(vec={:#x}); \
                     only SYSCALL is wired through the OSTD round-trip",
                    vec
                );
            }
        }
    }
}

/// Seed a freshly-created user task's [`UserContext`] from
/// (entry_point, stack_pointer, entry_arg) the legacy task-create
/// path used to encode in a synthetic `InterruptFrame`.
pub(crate) fn init_user_ctx_for_new_task(
    ctx: &mut UserContext,
    entry_point: u64,
    stack_pointer: u64,
    entry_arg: u64,
) {
    use slopos_ostd::user::context::UserRegs;
    let mut regs = UserRegs::default();
    regs.rip = entry_point;
    regs.rsp = stack_pointer;
    regs.rdi = entry_arg;
    regs.rflags_user_subset = 0x202;
    ctx.set_regs(regs);
}

/// Seed a forked / cloned child's [`UserContext`] from the parent's
/// syscall-time `InterruptFrame`.  Caller guarantees `frame` is the
/// parent's frame at SYSCALL exit.  `force_rax` is the value to
/// install in the child's RAX (typically 0 for fork's child return).
pub(crate) fn init_user_ctx_from_parent_frame(
    ctx: &mut UserContext,
    frame: &InterruptFrame,
    force_rax: u64,
) {
    use slopos_ostd::user::context::UserRegs;
    let mut regs = UserRegs::default();
    regs.r15 = frame.r15;
    regs.r14 = frame.r14;
    regs.r13 = frame.r13;
    regs.r12 = frame.r12;
    regs.r11 = frame.r11;
    regs.r10 = frame.r10;
    regs.r9 = frame.r9;
    regs.r8 = frame.r8;
    regs.rbp = frame.rbp;
    regs.rdi = frame.rdi;
    regs.rsi = frame.rsi;
    regs.rdx = frame.rdx;
    regs.rcx = frame.rcx;
    regs.rbx = frame.rbx;
    regs.rax = force_rax;
    regs.rip = frame.rip;
    regs.rsp = frame.rsp;
    regs.rflags_user_subset = frame.rflags;
    ctx.set_regs(regs);
}
