//! Per-syscall context.
//!
//! Built once by [`crate::syscall::dispatch::syscall_handle`] when a
//! syscall enters the kernel. Handler bodies receive `&SyscallContext`
//! and never touch raw register state for argument parsing — typed
//! arguments are decoded by the macro from `ctx.regs()`. The context
//! also exposes the active task, the calling process's
//! [`slopos_ostd::mm::vm_space::VmSpace`], and the full
//! [`UserContext`] for the few handlers that perform whole-frame
//! manipulation (`exec`, `fork`, `clone`, `rt_sigreturn`).

use slopos_abi::Errno;
use slopos_abi::task::{
    INVALID_PROCESS_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_CONSOLE_ADMIN, TASK_FLAG_DISPLAY_EXCLUSIVE,
    TASK_FLAG_NET_ADMIN, TASK_FLAG_SYSTEM,
};
use slopos_ostd::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::wl_currency::{self, WL_DELTA};
use slopos_sched::task::TaskRef;
use slopos_sched::task_struct::{Current, Task};

use crate::syscall::result::SyscallResult;

/// Six-register argument payload, snapshotted at syscall entry.
///
/// Index conventions (System V AMD64, with `R10` standing in for
/// `RCX`, which the `syscall` instruction clobbers):
///
/// | Index | Register |
/// |-------|----------|
/// | 0     | `rdi`    |
/// | 1     | `rsi`    |
/// | 2     | `rdx`    |
/// | 3     | `r10`    |
/// | 4     | `r8`     |
/// | 5     | `r9`     |
pub type SyscallRegs = [u64; 6];

/// The calling task, its arguments, and its user-mode frame, for the duration
/// of one syscall.
///
/// The task is a **borrow**, not a pointer and not an owning handle. A pointer
/// carried no lifetime, so every accessor had to re-derive a reference and hope;
/// an owning handle would be worse, because SlopOS tears a blocked task down
/// from another CPU without unwinding, so a `KArc` left on this frame would
/// never be dropped and would pin the task, its stacks and its address space
/// forever. A borrow is exactly the claim the syscall path can honestly make:
/// the task is alive because it is the one executing this code.
pub struct SyscallContext<'a> {
    task: &'a Task,
    task_id: u32,
    user_ctx: &'a UserContext,
    regs: SyscallRegs,
    /// A context describes one syscall on one CPU, and both borrows it
    /// holds are that CPU's. Dropping the raw pointer this struct used to
    /// carry would otherwise have made it auto-`Send + Sync`.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl<'a> SyscallContext<'a> {
    /// Build a context for the task this CPU is running. The production
    /// dispatch path.
    ///
    /// The guard is borrowed rather than stored: `CurrentTask` is `!Send` and
    /// carries its own meaning (this CPU is running this task), whereas what a
    /// handler needs is just the task. Taking `&'a Current` ties the context's
    /// lifetime to the guard, which is what makes the borrow inside sound.
    pub fn from_current(current: &'a Current, ctx: &'a UserContext) -> Self {
        Self::new(current.task(), current.id(), ctx)
    }

    /// Build a context from a registry guard. Tests only.
    ///
    /// Not a convenience: the kernel test fixture parks the BSP on a
    /// pre-heap bootstrap stub, so `Current::get()` returns `None` there and
    /// the production constructor is unusable. That, rather than any staleness
    /// concern, is why a witness cannot simply be stored in this struct.
    #[doc(hidden)]
    pub fn from_task_ref(task: &'a TaskRef, ctx: &'a UserContext) -> Self {
        Self::new(task, task.task_id, ctx)
    }

    #[inline]
    fn new(task: &'a Task, task_id: u32, ctx: &'a UserContext) -> Self {
        // Snapshot the argument registers once: a handler reads them
        // through `SyscallArg::from_raw` long after the user context may
        // have been rewritten by signal delivery or a restart decision.
        Self {
            task,
            task_id,
            regs: ctx.syscall_args(),
            user_ctx: ctx,
            _not_send: core::marker::PhantomData,
        }
    }

    // ── Raw argument payload (macro-internal) ─────────────────────────

    /// The macro-internal hook used by `define_syscall!` to thread
    /// argument parsing across `SyscallArg::from_raw` calls. Not for
    /// hand-written handler bodies — bodies never reach for raw
    /// register slots; typed args flow through `SyscallArg::from_raw`.
    #[inline]
    pub fn regs(&self) -> &SyscallRegs {
        &self.regs
    }

    // ── Task / process / VM space accessors ───────────────────────────

    /// The calling task. Infallible: a context cannot exist without one.
    #[inline]
    pub fn task(&self) -> &'a Task {
        self.task
    }

    /// The calling task's registry id.
    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task_id
    }

    /// The calling task's process id, or `INVALID_PROCESS_ID` for a task bound
    /// to no process. Use [`require_process_id`](Self::require_process_id) when
    /// the handler needs a real one.
    #[inline]
    pub fn process_id(&self) -> u32 {
        self.task.process_id
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, Errno> {
        Ok(self.task_id)
    }

    #[inline]
    pub fn require_process_id(&self) -> Result<u32, Errno> {
        match self.process_id() {
            pid if pid != INVALID_PROCESS_ID => Ok(pid),
            _ => Err(Errno::ESRCH),
        }
    }

    /// Resolve the caller's [`VmSpace`]. Returns `EFAULT` if the
    /// caller is bound to no process or the process has no VM space.
    pub fn vm_space(&self) -> Result<KArc<VmSpace>, Errno> {
        let pid = self.require_process_id()?;
        slopos_mm::process_vm::process_vm_get_vm_space(pid).ok_or(Errno::EFAULT)
    }

    // ── Permission checks ─────────────────────────────────────────────

    #[inline]
    pub fn has_flag(&self, flag: u16) -> bool {
        self.task.flags & flag != 0
    }

    #[inline]
    pub fn is_compositor(&self) -> bool {
        self.has_flag(TASK_FLAG_COMPOSITOR)
    }

    #[inline]
    pub fn is_display_exclusive(&self) -> bool {
        self.has_flag(TASK_FLAG_DISPLAY_EXCLUSIVE)
    }

    /// Console-administration privilege — modelled on Linux's
    /// `capable(CAP_SYS_TTY_CONFIG)`. Conferred by path identity on
    /// `/bin/keymap`; `TASK_FLAG_SYSTEM` implies it, the way root implies
    /// every capability, so init still applies the persisted layout at boot.
    #[inline]
    pub fn is_console_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_SYSTEM) || self.has_flag(TASK_FLAG_CONSOLE_ADMIN)
    }

    /// Network-administration privilege — modelled on Linux's
    /// `capable(CAP_NET_ADMIN)`. Conferred by path identity at spawn, so it
    /// names the one program allowed to reconfigure the stack rather than a
    /// class of callers.
    #[inline]
    pub fn is_net_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_NET_ADMIN)
    }

    #[inline]
    pub fn require_compositor(&self) -> Result<(), Errno> {
        if self.is_compositor() {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    #[inline]
    pub fn require_display_exclusive(&self) -> Result<(), Errno> {
        if self.is_display_exclusive() {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    #[inline]
    pub fn require_console_admin(&self) -> Result<(), Errno> {
        if self.is_console_admin() {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    #[inline]
    pub fn require_net_admin(&self) -> Result<(), Errno> {
        if self.is_net_admin() {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }

    // ── Full user-context access (used by exec / fork / clone /
    // rt_sigreturn — whole-frame manipulation, NOT argument parsing).
    // ────────────────────────────────────────────────────────────────

    /// The user-mode register file this syscall entered from. Writes go
    /// through it directly — see the write contract on [`UserContext`].
    #[inline]
    pub fn user_ctx(&self) -> &'a UserContext {
        self.user_ctx
    }

    /// User-mode RSP at syscall entry. Convenience for `rt_sigreturn`
    /// (and any future handler that needs to peek the user stack
    /// pointer without rebuilding the whole frame view).
    #[inline]
    pub fn user_rsp(&self) -> u64 {
        self.user_ctx().rsp()
    }

    // ── Dispatcher-only return-value writers ──────────────────────────
    //
    // These are the **only** sites that write `rax`. The dispatcher
    // matches on `SyscallResult` and calls one of these; handler
    // bodies never invoke them directly.

    /// Write a successful return value to user `rax`. Bumps the
    /// `wl_currency` balance, mirroring the pre-Phase-2D
    /// `ctx.ok(value)` accounting.
    pub fn write_ok(&self, value: u64) {
        wl_currency::adjust_balance(WL_DELTA);
        self.user_ctx.set_rax(value);
    }

    /// Write an errno return value to user `rax`. Decrements the
    /// `wl_currency` balance.
    pub fn write_err(&self, errno: Errno) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(errno.as_u64());
    }

    /// Write a raw u64 (used for the `ERRNO_ERESTARTSYS` sentinel that
    /// is outside the `[-4095, -1]` `Errno` range).
    pub fn write_err_u64(&self, raw: u64) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(raw);
    }

    /// Convenience: write the post-handler `SyscallResult` directly.
    /// `NoReturn` leaves `rax` untouched.
    pub fn write_result(&self, result: SyscallResult) {
        match result {
            SyscallResult::Ok(v) => self.write_ok(v),
            SyscallResult::Err(e) => {
                if e == Errno::ERESTARTSYS {
                    self.write_err_u64(slopos_abi::syscall::ERRNO_ERESTARTSYS);
                } else {
                    self.write_err(e);
                }
            }
            SyscallResult::NoReturn => {}
        }
    }
}
