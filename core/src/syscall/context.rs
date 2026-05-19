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
    INVALID_PROCESS_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_SYSTEM,
};
use slopos_ostd::KArc;
use slopos_ostd::mm::vm_space::VmSpace;
use slopos_ostd::user::context::UserContext;
use slopos_ostd::wl_currency::{self, WL_DELTA};
use slopos_sched::task::{
    task_borrow, task_borrow_mut, task_flags as task_flags_of, task_id_of, task_process_id,
};
use slopos_sched::task_struct::Task;

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

pub struct SyscallContext {
    task_ptr: *mut Task,
    user_ctx_ptr: *mut UserContext,
    regs: SyscallRegs,
}

impl SyscallContext {
    /// Construct from a live `UserContext` reference. Used by the
    /// dispatch entry point.
    pub fn from_user_context(task: *mut Task, ctx: &mut UserContext) -> Option<Self> {
        let r = ctx.regs();
        let regs: SyscallRegs = [r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9];
        Some(Self {
            task_ptr: task,
            user_ctx_ptr: ctx as *mut UserContext,
            regs,
        })
    }

    /// Test-only constructor — used by `core/src/syscall/tests.rs`.
    /// Builds a `SyscallContext` directly from a caller-owned
    /// `UserContext`, identical to the dispatch-path constructor.
    #[doc(hidden)]
    pub fn from_test_frame(task: *mut Task, ctx: &mut UserContext) -> Option<Self> {
        Self::from_user_context(task, ctx)
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

    #[inline]
    pub fn task_ptr(&self) -> *mut Task {
        self.task_ptr
    }

    #[inline]
    pub fn has_task(&self) -> bool {
        !self.task_ptr.is_null()
    }

    #[inline]
    pub fn task(&self) -> Option<&Task> {
        task_borrow(self.task_ptr)
    }

    #[inline]
    pub fn task_mut(&self) -> Option<&mut Task> {
        task_borrow_mut(self.task_ptr)
    }

    #[inline]
    pub fn task_id(&self) -> Option<u32> {
        task_id_of(self.task_ptr)
    }

    #[inline]
    pub fn process_id(&self) -> Option<u32> {
        task_process_id(self.task_ptr)
    }

    #[inline]
    pub fn require_task(&self) -> Result<(), Errno> {
        if self.task_ptr.is_null() {
            Err(Errno::ESRCH)
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, Errno> {
        self.task_id().ok_or(Errno::ESRCH)
    }

    #[inline]
    pub fn require_process_id(&self) -> Result<u32, Errno> {
        match self.process_id() {
            Some(pid) if pid != INVALID_PROCESS_ID => Ok(pid),
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
        match task_flags_of(self.task_ptr) {
            Some(f) => f & flag != 0,
            None => false,
        }
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
    /// `capable(CAP_SYS_TTY_CONFIG)`. Uses `TASK_FLAG_SYSTEM` as the
    /// SlopOS equivalent until a proper capability bitfield exists.
    #[inline]
    pub fn is_console_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_SYSTEM)
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

    // ── Full user-context access (used by exec / fork / clone /
    // rt_sigreturn — whole-frame manipulation, NOT argument parsing).
    // ────────────────────────────────────────────────────────────────

    #[inline]
    pub fn user_ctx_ptr(&self) -> *mut UserContext {
        self.user_ctx_ptr
    }

    #[inline]
    pub fn user_ctx(&self) -> &UserContext {
        UserContext::from_ptr(self.user_ctx_ptr).expect("syscall: user_ctx_ptr null")
    }

    /// Mutable view of the per-task user-mode register snapshot the
    /// syscall handler is operating on. Lifetime is the borrow of
    /// `self`; callers must not retain it across nested handler
    /// dispatch.
    #[inline]
    pub fn user_ctx_mut(&self) -> &mut UserContext {
        UserContext::from_ptr_mut(self.user_ctx_ptr).expect("syscall: user_ctx_ptr null")
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
        if let Some(uc) = UserContext::from_ptr_mut(self.user_ctx_ptr) {
            uc.set_rax(value);
        }
    }

    /// Write an errno return value to user `rax`. Decrements the
    /// `wl_currency` balance.
    pub fn write_err(&self, errno: Errno) {
        wl_currency::adjust_balance(-WL_DELTA);
        if let Some(uc) = UserContext::from_ptr_mut(self.user_ctx_ptr) {
            uc.set_rax(errno.as_u64());
        }
    }

    /// Write a raw u64 (used for the `ERRNO_ERESTARTSYS` sentinel that
    /// is outside the `[-4095, -1]` `Errno` range).
    pub fn write_err_u64(&self, raw: u64) {
        wl_currency::adjust_balance(-WL_DELTA);
        if let Some(uc) = UserContext::from_ptr_mut(self.user_ctx_ptr) {
            uc.set_rax(raw);
        }
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
