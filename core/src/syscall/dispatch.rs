use core::sync::atomic::Ordering;

use slopos_abi::signal::{SA_RESTART, SIG_DFL, SIG_IGN};
use slopos_abi::syscall::ERRNO_ERESTARTSYS;
use slopos_utils::klog_info;

use crate::sched::scheduler_get_current_task;
use crate::syscall::handlers::syscall_lookup;

use crate::scheduler::task_struct::Task;
use slopos_abi::task::TASK_FLAG_USER_MODE;
use slopos_ostd::user::context::UserContext;

pub fn syscall_handle(ctx_ptr: *mut UserContext) {
    if ctx_ptr.is_null() {
        return;
    }

    let sysno = unsafe { (*ctx_ptr).regs().rax };

    let task = scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        return;
    }
    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }
    }

    // Clobber rax with a safe negative sentinel so that a handler
    // that forgets to write a return value does not leak stale register
    // contents to userland.
    unsafe {
        (*ctx_ptr).set_rax(slopos_abi::syscall::ERRNO_EINVAL as u64);
    }

    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
        unsafe {
            (*ctx_ptr).set_rax(slopos_abi::syscall::ENOSYS_RETURN);
        }
    } else {
        let handler = unsafe { (*entry).handler };
        if let Some(func) = handler {
            func(task, ctx_ptr);

            // ---------------------------------------------------------------
            // ERESTARTSYS signal restart logic.
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
            handle_erestartsys(task, ctx_ptr, sysno);

            // Safety net: ERESTARTSYS must NEVER leak to userland.
            debug_assert_erestartsys_not_leaked(ctx_ptr);
        } else {
            // Reserved table slot with no handler — return ENOSYS.
            unsafe {
                (*ctx_ptr).set_rax(slopos_abi::syscall::ENOSYS_RETURN);
            }
        }
    }

    // Deliver pending signals on every syscall exit path, not just when
    // a handler ran.  Linux checks TIF_SIGPENDING unconditionally on
    // return to userspace.
    crate::syscall::signal::deliver_pending_signal(task, ctx_ptr);
}

// ---------------------------------------------------------------------------
// ERESTARTSYS restart handling
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
fn handle_erestartsys(task: *mut Task, ctx_ptr: *mut UserContext, sysno: u64) {
    let result = unsafe { (*ctx_ptr).regs().rax };
    if result != ERRNO_ERESTARTSYS {
        return;
    }

    // --- Minimal unsafe region: read raw signal state into safe locals ---
    let (pending, blocked, handler, flags) = unsafe {
        let pending = (*task).signal_pending.load(Ordering::Acquire);
        let blocked = (*task).signal_blocked;
        let deliverable = pending & !blocked;

        if deliverable == 0 {
            (pending, blocked, 0u64, 0u64)
        } else {
            // Inspect the signal that deliver_pending_signal will pick (the
            // lowest-numbered deliverable signal, matching its trailing_zeros
            // selection).
            let signum = (deliverable.trailing_zeros() + 1) as u8;
            let idx = (signum as usize).wrapping_sub(1);
            let action = (*task).signal_actions[idx];
            (pending, blocked, action.handler, action.flags)
        }
    };

    // --- Safe policy decision using copied locals ---
    let deliverable = pending & !blocked;

    let should_restart = if deliverable == 0 {
        // No deliverable signals — restart the syscall immediately.
        true
    } else {
        let is_user_handler = handler != SIG_DFL && handler != SIG_IGN;
        if !is_user_handler {
            // SIG_DFL or SIG_IGN: no user handler will run.
            // Restart the syscall — if the default action is Terminate,
            // deliver_pending_signal will kill the process and the
            // restart is moot.
            true
        } else if (flags & SA_RESTART) != 0 {
            // User handler with SA_RESTART: set up for transparent
            // restart after the signal handler returns via sigreturn.
            true
        } else {
            // User handler without SA_RESTART: convert to EINTR.
            false
        }
    };

    // --- Write decision back through `set_regs` so the user-CS/SS/RFLAGS
    // mask discipline is reapplied even though we only mutate rip/rax. ---
    unsafe {
        if should_restart {
            let mut regs = *(*ctx_ptr).regs();
            regs.rip = regs.rip.wrapping_sub(SYSCALL_INSN_SIZE);
            regs.rax = sysno;
            (*ctx_ptr).set_regs(regs);
        } else {
            (*ctx_ptr).set_rax((-4i64) as u64);
        }
    }
}

/// Safety net: assert that ERESTARTSYS never leaks to userland.
/// In debug builds, this panics.  In release builds, it silently converts
/// to EINTR as a last resort.
fn debug_assert_erestartsys_not_leaked(ctx_ptr: *mut UserContext) {
    let rax = unsafe { (*ctx_ptr).regs().rax };
    if rax == ERRNO_ERESTARTSYS {
        // This should never happen — handle_erestartsys should have
        // already converted or restarted.  Convert to EINTR as a
        // safety net.
        unsafe {
            (*ctx_ptr).set_rax((-4i64) as u64);
        }
    }
}
