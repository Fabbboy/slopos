use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{
    NSIG, SA_NODEFER, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIG_UNCATCHABLE, SIGKILL,
    SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit, sig_default_action,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskExitReason, TaskFaultReason};
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr as MmUserPtr;
use slopos_ostd::user::context::UserContext;

use crate::syscall::args::{Signum, UserPtr};
use crate::syscall::result::SyscallResult;
use slopos_sched::scheduler::{schedule, unblock_task};
use slopos_sched::task::{
    task_find_by_id, task_id_of, task_iterate_active, task_pgid, task_signal_raise, task_terminate,
};
use slopos_sched::task_struct::{SignalAction, Task};

fn parse_signum(raw: u64) -> Option<u8> {
    if raw == 0 || raw as usize > NSIG {
        None
    } else {
        Some(raw as u8)
    }
}

/// Heap-backed list of unique task IDs collected during signal
/// delivery.
struct TargetSet {
    ids: slopos_ostd::KVec<u32>,
}

impl TargetSet {
    fn new() -> Self {
        Self {
            ids: slopos_ostd::KVec::new(),
        }
    }

    fn push(&mut self, task_id: u32) {
        if task_id == INVALID_TASK_ID {
            return;
        }
        for id in self.ids.iter() {
            if *id == task_id {
                return;
            }
        }
        let _ = self.ids.push(task_id);
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn iter(&self) -> core::slice::Iter<'_, u32> {
        self.ids.iter()
    }
}

struct GroupCollectContext {
    pgid: u32,
    targets: *mut TargetSet,
}

struct AllCollectContext {
    exclude_task_id: u32,
    targets: *mut TargetSet,
}

fn collect_group_member(task: *mut Task, context: *mut c_void) {
    let Some(ctx) = slopos_ostd::util::ptr_buf::try_void_ctx_mut::<GroupCollectContext>(context)
    else {
        return;
    };
    let Some(pgid) = task_pgid(task) else {
        return;
    };
    if pgid != ctx.pgid {
        return;
    }
    let Some(tid) = task_id_of(task) else {
        return;
    };
    if let Some(set) = slopos_ostd::util::ptr_buf::try_borrow_ref_mut(ctx.targets) {
        set.push(tid);
    }
}

fn collect_all_members(task: *mut Task, context: *mut c_void) {
    let Some(ctx) = slopos_ostd::util::ptr_buf::try_void_ctx_mut::<AllCollectContext>(context)
    else {
        return;
    };
    let Some(task_id) = task_id_of(task) else {
        return;
    };
    if task_id == INVALID_TASK_ID || task_id == ctx.exclude_task_id {
        return;
    }
    if let Some(set) = slopos_ostd::util::ptr_buf::try_borrow_ref_mut(ctx.targets) {
        set.push(task_id);
    }
}

fn collect_targets_for_group(pgid: u32, targets: &mut TargetSet) {
    let mut ctx = GroupCollectContext {
        pgid,
        targets: targets as *mut TargetSet,
    };
    task_iterate_active(
        Some(collect_group_member),
        (&mut ctx as *mut GroupCollectContext).cast(),
    );
}

fn collect_targets_for_all(exclude_task_id: u32, targets: &mut TargetSet) {
    let mut ctx = AllCollectContext {
        exclude_task_id,
        targets: targets as *mut TargetSet,
    };
    task_iterate_active(
        Some(collect_all_members),
        (&mut ctx as *mut AllCollectContext).cast(),
    );
}

fn action_from_user(new_action: UserSigaction) -> SignalAction {
    SignalAction {
        handler: new_action.sa_handler,
        flags: new_action.sa_flags,
        restorer: new_action.sa_restorer,
        mask: new_action.sa_mask & !SIG_UNCATCHABLE,
    }
}

fn action_to_user(action: &SignalAction) -> UserSigaction {
    UserSigaction {
        sa_handler: action.handler,
        sa_flags: action.flags,
        sa_restorer: action.restorer,
        sa_mask: action.mask,
    }
}

define_syscall!(syscall_rt_sigaction
    (ctx, signum: Signum, new_act_ptr: u64, old_act_ptr: u64, sigsetsize: u64)
    -> Result<(), Errno>
{
    if sigsetsize != core::mem::size_of::<SigSet>() as u64 {
        return Err(Errno::EINVAL);
    }
    let signum = signum.raw();
    let task_ref = ctx.task_mut().ok_or(Errno::EINVAL)?;
    let idx = (signum - 1) as usize;

    if old_act_ptr != 0 {
        let old_ptr = MmUserPtr::<UserSigaction>::try_new(old_act_ptr).map_err(|_| Errno::EFAULT)?;
        let old_action = action_to_user(&task_ref.signal_actions[idx]);
        copy_to_user(old_ptr, &old_action).map_err(|_| Errno::EFAULT)?;
    }

    if new_act_ptr != 0 {
        if (sig_bit(signum) & SIG_UNCATCHABLE) != 0 {
            return Err(Errno::EINVAL);
        }
        let new_ptr = MmUserPtr::<UserSigaction>::try_new(new_act_ptr).map_err(|_| Errno::EFAULT)?;
        let new_action = copy_from_user(new_ptr).map_err(|_| Errno::EFAULT)?;
        if new_action.sa_handler != SIG_DFL
            && new_action.sa_handler != SIG_IGN
            && new_action.sa_restorer == 0
        {
            return Err(Errno::EINVAL);
        }
        task_ref.signal_actions[idx] = action_from_user(new_action);
    }

    Ok(())
});

define_syscall!(syscall_rt_sigprocmask
    (ctx, how: u32, set_ptr: u64, oldset_ptr: u64, sigsetsize: u64)
    -> Result<(), Errno>
{
    if sigsetsize != core::mem::size_of::<SigSet>() as u64 {
        return Err(Errno::EINVAL);
    }
    let task_ref = ctx.task_mut().ok_or(Errno::EINVAL)?;

    if oldset_ptr != 0 {
        let old_ptr = MmUserPtr::<SigSet>::try_new(oldset_ptr).map_err(|_| Errno::EFAULT)?;
        copy_to_user(old_ptr, &task_ref.signal_blocked).map_err(|_| Errno::EFAULT)?;
    }

    if set_ptr != 0 {
        let new_ptr = MmUserPtr::<SigSet>::try_new(set_ptr).map_err(|_| Errno::EFAULT)?;
        let set = copy_from_user(new_ptr).map_err(|_| Errno::EFAULT)?;

        let mut blocked = task_ref.signal_blocked;
        match how {
            slopos_abi::signal::SIG_BLOCK => blocked |= set,
            SIG_UNBLOCK => blocked &= !set,
            SIG_SETMASK => blocked = set,
            _ => return Err(Errno::EINVAL),
        }
        task_ref.signal_blocked = blocked & !SIG_UNCATCHABLE;
    }

    Ok(())
});

define_syscall!(syscall_kill
    (ctx, raw_pid_arg: i64, sig: u64) -> SyscallResult
{
    let caller_id = ctx.task_id().unwrap_or(INVALID_TASK_ID);

    if raw_pid_arg < i32::MIN as i64 || raw_pid_arg > i32::MAX as i64 {
        return SyscallResult::Err(Errno::ESRCH);
    }
    let pid = raw_pid_arg as i32;

    let mut targets = TargetSet::new();
    if pid > 0 {
        let target_id = pid as u32;
        if task_find_by_id(target_id).is_null() {
            return SyscallResult::Err(Errno::ESRCH);
        }
        targets.push(target_id);
    } else if pid == 0 {
        if caller_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        let caller = task_find_by_id(caller_id);
        if caller.is_null() {
            return SyscallResult::Err(Errno::ESRCH);
        }
        let caller_pgid = task_pgid(caller).unwrap_or(INVALID_TASK_ID);
        if caller_pgid == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_group(caller_pgid, &mut targets);
    } else if pid == -1 {
        if caller_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_all(caller_id, &mut targets);
    } else {
        if pid == i32::MIN {
            return SyscallResult::Err(Errno::ESRCH);
        }
        let group_id = (-pid) as u32;
        if group_id == INVALID_TASK_ID {
            return SyscallResult::Err(Errno::ESRCH);
        }
        collect_targets_for_group(group_id, &mut targets);
    }

    if targets.len() == 0 {
        return SyscallResult::Err(Errno::ESRCH);
    }

    if sig == 0 {
        return SyscallResult::Ok(0);
    }

    let Some(signum) = parse_signum(sig) else {
        return SyscallResult::Err(Errno::EINVAL);
    };

    let mut signaled = 0usize;
    let mut caller_terminated = false;

    for target_id in targets.iter() {
        let target = task_find_by_id(*target_id);
        if target.is_null() {
            continue;
        }

        if signum == SIGKILL {
            if task_terminate(*target_id) == 0 {
                signaled += 1;
                if *target_id == caller_id {
                    caller_terminated = true;
                }
            }
            continue;
        }

        let _ = task_signal_raise(target, sig_bit(signum));
        let _ = unblock_task(target);
        signaled += 1;
    }

    if signaled == 0 {
        return SyscallResult::Err(Errno::ESRCH);
    }

    if caller_terminated {
        schedule();
        return SyscallResult::NoReturn;
    }

    SyscallResult::Ok(0)
});

fn read_signal_frame(rsp: u64) -> Option<SignalFrame> {
    let ptr = MmUserPtr::<SignalFrame>::try_new(rsp).ok()?;
    copy_from_user(ptr).ok()
}

define_syscall!(syscall_rt_sigreturn (ctx) -> SyscallResult {
    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return SyscallResult::Err(Errno::EINVAL),
    };

    // After the handler's `ret` pops the restorer address, RSP points
    // directly at the SignalFrame.
    let rsp = ctx.user_rsp();
    let sigframe = match read_signal_frame(rsp) {
        Some(sf) => sf,
        None => return SyscallResult::Err(Errno::EFAULT),
    };

    task_ref.signal_blocked = sigframe.saved_mask & !SIG_UNCATCHABLE;

    // Rebuild the user GPR snapshot from the SignalFrame and commit
    // through `set_regs` (re-applies CS/SS selectors and RFLAGS mask).
    let mut regs = *ctx.user_ctx().regs();
    regs.rax = sigframe.rax;
    regs.rbx = sigframe.rbx;
    regs.rcx = sigframe.rcx;
    regs.rdx = sigframe.rdx;
    regs.rsi = sigframe.rsi;
    regs.rdi = sigframe.rdi;
    regs.rbp = sigframe.rbp;
    regs.rsp = sigframe.rsp;
    regs.r8 = sigframe.r8;
    regs.r9 = sigframe.r9;
    regs.r10 = sigframe.r10;
    regs.r11 = sigframe.r11;
    regs.r12 = sigframe.r12;
    regs.r13 = sigframe.r13;
    regs.r14 = sigframe.r14;
    regs.r15 = sigframe.r15;
    regs.rip = sigframe.rip;
    regs.rflags_user_subset = sigframe.rflags;
    ctx.user_ctx_mut().set_regs(regs);

    // sigreturn fully replaced the user-mode register state — the
    // dispatcher must not overwrite RAX after we return.
    SyscallResult::NoReturn
});

pub fn deliver_pending_signal(task: *mut Task, ctx_ptr: *mut UserContext) {
    let Some(user_ctx) = UserContext::from_ptr_mut(ctx_ptr) else {
        return;
    };
    let Some(task_ref) = slopos_sched::task::task_borrow_mut(task) else {
        return;
    };

    if (task_ref.flags & TASK_FLAG_USER_MODE) == 0 {
        return;
    }

    let pending = task_ref.signal_pending.load(Ordering::Acquire);
    let deliverable = pending & !task_ref.signal_blocked;
    if deliverable == 0 {
        return;
    }

    let signum = (deliverable.trailing_zeros() + 1) as u8;
    let bit = sig_bit(signum);
    task_ref.signal_pending.fetch_and(!bit, Ordering::AcqRel);

    let action = task_ref.signal_actions[(signum - 1) as usize];
    if action.handler == SIG_IGN {
        return;
    }

    if action.handler == SIG_DFL {
        match sig_default_action(signum) {
            SigDefault::Ignore | SigDefault::Stop | SigDefault::Continue => return,
            SigDefault::Terminate => {
                let task_id = task_ref.task_id;
                task_ref.exit_reason = TaskExitReason::Normal;
                task_ref.fault_reason = TaskFaultReason::None;
                task_ref.exit_code = 128 + signum as u32;
                if task_terminate(task_id) == 0 {
                    schedule();
                }
                return;
            }
        }
    }

    if action.restorer == 0 {
        return;
    }

    let regs_snapshot = *user_ctx.regs();

    let total_size = 8 + core::mem::size_of::<SignalFrame>() as u64;
    let frame_addr = regs_snapshot.rsp.wrapping_sub(total_size) & !0xF;

    let restorer_ptr = match MmUserPtr::<u64>::try_new(frame_addr) {
        Ok(p) => p,
        Err(_) => return,
    };
    if copy_to_user(restorer_ptr, &action.restorer).is_err() {
        return;
    }

    let sigframe_addr = frame_addr.wrapping_add(8);
    let sigframe_ptr = match MmUserPtr::<SignalFrame>::try_new(sigframe_addr) {
        Ok(p) => p,
        Err(_) => return,
    };

    let saved_mask = task_ref.signal_blocked;
    let sigframe = SignalFrame {
        signum: signum as u64,
        rax: regs_snapshot.rax,
        rbx: regs_snapshot.rbx,
        rcx: regs_snapshot.rcx,
        rdx: regs_snapshot.rdx,
        rsi: regs_snapshot.rsi,
        rdi: regs_snapshot.rdi,
        rbp: regs_snapshot.rbp,
        rsp: regs_snapshot.rsp,
        r8: regs_snapshot.r8,
        r9: regs_snapshot.r9,
        r10: regs_snapshot.r10,
        r11: regs_snapshot.r11,
        r12: regs_snapshot.r12,
        r13: regs_snapshot.r13,
        r14: regs_snapshot.r14,
        r15: regs_snapshot.r15,
        rip: regs_snapshot.rip,
        rflags: regs_snapshot.rflags_user_subset,
        saved_mask,
    };

    if copy_to_user(sigframe_ptr, &sigframe).is_err() {
        return;
    }

    let mut blocked = saved_mask | action.mask;
    if (action.flags & SA_NODEFER) == 0 {
        blocked |= bit;
    }
    task_ref.signal_blocked = blocked & !SIG_UNCATCHABLE;

    let mut regs = regs_snapshot;
    regs.rsp = frame_addr;
    regs.rip = action.handler;
    regs.rdi = signum as u64;
    regs.rsi = 0;
    regs.rdx = 0;
    user_ctx.set_regs(regs);
}

#[allow(dead_code)]
type _Unused<T> = UserPtr<T>;
