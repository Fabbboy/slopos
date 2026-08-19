use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{SA_RESTART, SIG_DFL, SIG_IGN, SIGNAL_MASK};
use slopos_abi::syscall::ERRNO_ERESTARTSYS;
use slopos_abi::task::TASK_FLAG_USER_MODE;
use slopos_ostd::klog_info;
use slopos_ostd::user::context::UserContext;
use slopos_sched::task_struct::Task;

use slopos_ostd::authority::{AuthorityDecision, decide};

use crate::syscall::common::SyscallEntry;
use crate::syscall::context::SyscallContext;
use crate::syscall::handlers::syscall_lookup;
use crate::syscall::result::SyscallResult;

/// Whether `task` may invoke the operation `entry` classifies.
///
/// Returns `true` for every ungated operation without reading anything, so the
/// common path is one compare on a byte already in cache.
///
/// The failure mode is split by kind, deliberately: invoking an operation your
/// authority does not name is a *program bug* and is loud, because a program
/// that asks for something it can never have wants to be told. Acting on an
/// object you were not given is an ordinary `EPERM` and is silent — that one is
/// an attacker probing, and a log line per probe is the denial-of-service.
#[inline]
fn authorize(task: &Task, entry: &SyscallEntry, sysno: u64) -> bool {
    if !entry.cap.is_gated() {
        return true;
    }
    match decide(task_caps(task), entry.cap) {
        AuthorityDecision::Allow => true,
        AuthorityDecision::WarnAndAllow => {
            klog_info!(
                "AUTHORITY: task {} invoked syscall {} without {} (authority=warn)",
                task.task_id,
                sysno,
                entry.cap.name(),
            );
            true
        }
        AuthorityDecision::Deny => false,
    }
}

/// The caller's effective capability mask.
///
/// Derived from `task.flags` until the `Cred` lands: the flags *are* the whole
/// of today's privilege model, so deriving keeps one source of truth rather
/// than adding a second that could disagree with it. The `Process`-owned
/// `Cred` replaces this body without moving the call site.
#[inline]
fn task_caps(task: &Task) -> u64 {
    slopos_ostd::task::ops::task_caps(task)
}

pub fn syscall_handle(user_ctx: &UserContext) {
    let sysno = user_ctx.rax();

    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return;
    };
    let task = current.task();
    if (task.flags & TASK_FLAG_USER_MODE) == 0 {
        return;
    }

    // A handler that forgets to write a return value must not leak stale
    // register contents to userland.
    user_ctx.set_rax(slopos_abi::syscall::ERRNO_EINVAL as u64);

    let entry = syscall_lookup(sysno);
    let handler = entry.and_then(|e| e.handler);

    match handler {
        Some(func) => {
            // Here, not in the handler: the entry is already in cache, so the
            // check is one compare beside the pointer about to be called — and
            // a handler cannot forget what it does not perform.
            if let Some(entry) = entry
                && !authorize(task, entry, sysno)
            {
                user_ctx.set_rax(Errno::EPERM.as_u64());
                crate::syscall::signal::deliver_pending_signal(&current, user_ctx);
                return;
            }
            let ctx = SyscallContext::from_current(&current, user_ctx);
            let result = func(&ctx);
            ctx.write_result(result);

            // Must precede `deliver_pending_signal` so the signal frame
            // captures the rewound state.
            handle_erestartsys(task, user_ctx, sysno);

            debug_assert_erestartsys_not_leaked(user_ctx);
        }
        None => {
            if entry.is_none() {
                klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
            }
            user_ctx.set_rax(slopos_abi::syscall::ENOSYS_RETURN);
        }
    }

    crate::syscall::signal::deliver_pending_signal(&current, user_ctx);
}

/// The x86_64 `syscall` instruction is 2 bytes (`0F 05`), so rewinding
/// `frame.rip` by that points back at it for transparent re-execution.
const SYSCALL_INSN_SIZE: u64 = 2;

/// Syscalls that carry a caller-supplied timeout.
///
/// A restart re-arms the *original* timeout, so under signal pressure each
/// delivery starts a fresh full-length wait. These must report `EINTR`.
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

/// Last resort: `ERESTARTSYS` must never reach userland, so convert it.
fn debug_assert_erestartsys_not_leaked(user_ctx: &UserContext) {
    let rax = user_ctx.rax();
    if rax == ERRNO_ERESTARTSYS {
        user_ctx.set_rax(Errno::EINTR.as_u64());
    }
}

/// Invoke a handler with a caller-built `UserContext`, bypassing ISR entry.
///
/// **Runs no authority check.** For a caller that has already made the
/// decision, or one invoking an operation classified as needing nothing. A
/// test reaching a gated handler through this proves the handler's own logic
/// and says nothing about whether the operation is reachable — use
/// [`dispatch_entry`] for that.
pub fn dispatch_handler(
    handler: crate::syscall::common::SyscallHandler,
    task: &slopos_sched::task::TaskRef,
    frame: &mut UserContext,
) -> SyscallResult {
    let ctx = SyscallContext::from_task_ref(task, frame);
    let result = handler(&ctx);
    ctx.write_result(result);
    result
}

/// Invoke a syscall through its table entry, applying the same authority
/// decision `syscall_handle` makes.
///
/// The difference from [`dispatch_handler`] is the whole point: the capability
/// check lives in the dispatcher, so a path that calls a handler directly is
/// *not* the syscall. Anything asserting what userland can reach must come
/// through here.
pub fn dispatch_entry(
    entry: &SyscallEntry,
    task: &slopos_sched::task::TaskRef,
    frame: &mut UserContext,
) -> SyscallResult {
    let Some(handler) = entry.handler else {
        return SyscallResult::Err(Errno::ENOSYS);
    };
    if !authorize(task, entry, u64::MAX) {
        let ctx = SyscallContext::from_task_ref(task, frame);
        let result = SyscallResult::Err(Errno::EPERM);
        ctx.write_result(result);
        return result;
    }
    dispatch_handler(handler, task, frame)
}
