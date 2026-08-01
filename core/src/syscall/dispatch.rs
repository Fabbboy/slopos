use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{SA_RESTART, SIG_DFL, SIG_IGN, SIGNAL_MASK};
use slopos_abi::syscall::ERRNO_ERESTARTSYS;
use slopos_abi::task::TASK_FLAG_USER_MODE;
use slopos_ostd::klog_info;
use slopos_ostd::user::context::UserContext;
use slopos_sched::task_struct::Task;

use crate::syscall::context::SyscallContext;
use crate::syscall::handlers::syscall_lookup;
use crate::syscall::result::SyscallResult;

pub fn syscall_handle(user_ctx: &UserContext) {
    let sysno = user_ctx.rax();

    // The task running this syscall is this CPU's current, by definition of how
    // we got here. The guard is taken once for the whole exit path: the restart
    // decision and the signal delivery below both need the task, and the
    // delivery runs on the no-handler arm too.
    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return;
    };
    let task = current.task();
    if (task.flags & TASK_FLAG_USER_MODE) == 0 {
        return;
    }

    // Clobber rax with a safe negative sentinel so that a handler
    // that forgets to write a return value does not leak stale
    // register contents to userland.
    user_ctx.set_rax(slopos_abi::syscall::ERRNO_EINVAL as u64);

    let entry = syscall_lookup(sysno);
    let handler = entry.and_then(|e| e.handler);

    match handler {
        Some(func) => {
            let ctx = SyscallContext::from_current(&current, user_ctx);
            let result = func(&ctx);
            ctx.write_result(result);

            // ERESTARTSYS signal-restart logic: when a blocking
            // syscall (typically a TTY read) is interrupted by a
            // signal, the handler returns `Err(Errno::ERESTARTSYS)`
            // which `ctx.write_result` translated to the
            // `ERRNO_ERESTARTSYS` sentinel in `rax`. This block
            // decides whether to transparently restart or convert to
            // `EINTR`, based on the pending signal's `SA_RESTART`
            // flag. Runs before `deliver_pending_signal` so the
            // signal frame captures the correct state.
            handle_erestartsys(task, user_ctx, sysno);

            // Safety net: ERESTARTSYS must NEVER leak to userland.
            debug_assert_erestartsys_not_leaked(user_ctx);
        }
        None => {
            if entry.is_none() {
                klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
            }
            // Reserved table slot with no handler — return ENOSYS.
            user_ctx.set_rax(slopos_abi::syscall::ENOSYS_RETURN);
        }
    }

    // Deliver pending signals on every syscall exit path, not just
    // when a handler ran. Linux checks TIF_SIGPENDING unconditionally
    // on return to userspace.
    crate::syscall::signal::deliver_pending_signal(&current, user_ctx);
}

// ---------------------------------------------------------------------------
// ERESTARTSYS restart handling
// ---------------------------------------------------------------------------

/// The x86_64 `syscall` instruction is 2 bytes (`0F 05`). After
/// `syscall`, RCX holds the return address; the kernel saves it as
/// `frame.rip`. Rewinding by 2 bytes points back at the `syscall`
/// instruction itself, enabling transparent re-execution.
const SYSCALL_INSN_SIZE: u64 = 2;

/// Inspect the syscall return value and, if it is `ERESTARTSYS`,
/// decide whether to transparently restart the syscall or convert to
/// `EINTR`.
/// Syscalls that carry a caller-supplied timeout.
///
/// `ERESTARTSYS` restarts from argument zero — the rewind below reloads `rax`
/// with the syscall number and steps `rip` back onto the `syscall`
/// instruction — so a restart re-arms the *original* timeout. Under signal
/// pressure that livelocks: each delivery restarts a fresh full-length wait.
/// These must report `EINTR` and let userland re-derive the remainder, or
/// carry an absolute deadline in their own loop.
const TIMEOUT_BEARING: &[u64] = &[
    slopos_abi::syscall::SYSCALL_SLEEP_MS,
    slopos_abi::syscall::SYSCALL_POLL,
    slopos_abi::syscall::SYSCALL_SELECT,
    slopos_abi::syscall::SYSCALL_FUTEX,
    slopos_abi::syscall::SYSCALL_RING_ENTER,
];

fn handle_erestartsys(task_ref: &Task, user_ctx: &UserContext, sysno: u64) {
    let result = user_ctx.rax();
    if result != ERRNO_ERESTARTSYS {
        return;
    }
    debug_assert!(
        !TIMEOUT_BEARING.contains(&sysno),
        "syscall {sysno} returned ERESTARTSYS with a caller-supplied timeout; \
         it must return EINTR so the remaining time is not re-armed"
    );

    let pending = task_ref.signal_pending.load(Ordering::Acquire);
    let blocked = task_ref.signal_blocked();
    let deliverable = pending & SIGNAL_MASK & !blocked;
    let (handler, flags) = if deliverable == 0 {
        (0u64, 0u64)
    } else {
        let signum = (deliverable.trailing_zeros() + 1) as u8;
        let idx = (signum as usize).wrapping_sub(1);
        let action = task_ref.signal_actions[idx].load_owner_only();
        (action.handler, action.flags)
    };

    let should_restart = if deliverable == 0 {
        true
    } else {
        let is_user_handler = handler != SIG_DFL && handler != SIG_IGN;
        if !is_user_handler {
            true
        } else if (flags & SA_RESTART) != 0 {
            true
        } else {
            false
        }
    };

    if should_restart {
        let mut regs = user_ctx.regs();
        regs.rip = regs.rip.wrapping_sub(SYSCALL_INSN_SIZE);
        regs.rax = sysno;
        user_ctx.set_regs(regs);
    } else {
        user_ctx.set_rax(Errno::EINTR.as_u64());
    }
}

/// Safety net: assert that `ERESTARTSYS` never leaks to userland. In
/// debug builds this would panic; release silently converts to
/// `EINTR` as a last resort.
fn debug_assert_erestartsys_not_leaked(user_ctx: &UserContext) {
    let rax = user_ctx.rax();
    if rax == ERRNO_ERESTARTSYS {
        user_ctx.set_rax(Errno::EINTR.as_u64());
    }
}

// ---------------------------------------------------------------------------
// Test-only re-entry point
// ---------------------------------------------------------------------------

/// Invoke a handler directly with a caller-built `UserContext`.
/// Mirrors the dispatch path: snapshots argument registers into the
/// `SyscallContext`, runs the handler, and writes the result back to
/// the user-mode frame. Used by `core/src/syscall/tests.rs` to drive
/// handlers without going through the full ISR entry.
pub fn dispatch_handler(
    handler: crate::syscall::common::SyscallHandler,
    task: &slopos_sched::task::TaskRef,
    frame: &mut UserContext,
) -> SyscallResult {
    // Test entry point: the fixture parks the BSP on a bootstrap stub, so
    // `Current::get()` yields nothing and the caller supplies the task as the
    // registry guard it already holds.
    let ctx = SyscallContext::from_task_ref(task, frame);
    let result = handler(&ctx);
    ctx.write_result(result);
    result
}
