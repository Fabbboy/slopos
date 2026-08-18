//! Per-syscall context.
//!
//! Built once by [`crate::syscall::dispatch::syscall_handle`] at syscall entry.
//! Handler bodies receive `&SyscallContext` and typed arguments; the full
//! [`UserContext`] is exposed for the handlers that manipulate the whole frame
//! (`exec`, `fork`, `clone`, `rt_sigreturn`).

use slopos_abi::Errno;
use slopos_abi::task::{
    TASK_FLAG_COMPOSITOR, TASK_FLAG_CONSOLE_ADMIN, TASK_FLAG_DISPLAY_EXCLUSIVE,
    TASK_FLAG_NET_ADMIN, TASK_FLAG_PROC_ADMIN, TASK_FLAG_SYSTEM,
};
use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::wl_currency::{self, WL_DELTA};
use slopos_sched::task::TaskRef;
use slopos_sched::task_struct::{Current, Task};

use crate::syscall::result::SyscallResult;

/// Six-register argument payload, snapshotted at syscall entry: `rdi`, `rsi`,
/// `rdx`, `r10`, `r8`, `r9` — System V AMD64 with `r10` standing in for `rcx`,
/// which the `syscall` instruction clobbers.
pub type SyscallRegs = [u64; 6];

/// The calling task, its arguments, and its user-mode frame, for the duration
/// of one syscall. The task is a **borrow**: it is alive because it is
/// executing this code.
pub struct SyscallContext<'a> {
    task: &'a Task,
    task_id: u32,
    user_ctx: &'a UserContext,
    regs: SyscallRegs,
    /// Both borrows belong to one CPU; without this the context would be
    /// auto-`Send + Sync`.
    _not_send: core::marker::PhantomData<*const ()>,
}

impl<'a> SyscallContext<'a> {
    /// Build a context for the task this CPU is running. Borrowing the guard
    /// rather than storing it ties the context's lifetime to it.
    pub fn from_current(current: &'a Current, ctx: &'a UserContext) -> Self {
        Self::new(current.task(), current.id(), ctx)
    }

    /// Build a context from a registry guard. Tests only: the fixture parks the
    /// BSP on a pre-heap stub where `Current::get()` returns `None`, so the
    /// production constructor is unusable there.
    #[doc(hidden)]
    pub fn from_task_ref(task: &'a TaskRef, ctx: &'a UserContext) -> Self {
        Self::new(task, task.task_id, ctx)
    }

    #[inline]
    fn new(task: &'a Task, task_id: u32, ctx: &'a UserContext) -> Self {
        // Snapshot the argument registers once: signal delivery or a restart
        // decision may rewrite the user context before a handler reads them.
        Self {
            task,
            task_id,
            regs: ctx.syscall_args(),
            user_ctx: ctx,
            _not_send: core::marker::PhantomData,
        }
    }

    /// Macro-internal hook for `define_syscall!`; handler bodies take typed
    /// args through `SyscallArg::from_raw` instead.
    #[inline]
    pub fn regs(&self) -> &SyscallRegs {
        &self.regs
    }

    #[inline]
    pub fn task(&self) -> &'a Task {
        self.task
    }

    #[inline]
    pub fn task_id(&self) -> u32 {
        self.task_id
    }

    /// The calling task's process id, or `INVALID_PROCESS_ID` for a task bound
    /// to no process. ABI only — acting on the caller's process needs
    /// [`require_process`](Self::require_process), which id recycling cannot
    /// confuse.
    #[inline]
    pub fn process_id(&self) -> u32 {
        self.task.process_id
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, Errno> {
        Ok(self.task_id)
    }

    /// The calling process's descriptor table, read from the task's own
    /// generation-checked handle rather than resolved from its id. `ESRCH` for
    /// a kernel task — never [`FdTable::Kernel`], whose descriptors every
    /// kernel task shares.
    #[inline]
    pub fn require_process(&self) -> Result<FdTable, Errno> {
        self.task
            .process()
            .as_deref()
            .and_then(FdTable::of)
            .ok_or(Errno::ESRCH)
    }

    pub fn vm_space(&self) -> Result<KArc<VmSpace>, Errno> {
        let vm_process = self.require_process()?.process().ok_or(Errno::ESRCH)?;
        slopos_mm::process_vm::process_vm_get_vm_space(vm_process).ok_or(Errno::EFAULT)
    }

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

    /// Console-administration privilege, modelled on Linux's
    /// `capable(CAP_SYS_TTY_CONFIG)`. Conferred by path identity on
    /// `/bin/keymap`; `TASK_FLAG_SYSTEM` implies it.
    #[inline]
    pub fn is_console_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_SYSTEM) || self.has_flag(TASK_FLAG_CONSOLE_ADMIN)
    }

    /// Network-administration privilege, modelled on Linux's
    /// `capable(CAP_NET_ADMIN)`. Conferred by path identity at spawn.
    #[inline]
    pub fn is_net_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_NET_ADMIN)
    }

    /// Whole-machine enumeration privilege, modelled on Linux's `hidepid`
    /// bypass. Conferred by path identity on `/bin/sysmon`; `TASK_FLAG_SYSTEM`
    /// implies it.
    #[inline]
    pub fn is_proc_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_SYSTEM) || self.has_flag(TASK_FLAG_PROC_ADMIN)
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

    /// The user-mode register file this syscall entered from. Writes go
    /// through it directly — see the write contract on [`UserContext`].
    #[inline]
    pub fn user_ctx(&self) -> &'a UserContext {
        self.user_ctx
    }

    #[inline]
    pub fn user_rsp(&self) -> u64 {
        self.user_ctx().rsp()
    }

    // The only sites that write `rax`: the dispatcher matches on
    // `SyscallResult` and calls one; handler bodies never invoke them.

    /// Write a successful return value to user `rax`, bumping the
    /// `wl_currency` balance.
    pub fn write_ok(&self, value: u64) {
        wl_currency::adjust_balance(WL_DELTA);
        self.user_ctx.set_rax(value);
    }

    /// Write an errno return value to user `rax`, decrementing the
    /// `wl_currency` balance.
    pub fn write_err(&self, errno: Errno) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(errno.as_u64());
    }

    /// Write a raw u64, for the `ERRNO_ERESTARTSYS` sentinel that lies outside
    /// the `[-4095, -1]` `Errno` range.
    pub fn write_err_u64(&self, raw: u64) {
        wl_currency::adjust_balance(-WL_DELTA);
        self.user_ctx.set_rax(raw);
    }

    /// Write the post-handler `SyscallResult`; `NoReturn` leaves `rax` alone.
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
