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
//! placeholder.  The scheduler installs the task's own `VmSpace` at
//! dispatch, so the `&VmSpace` argument here is never functionally
//! activated.
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
//! ([`slopos_sched::task::task_lifecycle::build_user_task_entry_frame`])
//! seeds [`slopos_sched::task_struct::SwitchContext::rsp`] far
//! below `kernel_stack_top` (`SUPERVISOR_RESERVE` bytes lower).  The
//! top region is reserved for IRQ pushes and the IRQ-handler chain;
//! the supervisor lives in the bottom region.  See
//! `task_lifecycle::SUPERVISOR_RESERVE` for the full rationale and the
//! IRQ vs. supervisor budget sizing.

use slopos_ostd::KArc;
use slopos_ostd::klog_info;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::panic_recovery;
use slopos_ostd::sync::once_lock::OnceLock;
use slopos_ostd::user::mode::{ReturnReason, UserMode};

use slopos_sched::scheduler::scheduler_task_exit_impl;
use slopos_sched::task_struct::Current;

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

/// Entry point used by the scheduler for new user tasks. The kernel stack
/// is set up so `switch_registers` rets here on first dispatch; see
/// [`slopos_sched::task::task_lifecycle::build_user_task_entry_frame`].
///
/// `extern "sysv64"` because the address is taken and stored as a `u64` on
/// the kernel stack as a synthetic return address.
extern "sysv64" fn user_task_first_run() -> ! {
    user_task_loop()
}

/// Hand the first-run entry to OSTD so the scheduler can seed it without
/// resolving a C symbol. One-shot; the `&BspToken<'brand>` witnesses
/// BSP-only init.
pub fn install_user_task_entry<'b>(token: &slopos_ostd::sync::BspToken<'b>) {
    slopos_ostd::task::register_user_task_entry(token, user_task_first_run);
}

fn user_task_loop() -> ! {
    let space = placeholder_vm_space();
    // This loop is the task's own outermost frame, so one guard names it for
    // the whole loop: a task cannot stop being itself, and the borrow travels
    // with its frames across migration. It is the witness that authorises
    // handing the task's register snapshot to `UserMode`, which keeps it across
    // the iretq/SYSCALL round trip — the address is stable for the task's life.
    let current = Current::get().expect("user_task_loop: dispatched with no current task");
    let user_ctx = current.task().user_ctx(&current);
    loop {
        // The return-to-user tail. Interrupts are on, no lock is held, and the
        // preempt count is back at its baseline, so this is where a task under
        // sustained load pays for the deferred work its syscalls queued —
        // without it, reclamation would wait for a CPU to go idle.
        slopos_ostd::sync::bh::run_pending_if_due();

        // Marked for death before this task ever reached userland, or between
        // a syscall exit and here: leave through its own exit path rather than
        // entering user mode to be stopped at the next boundary. Routed
        // through delivery so the exit code comes from the pending signal's
        // own disposition rather than being invented here.
        if current.task().is_killed() {
            crate::syscall::signal::deliver_pending_signal(&current, user_ctx);
            scheduler_task_exit_impl();
        }

        let reason = {
            let user_mode = UserMode::new(user_ctx, space);
            user_mode.execute()
        };

        match reason {
            ReturnReason::Syscall(_n) => {
                // Hand the per-task UserContext straight to the syscall
                // dispatcher; handlers take the OSTD context by borrow, no
                // adapter required.
                if panic_recovery::production_recovery_enabled() {
                    match panic_recovery::run_recoverable(|| {
                        crate::syscall::dispatch::syscall_handle(user_ctx);
                    }) {
                        Ok(()) => {}
                        Err(oops) => {
                            klog_info!(
                                "panic recovery: syscall task={} {}:{}:{}: {} (oops total={})",
                                oops.task_id,
                                oops.file.as_str(),
                                oops.line,
                                oops.column,
                                oops.reason.as_str(),
                                panic_recovery::oops_count(),
                            );
                            scheduler_task_exit_impl();
                        }
                    }
                } else {
                    crate::syscall::dispatch::syscall_handle(user_ctx);
                }
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
