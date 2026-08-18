//! Kernel-side wrapper that drives the OSTD `UserMode::execute()` round-trip on
//! every user task.
//!
//! [`user_task_first_run`] is what the scheduler dispatches into for a new
//! (forked, cloned) user task; [`user_task_loop`] then loops on
//! `UserMode::execute()` and hands each user→kernel return to
//! [`crate::syscall::dispatch::syscall_handle`].
//!
//! # Stack discipline
//!
//! [`user_task_loop`]'s frame must survive every iretq → user → SYSCALL round
//! trip, but `TSS.RSP0` points at the same per-task kernel stack, so IRQ pushes
//! from user mode land on it. Task creation therefore seeds
//! [`slopos_sched::task_struct::SwitchContext::rsp`] `SUPERVISOR_RESERVE` bytes
//! below `kernel_stack_top`, reserving the top region for the IRQ chain; see
//! `task_lifecycle::SUPERVISOR_RESERVE` for the budget sizing.

use slopos_ostd::KArc;
use slopos_ostd::klog_info;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::panic_recovery;
use slopos_ostd::sync::once_lock::OnceLock;
use slopos_ostd::user::mode::{ReturnReason, UserMode};

use slopos_sched::scheduler::scheduler_task_exit_impl;
use slopos_sched::task_struct::Current;

/// Single shared `VmSpace` handle used by every user task: the trusted-side
/// `UserModeBackend` activates the supplied `&VmSpace` as a no-op, CR3 staying
/// under legacy-kernel control.
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

/// Entry point used by the scheduler for new user tasks; the kernel stack is
/// seeded so `switch_registers` rets here on first dispatch. `extern "sysv64"`
/// because the address is stored there as a synthetic return address.
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
    // The task's own outermost frame, so one guard names it for the whole loop.
    // It is the witness that authorises handing the task's register snapshot to
    // `UserMode`, whose address is stable for the task's life.
    let current = Current::get().expect("user_task_loop: dispatched with no current task");
    let user_ctx = current.task().user_ctx(&current);
    loop {
        // The return-to-user tail: interrupts on, no lock held, preempt count
        // back at baseline — where a loaded task pays for the deferred work its
        // syscalls queued, instead of waiting for a CPU to go idle.
        slopos_ostd::sync::bh::run_pending_if_due();

        // Killed before ever reaching userland, or since the last syscall exit:
        // leave through the task's own exit path, routed via delivery so the
        // exit code comes from the pending signal's own disposition.
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
