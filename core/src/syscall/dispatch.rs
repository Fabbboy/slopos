use core::sync::atomic::Ordering;

use slopos_abi::signal::{SA_RESTART, SIG_DFL, SIG_IGN};
use slopos_abi::syscall::ERRNO_ERESTARTSYS;
use slopos_lib::klog_info;

use crate::sched::save_task_context_from_interrupt_frame;
use crate::sched::scheduler_get_current_task;
use crate::syscall::handlers::syscall_lookup;

use crate::scheduler::task_struct::Task;
use slopos_abi::task::{TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE};
use slopos_lib::InterruptFrame;

struct NoPreemptGuard {
    task: *mut Task,
}

impl NoPreemptGuard {
    fn new(task: *mut Task) -> Self {
        unsafe { (*task).flags |= TASK_FLAG_NO_PREEMPT };
        Self { task }
    }
}

impl Drop for NoPreemptGuard {
    fn drop(&mut self) {
        if !self.task.is_null() {
            unsafe { (*self.task).flags &= !TASK_FLAG_NO_PREEMPT };
        }
    }
}

pub fn syscall_handle(frame: *mut InterruptFrame) {
    if frame.is_null() {
        return;
    }

    let sysno = unsafe { (*frame).rax };

    let task = scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        return;
    }
    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }
    }

    // CRITICAL: Set NO_PREEMPT *before* saving user context.
    //
    // save_task_context_from_interrupt_frame sets context_from_user=1,
    // which tells the scheduler it can resume this task directly from
    // task.context via IRETQ (skipping kernel context save).  Without
    // NO_PREEMPT held first, a timer interrupt between the context save
    // and the handler completion could trigger a context switch that
    // resumes from stale task.context values (e.g. rax still holding
    // the raw syscall number instead of the handler's return code).
    //
    // With NO_PREEMPT set, the scheduler sees in_syscall_block_path=true
    // and falls back to saving/restoring kernel context, which correctly
    // resumes execution within syscall_handle rather than jumping back
    // to userspace with stale register values.
    let _no_preempt = NoPreemptGuard::new(task);

    // Save user context snapshot.  frame.rax still holds the original
    // syscall number, giving the context a correct pre-syscall snapshot
    // (used for signal delivery, core dumps, ptrace).
    save_task_context_from_interrupt_frame(task, frame, true);

    // Clobber frame.rax with a safe negative sentinel.  If the handler
    // panics or misses a return path, userland gets -EINVAL rather than
    // the raw syscall number interpreted as a character.
    unsafe {
        (*frame).rax = slopos_abi::syscall::ERRNO_EINVAL as u64;
    }

    let pid = unsafe { (*task).process_id };
    let _provider_guard = slopos_mm::user_copy::set_syscall_process_id(pid);

    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
        unsafe {
            (*frame).rax = slopos_abi::syscall::ENOSYS_RETURN;
        }
    } else {
        let handler = unsafe { (*entry).handler };
        if let Some(func) = handler {
            func(task, frame);

            // ---------------------------------------------------------------
            // Finishing Phase 7: ERESTARTSYS signal restart logic.
            //
            // If the handler returned ERESTARTSYS (-512), the blocking
            // syscall (typically a TTY read) was interrupted by a signal.
            // We decide here whether to restart transparently or convert
            // to EINTR, based on the pending signal's SA_RESTART flag.
            //
            // This MUST run before deliver_pending_signal so that the
            // signal frame captures the correct state (either the
            // restart state or the EINTR state).
            // ---------------------------------------------------------------
            handle_erestartsys(task, frame, sysno);

            crate::syscall::signal::deliver_pending_signal(task, frame);

            // Safety net: ERESTARTSYS must NEVER leak to userland.
            debug_assert_erestartsys_not_leaked(frame);
        }
    }

    // Sync all frame registers that may have been modified back to the
    // saved user context.  This MUST happen while NO_PREEMPT is still
    // held (before NoPreemptGuard drops).
    //
    // After the guard drops there is a window before the assembly `cli`
    // where a timer interrupt can trigger schedule_from_trap_exit().
    // The scheduler sees context_from_user=1 and NO_PREEMPT=0, so it
    // may resume this task from task.context via context_switch_user/IRETQ.
    // Without this sync, stale pre-handler values would leak to userland.
    //
    // Registers potentially modified by:
    //   - Syscall handler: rax (return value)
    //   - Signal delivery: rip, rsp, rdi, rsi, rdx (redirected to
    //     signal trampoline)
    unsafe {
        (*task).context.rax = (*frame).rax;
        (*task).context.rip = (*frame).rip;
        (*task).context.rsp = (*frame).rsp;
        (*task).context.rdi = (*frame).rdi;
        (*task).context.rsi = (*frame).rsi;
        (*task).context.rdx = (*frame).rdx;
    }
}

// ---------------------------------------------------------------------------
// Finishing Phase 7: ERESTARTSYS restart handling
//
// When a blocking syscall (currently TTY read) is interrupted by a signal,
// it returns ERESTARTSYS (-512) instead of EINTR.  This function inspects
// the pending signal that caused the interruption and decides:
//
//   - User handler with SA_RESTART: rewind RIP to the `syscall` instruction
//     and restore RAX to the original syscall number.  When the signal
//     handler returns via sigreturn, the process transparently re-executes
//     the syscall.
//
//   - User handler without SA_RESTART: convert to EINTR (-4).  The signal
//     frame will capture this value, and userland sees EINTR after the
//     handler returns.
//
//   - SIG_DFL or SIG_IGN: the signal does not invoke a user handler.
//     Restart the syscall unconditionally (the signal is either ignored or
//     its default action will be taken — if that action terminates the
//     process, the restart is moot).
//
//   - No pending signals: restart unconditionally.  The signal was consumed
//     between the TTY wait and this check (e.g. by concurrent delivery).
// ---------------------------------------------------------------------------

/// The x86_64 `syscall` instruction is 2 bytes (0F 05).  After `syscall`,
/// RCX holds the return address (instruction after `syscall`), which the
/// kernel saves as frame.rip.  Rewinding by 2 bytes points back at the
/// `syscall` instruction itself, enabling transparent re-execution.
const SYSCALL_INSN_SIZE: u64 = 2;

/// Inspect the syscall return value and, if it is `ERESTARTSYS`, decide
/// whether to transparently restart the syscall or convert to `EINTR`.
///
/// # Signal-inspection race safety
///
/// This function reads `signal_pending` and inspects `signal_actions`
/// *before* `deliver_pending_signal` dequeues the signal.  A concurrent
/// signal arrival could theoretically alter the set between the two
/// operations, causing the `SA_RESTART` decision to be made against a
/// different signal than the one actually delivered.
///
/// This race is safe in practice because:
///
/// 1. **Same-CPU signals**: This code runs in the syscall return path
///    with interrupts disabled (we are inside the ISR / SYSCALL exit).
///    No interrupt handler on this CPU can post a new signal between
///    our read and `deliver_pending_signal`.
///
/// 2. **Cross-CPU signals**: Another CPU may set a bit in
///    `signal_pending` via IPI, but that IPI will not be serviced on
///    this CPU until interrupts are re-enabled — which happens *after*
///    both `handle_erestartsys` and `deliver_pending_signal` have
///    completed.
///
/// 3. **Safety net**: `debug_assert_erestartsys_not_leaked` catches any
///    leaked `ERESTARTSYS` value as a last resort.
fn handle_erestartsys(task: *mut Task, frame: *mut InterruptFrame, sysno: u64) {
    let result = unsafe { (*frame).rax };
    if result != ERRNO_ERESTARTSYS {
        return;
    }

    unsafe {
        let pending = (*task).signal_pending.load(Ordering::Acquire);
        let deliverable = pending & !(*task).signal_blocked;

        if deliverable == 0 {
            // No deliverable signals — restart the syscall immediately.
            (*frame).rip = (*frame).rip.wrapping_sub(SYSCALL_INSN_SIZE);
            (*frame).rax = sysno;
            return;
        }

        // Inspect the signal that deliver_pending_signal will pick (the
        // lowest-numbered deliverable signal, matching its trailing_zeros
        // selection).
        let signum = (deliverable.trailing_zeros() + 1) as u8;
        let idx = (signum as usize).wrapping_sub(1);
        let action = (*task).signal_actions[idx];

        let is_user_handler = action.handler != SIG_DFL && action.handler != SIG_IGN;

        if !is_user_handler {
            // SIG_DFL or SIG_IGN: no user handler will run.
            // Restart the syscall — if the default action is Terminate,
            // deliver_pending_signal will kill the process and the
            // restart is moot.
            (*frame).rip = (*frame).rip.wrapping_sub(SYSCALL_INSN_SIZE);
            (*frame).rax = sysno;
        } else if (action.flags & SA_RESTART) != 0 {
            // User handler with SA_RESTART: set up for transparent
            // restart after the signal handler returns via sigreturn.
            (*frame).rip = (*frame).rip.wrapping_sub(SYSCALL_INSN_SIZE);
            (*frame).rax = sysno;
        } else {
            // User handler without SA_RESTART: convert to EINTR.
            (*frame).rax = (-4i64) as u64;
        }
    }
}

/// Safety net: assert that ERESTARTSYS never leaks to userland.
/// In debug builds, this panics.  In release builds, it silently converts
/// to EINTR as a last resort.
fn debug_assert_erestartsys_not_leaked(frame: *mut InterruptFrame) {
    let rax = unsafe { (*frame).rax };
    if rax == ERRNO_ERESTARTSYS {
        // This should never happen — handle_erestartsys should have
        // already converted or restarted.  Convert to EINTR as a
        // safety net.
        unsafe {
            (*frame).rax = (-4i64) as u64;
        }
    }
}
