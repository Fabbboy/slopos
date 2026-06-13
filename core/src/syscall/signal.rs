use core::ffi::c_void;
use core::sync::atomic::Ordering;

use slopos_abi::Errno;
use slopos_abi::signal::{
    NSIG, SA_NODEFER, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIG_UNCATCHABLE, SIGKILL,
    SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit, sig_default_action,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE, TaskExitReason, TaskFaultReason};
use slopos_mm::user_copy::{
    copy_bytes_from_user, copy_bytes_to_user, copy_from_user, copy_to_user,
};
use slopos_mm::user_ptr::{UserBytes, UserPtr as MmUserPtr};
use slopos_ostd::irq::InterruptFrame;
use slopos_ostd::task::FPU_STATE_SIZE;
use slopos_ostd::task::accessors::task_fpu_state_mut;
use slopos_ostd::user::context::{
    USER_RFLAGS_FORCED, USER_RFLAGS_PERMITTED, UserContext, UserRegs,
};

use crate::syscall::args::{Signum, UserPtr};
use crate::syscall::result::SyscallResult;
use slopos_sched::scheduler::{schedule, unblock_task};
use slopos_sched::task::{
    task_find_by_id, task_id_of, task_iterate_active, task_pgid, task_signal_post, task_terminate,
};
use slopos_sched::task_struct::{SignalAction, Task};
use slopos_sched::trap::trap_running_on_exception_stack;

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

        // POSIX: kill() succeeds even when the disposition discards the
        // signal — only the wake is skipped for a send-time drop.
        if task_signal_post(target, signum) {
            let _ = unblock_task(target);
        }
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

/// The FPU/vector save area sits immediately after the `SignalFrame` on
/// the user stack; delivery writes it and sigreturn reads it back from
/// the same offset. (The `SignalFrame` ABI is unchanged — this area is
/// kernel-internal and the userland restorer never touches it.)
#[inline]
fn sigframe_fpu_addr(sigframe_addr: u64) -> u64 {
    sigframe_addr.wrapping_add(core::mem::size_of::<SignalFrame>() as u64)
}

/// Save the interrupted task's live FPU/SSE/AVX state into its user
/// signal frame so sigreturn can restore it — a handler that touches the
/// vector registers must not corrupt the interrupted code's state. The
/// kernel is `+soft-float` and has not touched the vector file since
/// entry, so the live CPU state is still the interrupted user's. Returns
/// false on a user-copy fault.
fn save_fpu_to_sigframe(task: *mut Task, sigframe_addr: u64) -> bool {
    let Some(fpu) = task_fpu_state_mut(task) else {
        return false;
    };
    fpu.save_current(slopos_ostd::cpu::x86_64::xsave::active_xcr0());
    let Ok(bytes) = UserBytes::try_new(sigframe_fpu_addr(sigframe_addr), FPU_STATE_SIZE) else {
        return false;
    };
    copy_bytes_to_user(bytes, &fpu.data).is_ok()
}

/// Restore the FPU/vector state saved by [`save_fpu_to_sigframe`]. The
/// copy-in and `xrstor` run under an IRQ-off critical section so a
/// context switch cannot overwrite the task's `fpu_state` slot between
/// the two steps (the scheduler also saves/restores that same slot).
/// Returns false on a user-copy fault, leaving the prior FPU state.
fn restore_fpu_from_sigframe(task: *mut Task, sigframe_addr: u64) -> bool {
    let Ok(bytes) = UserBytes::try_new(sigframe_fpu_addr(sigframe_addr), FPU_STATE_SIZE) else {
        return false;
    };
    let xcr0 = slopos_ostd::cpu::x86_64::xsave::active_xcr0();
    slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
        let Some(fpu) = task_fpu_state_mut(task) else {
            return false;
        };
        if copy_bytes_from_user(bytes, &mut fpu.data).is_err() {
            return false;
        }
        fpu.restore_to_cpu(xcr0);
        true
    })
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

    // Restore the FPU/vector state saved at delivery (best-effort: a
    // faulted save area leaves the current vector registers in place).
    let _ = restore_fpu_from_sigframe(ctx.task_ptr(), rsp);

    // sigreturn fully replaced the user-mode register state — the
    // dispatcher must not overwrite RAX after we return.
    SyscallResult::NoReturn
});

/// Read/write view over the user-mode register file that signal
/// delivery mutates. The two delivery sites — the syscall-exit path
/// ([`UserContext`]) and the IRQ-exit path ([`InterruptFrame`]) — share
/// [`deliver_pending_signal_core`] through this trait so the frame
/// layout, RFLAGS masking, and redirect logic stay in lockstep.
trait UserRegView {
    /// Snapshot the GPR file as a `UserRegs`, with `rflags_user_subset`
    /// already masked to the user-permitted bits so the value stored in
    /// the `SignalFrame` matches across both delivery sites.
    fn snapshot(&self) -> UserRegs;

    /// Commit a redirected register file (handler entry: new RSP/RIP +
    /// signum in RDI). Re-applies RFLAGS masking and the user CS/SS
    /// selectors so a redirect can never escape the sandbox.
    fn commit_redirect(&mut self, regs: &UserRegs);
}

impl UserRegView for UserContext {
    fn snapshot(&self) -> UserRegs {
        *self.regs()
    }

    fn commit_redirect(&mut self, regs: &UserRegs) {
        self.set_regs(*regs);
    }
}

/// IRQ-exit register view over the CPU-pushed [`InterruptFrame`]. The
/// `iretq` that resumes user mode loads RIP/RSP/RFLAGS from this frame,
/// so redirecting user execution at IRQ exit means mutating it in place.
struct InterruptFrameRegs<'a> {
    frame: &'a mut InterruptFrame,
}

impl UserRegView for InterruptFrameRegs<'_> {
    fn snapshot(&self) -> UserRegs {
        let f = &*self.frame;
        UserRegs {
            rax: f.rax,
            rbx: f.rbx,
            rcx: f.rcx,
            rdx: f.rdx,
            rsi: f.rsi,
            rdi: f.rdi,
            rbp: f.rbp,
            rsp: f.rsp,
            r8: f.r8,
            r9: f.r9,
            r10: f.r10,
            r11: f.r11,
            r12: f.r12,
            r13: f.r13,
            r14: f.r14,
            r15: f.r15,
            rip: f.rip,
            rflags_user_subset: f.rflags & USER_RFLAGS_PERMITTED,
            fs_base: 0,
            gs_base: 0,
            cs: f.cs as u16,
            ss: f.ss as u16,
            _pad: [0; 3],
        }
    }

    fn commit_redirect(&mut self, regs: &UserRegs) {
        let f = &mut *self.frame;
        f.rax = regs.rax;
        f.rbx = regs.rbx;
        f.rcx = regs.rcx;
        f.rdx = regs.rdx;
        f.rsi = regs.rsi;
        f.rdi = regs.rdi;
        f.rbp = regs.rbp;
        f.rsp = regs.rsp;
        f.r8 = regs.r8;
        f.r9 = regs.r9;
        f.r10 = regs.r10;
        f.r11 = regs.r11;
        f.r12 = regs.r12;
        f.r13 = regs.r13;
        f.r14 = regs.r14;
        f.r15 = regs.r15;
        f.rip = regs.rip;
        f.rflags = (regs.rflags_user_subset & USER_RFLAGS_PERMITTED) | USER_RFLAGS_FORCED;
    }
}

/// Shared signal-delivery core driven by both the syscall-exit and the
/// IRQ-exit paths. Pulls the lowest deliverable signal, runs its
/// disposition (ignore / default-terminate / user handler), and on the
/// handler path writes the restorer + `SignalFrame` to the user stack
/// and redirects `regs` to the handler.
///
/// On ANY `copy_to_user` failure the pending bit is re-armed
/// (`fetch_or`) so the signal retries at the next delivery point rather
/// than being silently dropped.
fn deliver_pending_signal_core(task: *mut Task, regs: &mut impl UserRegView) {
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

    let regs_snapshot = regs.snapshot();

    // Frame = [restorer ptr (8)] [SignalFrame] [FPU/vector save area].
    //
    // The restorer pointer at `frame_addr` doubles as the handler's
    // return address, so entering the handler is ABI-equivalent to a
    // `call`: SysV requires `(rsp + 8) % 16 == 0` at a function's first
    // instruction, i.e. `frame_addr % 16 == 8`. Aligning `frame_addr`
    // to 16 (the obvious choice) leaves the handler's stack misaligned
    // by 8, so any aligned vector spill (`vmovaps [rsp], …`) the handler
    // emits faults with #GP. Subtract 8 after the 16-byte floor.
    let total_size = 8 + core::mem::size_of::<SignalFrame>() as u64 + FPU_STATE_SIZE as u64;
    let frame_addr = (regs_snapshot.rsp.wrapping_sub(total_size) & !0xF).wrapping_sub(8);

    let restorer_ptr = match MmUserPtr::<u64>::try_new(frame_addr) {
        Ok(p) => p,
        Err(_) => {
            task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
            return;
        }
    };
    if copy_to_user(restorer_ptr, &action.restorer).is_err() {
        task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
        return;
    }

    let sigframe_addr = frame_addr.wrapping_add(8);
    let sigframe_ptr = match MmUserPtr::<SignalFrame>::try_new(sigframe_addr) {
        Ok(p) => p,
        Err(_) => {
            task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
            return;
        }
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
        task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
        return;
    }

    // Preserve the interrupted task's vector state across the handler.
    if !save_fpu_to_sigframe(task, sigframe_addr) {
        task_ref.signal_pending.fetch_or(bit, Ordering::AcqRel);
        return;
    }

    let mut blocked = saved_mask | action.mask;
    if (action.flags & SA_NODEFER) == 0 {
        blocked |= bit;
    }
    task_ref.signal_blocked = blocked & !SIG_UNCATCHABLE;

    let mut redirected = regs_snapshot;
    redirected.rsp = frame_addr;
    redirected.rip = action.handler;
    redirected.rdi = signum as u64;
    redirected.rsi = 0;
    redirected.rdx = 0;
    regs.commit_redirect(&redirected);
}

pub fn deliver_pending_signal(task: *mut Task, ctx_ptr: *mut UserContext) {
    let Some(user_ctx) = UserContext::from_ptr_mut(ctx_ptr) else {
        return;
    };
    deliver_pending_signal_core(task, user_ctx);
}

/// Deliver a pending signal on the IRQ/timer/IPI return-to-user path.
///
/// Linux checks `TIF_SIGPENDING` on every return to user, including IRQ
/// exit; without this a user task spinning in pure userspace (no
/// syscalls) would never act on a pending signal and would be
/// unkillable. The `iretq` that resumes the interrupted task loads its
/// register state from `frame`, so a handler redirect mutates that
/// frame in place.
///
/// Guards (any failing → no-op): non-null frame, frame returning to
/// user (`cs & 3 == 3`), a current user-mode task, and NOT running on an
/// IST/exception stack — exception vectors 0-31 run under
/// `IstPreemptHold` and must never deliver signals here.
pub fn deliver_pending_signal_on_irq_exit(frame: *mut InterruptFrame) {
    let Some(frame_ref) = InterruptFrame::from_ptr_mut(frame) else {
        return;
    };
    if (frame_ref.cs & 3) != 3 {
        return;
    }
    if trap_running_on_exception_stack() {
        return;
    }

    let task = slopos_sched::scheduler::scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        return;
    }

    let mut view = InterruptFrameRegs { frame: frame_ref };
    deliver_pending_signal_core(task, &mut view);
}

#[allow(dead_code)]
type _Unused<T> = UserPtr<T>;
