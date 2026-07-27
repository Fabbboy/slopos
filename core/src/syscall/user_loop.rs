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
use slopos_ostd::user::context::UserContext;
use slopos_ostd::user::mode::{ReturnReason, UserMode};

use slopos_sched::scheduler::{scheduler_get_current_task, scheduler_task_exit_impl};
use slopos_sched::task_struct::Task;

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

slopos_ostd::extern_c_entry! {
    /// Entry point used by the scheduler for new user tasks. The
    /// kernel stack is set up so `switch_registers` rets here on first
    /// dispatch; see
    /// [`slopos_sched::task::task_lifecycle::build_user_task_entry_frame`].
    ///
    /// `extern "C"` because the address is taken and stored as a `u64`
    /// on the kernel stack as a synthetic return address.
    pub fn user_task_first_run() -> ! {
        let task = scheduler_get_current_task() as *mut Task;
        if task.is_null() {
            panic!("user_task_first_run: scheduler_get_current_task returned null");
        }
        user_task_loop(task)
    }
}

fn user_task_loop(task: *mut Task) -> ! {
    let space = placeholder_vm_space();
    loop {
        // `task` is the currently running task; nothing else mutates
        // `task->user_ctx` while it is the running task. The
        // `task_user_ctx_mut` accessor null-checks `task` once and
        // hands back a `&mut UserContext` whose lifetime is the loop
        // iteration scope.
        let ctx_ptr: *mut UserContext = slopos_ostd::task::accessors::task_user_ctx_ptr(task);
        let ctx_ref = UserContext::from_ptr_mut(ctx_ptr)
            .expect("user_task_loop: scheduler dispatched null task");

        let reason = {
            let user_mode = UserMode::new(ctx_ref, space);
            user_mode.execute()
        };

        match reason {
            ReturnReason::Syscall(_n) => {
                // Hand the per-task UserContext straight to the syscall
                // dispatcher; the handler signature now takes
                // `*mut UserContext`, no adapter required.
                if panic_recovery::production_recovery_enabled() {
                    match panic_recovery::run_recoverable(|| {
                        crate::syscall::dispatch::syscall_handle(ctx_ptr);
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
                    crate::syscall::dispatch::syscall_handle(ctx_ptr);
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
