use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::signal::{
    NSIG, SA_NODEFER, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIG_UNCATCHABLE, SIGKILL,
    SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit, sig_default_action,
};
use slopos_abi::syscall::{ERRNO_EFAULT, ERRNO_EINVAL, ERRNO_ESRCH};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskExitReason, TaskFaultReason};
use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::user::context::UserContext;

use crate::sched::{schedule, unblock_task};
use crate::scheduler::task::{task_find_by_id, task_iterate_active, task_terminate};
use crate::scheduler::task_struct::{SignalAction, Task};
use crate::syscall::common::{SyscallDisposition, syscall_return_err};
use crate::syscall::context::SyscallContext;

fn parse_signum(raw: u64) -> Option<u8> {
    if raw == 0 || raw as usize > NSIG {
        None
    } else {
        Some(raw as u8)
    }
}

/// Heap-backed list of unique task IDs collected during signal
/// delivery. Uses a `KVec` so the struct is fixed size on the stack
/// (just the three `KVec` header words) regardless of how many tasks
/// we end up targeting — sized arrays would force the whole stack
/// frame over the 2 KiB gate.
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
        // Best-effort: on OOM the target is silently dropped. A rare
        // signal-send to an exhausted heap is acceptable — the caller
        // receives at most `signaled == 0 → ERRNO_ESRCH`.
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
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *(context as *mut GroupCollectContext) };
    if unsafe { (*task).pgid } != ctx.pgid {
        return;
    }

    unsafe { (&mut *ctx.targets).push((*task).task_id) };
}

fn collect_all_members(task: *mut Task, context: *mut c_void) {
    if task.is_null() || context.is_null() {
        return;
    }

    let ctx = unsafe { &mut *(context as *mut AllCollectContext) };
    let task_id = unsafe { (*task).task_id };
    if task_id == INVALID_TASK_ID || task_id == ctx.exclude_task_id {
        return;
    }

    unsafe { (&mut *ctx.targets).push(task_id) };
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

pub fn syscall_rt_sigaction(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    if ctx_ptr.is_null() {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    }
    let Some(ctx) = SyscallContext::from_user_context(task, unsafe { &mut *ctx_ptr }) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };

    let args = ctx.args();
    if args.arg3 != core::mem::size_of::<SigSet>() as u64 {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let Some(signum) = parse_signum(args.arg0) else {
        return ctx.err_with(ERRNO_EINVAL);
    };

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return ctx.err_with(ERRNO_EINVAL),
    };
    let idx = (signum - 1) as usize;

    if args.arg2 != 0 {
        let old_ptr = match UserPtr::<UserSigaction>::try_new(args.arg2) {
            Ok(p) => p,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };
        let old_action = action_to_user(&task_ref.signal_actions[idx]);
        if copy_to_user(old_ptr, &old_action).is_err() {
            return ctx.err_with(ERRNO_EFAULT);
        }
    }

    if args.arg1 != 0 {
        if (sig_bit(signum) & SIG_UNCATCHABLE) != 0 {
            return ctx.err_with(ERRNO_EINVAL);
        }
        let new_ptr = match UserPtr::<UserSigaction>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };
        let new_action = match copy_from_user(new_ptr) {
            Ok(a) => a,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };
        if new_action.sa_handler != SIG_DFL
            && new_action.sa_handler != SIG_IGN
            && new_action.sa_restorer == 0
        {
            return ctx.err_with(ERRNO_EINVAL);
        }
        task_ref.signal_actions[idx] = action_from_user(new_action);
    }

    ctx.ok(0)
}

pub fn syscall_rt_sigprocmask(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    if ctx_ptr.is_null() {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    }
    let Some(ctx) = SyscallContext::from_user_context(task, unsafe { &mut *ctx_ptr }) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };

    let args = ctx.args();
    if args.arg3 != core::mem::size_of::<SigSet>() as u64 {
        return ctx.err_with(ERRNO_EINVAL);
    }

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return ctx.err_with(ERRNO_EINVAL),
    };

    if args.arg2 != 0 {
        let old_ptr = match UserPtr::<SigSet>::try_new(args.arg2) {
            Ok(p) => p,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };
        if copy_to_user(old_ptr, &task_ref.signal_blocked).is_err() {
            return ctx.err_with(ERRNO_EFAULT);
        }
    }

    if args.arg1 != 0 {
        let new_ptr = match UserPtr::<SigSet>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };
        let set = match copy_from_user(new_ptr) {
            Ok(v) => v,
            Err(_) => return ctx.err_with(ERRNO_EFAULT),
        };

        let mut blocked = task_ref.signal_blocked;
        match args.arg0 as u32 {
            slopos_abi::signal::SIG_BLOCK => blocked |= set,
            SIG_UNBLOCK => blocked &= !set,
            SIG_SETMASK => blocked = set,
            _ => return ctx.err_with(ERRNO_EINVAL),
        }
        task_ref.signal_blocked = blocked & !SIG_UNCATCHABLE;
    }

    ctx.ok(0)
}

pub fn syscall_kill(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    if ctx_ptr.is_null() {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    }
    let Some(ctx) = SyscallContext::from_user_context(task, unsafe { &mut *ctx_ptr }) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };

    let args = ctx.args();
    let caller_id = ctx.task_id().unwrap_or(INVALID_TASK_ID);

    let raw_pid = args.arg0 as i64;
    if raw_pid < i32::MIN as i64 || raw_pid > i32::MAX as i64 {
        return ctx.err_with(ERRNO_ESRCH);
    }
    let pid = raw_pid as i32;

    let mut targets = TargetSet::new();
    if pid > 0 {
        let target_id = pid as u32;
        if task_find_by_id(target_id).is_null() {
            return ctx.err_with(ERRNO_ESRCH);
        }
        targets.push(target_id);
    } else if pid == 0 {
        if caller_id == INVALID_TASK_ID {
            return ctx.err_with(ERRNO_ESRCH);
        }
        let caller = task_find_by_id(caller_id);
        if caller.is_null() {
            return ctx.err_with(ERRNO_ESRCH);
        }
        let caller_pgid = unsafe { (*caller).pgid };
        if caller_pgid == INVALID_TASK_ID {
            return ctx.err_with(ERRNO_ESRCH);
        }
        collect_targets_for_group(caller_pgid, &mut targets);
    } else if pid == -1 {
        if caller_id == INVALID_TASK_ID {
            return ctx.err_with(ERRNO_ESRCH);
        }
        collect_targets_for_all(caller_id, &mut targets);
    } else {
        if pid == i32::MIN {
            return ctx.err_with(ERRNO_ESRCH);
        }
        let group_id = (-pid) as u32;
        if group_id == INVALID_TASK_ID {
            return ctx.err_with(ERRNO_ESRCH);
        }
        collect_targets_for_group(group_id, &mut targets);
    }

    if targets.len() == 0 {
        return ctx.err_with(ERRNO_ESRCH);
    }

    if args.arg1 == 0 {
        return ctx.ok(0);
    }

    let Some(signum) = parse_signum(args.arg1) else {
        return ctx.err_with(ERRNO_EINVAL);
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

        unsafe {
            (*target)
                .signal_pending
                .fetch_or(sig_bit(signum), Ordering::AcqRel);
        }
        let _ = unblock_task(target);
        signaled += 1;
    }

    if signaled == 0 {
        return ctx.err_with(ERRNO_ESRCH);
    }

    if caller_terminated {
        schedule();
        return SyscallDisposition::NoReturn;
    }

    ctx.ok(0)
}

fn read_signal_frame(rsp: u64) -> Option<SignalFrame> {
    let ptr = UserPtr::<SignalFrame>::try_new(rsp).ok()?;
    copy_from_user(ptr).ok()
}

pub fn syscall_rt_sigreturn(task: *mut Task, ctx_ptr: *mut UserContext) -> SyscallDisposition {
    if ctx_ptr.is_null() {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    }
    let Some(ctx) = SyscallContext::from_user_context(task, unsafe { &mut *ctx_ptr }) else {
        return syscall_return_err(ctx_ptr, ERRNO_EINVAL);
    };

    let task_ref = match ctx.task_mut() {
        Some(t) => t,
        None => return ctx.err_with(ERRNO_EINVAL),
    };

    // After the handler's `ret` pops the restorer address, RSP points
    // directly at the SignalFrame.  Read it from there.
    let rsp = ctx.user_ctx().rsp();
    let sigframe = match read_signal_frame(rsp) {
        Some(sf) => sf,
        None => return ctx.err_with(ERRNO_EFAULT),
    };

    task_ref.signal_blocked = sigframe.saved_mask & !SIG_UNCATCHABLE;

    // Rebuild the user GPR snapshot from the SignalFrame and commit it
    // through `set_regs`, which re-applies the user-CS/SS selectors and
    // RFLAGS sensitive-bit mask — user code cannot escape the sandbox
    // by crafting a sigframe with IOPL/AC/NT/VM/IF=0 set.
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

    ctx.ok(0)
}

pub fn deliver_pending_signal(task: *mut Task, ctx_ptr: *mut UserContext) {
    if task.is_null() || ctx_ptr.is_null() {
        return;
    }

    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }

        let pending = (*task).signal_pending.load(Ordering::Acquire);
        let deliverable = pending & !(*task).signal_blocked;
        if deliverable == 0 {
            return;
        }

        let signum = (deliverable.trailing_zeros() + 1) as u8;
        let bit = sig_bit(signum);
        (*task).signal_pending.fetch_and(!bit, Ordering::AcqRel);

        let action = (*task).signal_actions[(signum - 1) as usize];
        if action.handler == SIG_IGN {
            return;
        }

        if action.handler == SIG_DFL {
            match sig_default_action(signum) {
                SigDefault::Ignore | SigDefault::Stop | SigDefault::Continue => return,
                SigDefault::Terminate => {
                    let task_id = (*task).task_id;
                    (*task).exit_reason = TaskExitReason::Normal;
                    (*task).fault_reason = TaskFaultReason::None;
                    (*task).exit_code = 128 + signum as u32;
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

        // Snapshot of pre-delivery user registers — same struct that
        // `rt_sigreturn` will rebuild the user state from later.
        let regs_snapshot = *(*ctx_ptr).regs();

        // Linux convention: push the restorer address as a separate word
        // on the stack BEFORE the SignalFrame.  When the handler does
        // `ret`, it pops the restorer into RIP and RSP advances to point
        // at the SignalFrame, which rt_sigreturn reads directly.
        //
        // Stack layout (low address → high address):
        //   [frame_addr + 0]  = restorer address   (popped by `ret`)
        //   [frame_addr + 8]  = SignalFrame { signum, rax, … }
        let total_size = 8 + core::mem::size_of::<SignalFrame>() as u64;
        let frame_addr = regs_snapshot.rsp.wrapping_sub(total_size) & !0xF;

        // Write restorer as a separate u64 at frame_addr.
        let restorer_ptr = match UserPtr::<u64>::try_new(frame_addr) {
            Ok(p) => p,
            Err(_) => return,
        };
        if copy_to_user(restorer_ptr, &action.restorer).is_err() {
            return;
        }

        // Write SignalFrame at frame_addr + 8.
        let sigframe_addr = frame_addr.wrapping_add(8);
        let sigframe_ptr = match UserPtr::<SignalFrame>::try_new(sigframe_addr) {
            Ok(p) => p,
            Err(_) => return,
        };

        let saved_mask = (*task).signal_blocked;
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
        (*task).signal_blocked = blocked & !SIG_UNCATCHABLE;

        // Install the handler's entry state on the user context: redirect
        // RIP/RSP into the handler with `signum` in RDI, RSI/RDX zeroed.
        // `set_regs` reapplies CS/SS/RFLAGS-mask so a malicious caller
        // cannot smuggle in a forged user-RFLAGS via `regs_snapshot`.
        let mut regs = regs_snapshot;
        regs.rsp = frame_addr;
        regs.rip = action.handler;
        regs.rdi = signum as u64;
        regs.rsi = 0;
        regs.rdx = 0;
        (*ctx_ptr).set_regs(regs);
    }
}
