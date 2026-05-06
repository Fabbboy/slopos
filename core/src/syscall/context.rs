use crate::scheduler::task_struct::Task;
use crate::syscall::common::{SyscallDisposition, syscall_return_err, syscall_return_ok};
use slopos_abi::syscall::{ERRNO_EFAULT, ERRNO_EINVAL};
use slopos_abi::task::{
    INVALID_PROCESS_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_SYSTEM,
};
use slopos_ostd::user::context::UserContext;
use slopos_utils::wl_currency::{self, WL_DELTA};

#[derive(Clone, Copy)]
pub struct SyscallArgs {
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
}

pub struct SyscallContext {
    task_ptr: *mut Task,
    user_ctx_ptr: *mut UserContext,
    args: SyscallArgs,
}

impl SyscallContext {
    pub fn from_user_context(task: *mut Task, ctx: &mut UserContext) -> Option<Self> {
        let regs = ctx.regs();
        let args = SyscallArgs {
            arg0: regs.rdi,
            arg1: regs.rsi,
            arg2: regs.rdx,
            arg3: regs.r10,
            arg4: regs.r8,
            arg5: regs.r9,
        };

        Some(Self {
            task_ptr: task,
            user_ctx_ptr: ctx as *mut UserContext,
            args,
        })
    }

    #[inline]
    pub fn has_task(&self) -> bool {
        !self.task_ptr.is_null()
    }

    #[inline]
    pub fn task_id(&self) -> Option<u32> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { (*self.task_ptr).task_id })
        }
    }

    #[inline]
    pub fn process_id(&self) -> Option<u32> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { (*self.task_ptr).process_id })
        }
    }

    #[inline]
    pub fn has_flag(&self, flag: u16) -> bool {
        if self.task_ptr.is_null() {
            return false;
        }
        unsafe { (*self.task_ptr).flags & flag != 0 }
    }

    #[inline]
    pub fn is_compositor(&self) -> bool {
        self.has_flag(TASK_FLAG_COMPOSITOR)
    }

    #[inline]
    pub fn is_display_exclusive(&self) -> bool {
        self.has_flag(TASK_FLAG_DISPLAY_EXCLUSIVE)
    }

    /// Check if the calling task has console administration privilege.
    ///
    /// Modelled on Linux's `capable(CAP_SYS_TTY_CONFIG)` check in
    /// `drivers/tty/vt/vt_ioctl.c`.  Uses `TASK_FLAG_SYSTEM` as the
    /// SlopOS equivalent until a proper capability bitfield is added.
    #[inline]
    pub fn is_console_admin(&self) -> bool {
        self.has_flag(TASK_FLAG_SYSTEM)
    }

    #[inline]
    pub fn require_console_admin(&self) -> Result<(), SyscallDisposition> {
        if !self.is_console_admin() {
            Err(self.err_with(slopos_abi::Errno::EPERM.as_u64()))
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn args(&self) -> &SyscallArgs {
        &self.args
    }

    #[inline]
    pub fn user_ctx_ptr(&self) -> *mut UserContext {
        self.user_ctx_ptr
    }

    #[inline]
    pub fn user_ctx(&self) -> &UserContext {
        unsafe { &*self.user_ctx_ptr }
    }

    /// Mutable view of the per-task user-mode register snapshot the
    /// syscall handler is operating on. Returned reference's lifetime
    /// is the borrow of `self`; callers must not retain it across a
    /// reentrant `&mut UserContext` reborrow (e.g. nested handler dispatch).
    #[inline]
    pub fn user_ctx_mut(&self) -> &mut UserContext {
        unsafe { &mut *self.user_ctx_ptr }
    }

    #[inline]
    pub fn task_ptr(&self) -> *mut Task {
        self.task_ptr
    }

    #[inline]
    pub fn task_mut(&self) -> Option<&mut Task> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { &mut *self.task_ptr })
        }
    }

    #[inline]
    pub fn ok(&self, value: u64) -> SyscallDisposition {
        wl_currency::adjust_balance(WL_DELTA);
        syscall_return_ok(self.user_ctx_ptr, value)
    }

    #[inline]
    pub fn ok_i64(&self, value: i64) -> SyscallDisposition {
        self.ok(value as u64)
    }

    #[inline]
    pub fn err(&self) -> SyscallDisposition {
        self.err_with(ERRNO_EINVAL)
    }

    #[inline]
    pub fn err_with(&self, errno: u64) -> SyscallDisposition {
        wl_currency::adjust_balance(-WL_DELTA);
        syscall_return_err(self.user_ctx_ptr, errno)
    }

    #[inline]
    pub fn invalid_arg(&self) -> SyscallDisposition {
        self.err_with(ERRNO_EINVAL)
    }

    #[inline]
    pub fn bad_address(&self) -> SyscallDisposition {
        self.err_with(ERRNO_EFAULT)
    }

    #[inline]
    pub fn require_task(&self) -> Result<(), SyscallDisposition> {
        if self.task_ptr.is_null() {
            Err(self.err())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, SyscallDisposition> {
        self.task_id().ok_or_else(|| self.err())
    }

    #[inline]
    pub fn require_process_id(&self) -> Result<u32, SyscallDisposition> {
        match self.process_id() {
            Some(pid) if pid != INVALID_PROCESS_ID => Ok(pid),
            _ => Err(self.err()),
        }
    }

    #[inline]
    pub fn require_compositor(&self) -> Result<(), SyscallDisposition> {
        if !self.is_compositor() {
            Err(self.err())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn require_display_exclusive(&self) -> Result<(), SyscallDisposition> {
        if !self.is_display_exclusive() {
            Err(self.err())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn check_result(&self, result: i32) -> Result<(), SyscallDisposition> {
        if result != 0 { Err(self.err()) } else { Ok(()) }
    }

    #[inline]
    pub fn check_negative(&self, result: i32) -> Result<(), SyscallDisposition> {
        if result < 0 { Err(self.err()) } else { Ok(()) }
    }

    #[inline]
    pub fn err_user_ptr(&self, _err: slopos_mm::user_ptr::UserPtrError) -> SyscallDisposition {
        self.err_with(ERRNO_EFAULT)
    }

    #[inline]
    pub fn check_user_ptr<T>(
        &self,
        result: Result<T, slopos_mm::user_ptr::UserPtrError>,
    ) -> Result<T, SyscallDisposition> {
        result.map_err(|e| self.err_user_ptr(e))
    }

    // =========================================================================
    // Result conversion helpers - eliminate boilerplate patterns
    // =========================================================================

    /// Convert a signed return code to a disposition.
    /// Returns `err()` if rc < 0, otherwise `ok(0)`.
    ///
    /// Replaces: `if rc < 0 { ctx.err() } else { ctx.ok(0) }`
    #[inline]
    pub fn from_rc(&self, rc: i32) -> SyscallDisposition {
        if rc < 0 { self.err() } else { self.ok(0) }
    }

    /// Convert a signed return code to a disposition, returning the value on success.
    /// Returns `err()` if rc < 0, otherwise `ok(rc as u64)`.
    ///
    /// Replaces: `if rc < 0 { ctx.err() } else { ctx.ok(rc as u64) }`
    #[inline]
    pub fn from_rc_value(&self, rc: i64) -> SyscallDisposition {
        if rc < 0 {
            self.err()
        } else {
            self.ok(rc as u64)
        }
    }

    /// Convert a token/handle value to a disposition.
    /// Returns `err()` if value == 0, otherwise `ok(value as u64)`.
    ///
    /// Replaces: `if token == 0 { ctx.err() } else { ctx.ok(token as u64) }`
    #[inline]
    pub fn from_token(&self, token: u32) -> SyscallDisposition {
        if token == 0 {
            self.err()
        } else {
            self.ok(token as u64)
        }
    }

    /// Convert a u64 address/value to a disposition.
    /// Returns `err()` if value == 0, otherwise `ok(value)`.
    ///
    /// Replaces: `if vaddr == 0 { ctx.err() } else { ctx.ok(vaddr) }`
    #[inline]
    pub fn from_nonzero(&self, value: u64) -> SyscallDisposition {
        if value == 0 {
            self.err()
        } else {
            self.ok(value)
        }
    }

    /// Convert a Result<(), E> to a disposition.
    /// Returns `err()` on Err, otherwise `ok(0)`.
    #[inline]
    pub fn from_result<E>(&self, result: Result<(), E>) -> SyscallDisposition {
        match result {
            Ok(()) => self.ok(0),
            Err(_) => self.err(),
        }
    }

    /// Convert a Result<T, E> to a disposition with a mapper for the success value.
    /// Returns `err()` on Err, otherwise `ok(f(value))`.
    #[inline]
    pub fn from_result_map<T, E, F>(&self, result: Result<T, E>, f: F) -> SyscallDisposition
    where
        F: FnOnce(T) -> u64,
    {
        match result {
            Ok(v) => self.ok(f(v)),
            Err(_) => self.err(),
        }
    }

    /// Convert a bool to a disposition.
    /// Returns `err()` if false, otherwise `ok(0)`.
    #[inline]
    pub fn from_bool(&self, success: bool) -> SyscallDisposition {
        if success { self.ok(0) } else { self.err() }
    }

    /// Convert a bool to a disposition with a custom success value.
    /// Returns `err()` if false, otherwise `ok(value)`.
    #[inline]
    pub fn from_bool_value(&self, success: bool, value: u64) -> SyscallDisposition {
        if success { self.ok(value) } else { self.err() }
    }

    /// Convert an i32 result where != 0 means failure (common for C-style APIs).
    /// Returns `err()` if rc != 0, otherwise `ok(0)`.
    ///
    /// Replaces: `if rc != 0 { ctx.err() } else { ctx.ok(0) }`
    #[inline]
    pub fn from_zero_success(&self, rc: i32) -> SyscallDisposition {
        if rc != 0 { self.err() } else { self.ok(0) }
    }
}

macro_rules! syscall_arg_accessors {
    ($($field:ident),+) => {
        $(
            paste::paste! {
                #[inline]
                pub fn [<$field _u32>](&self) -> u32 { self.$field as u32 }
                #[inline]
                pub fn [<$field _i32>](&self) -> i32 { self.$field as i32 }
                #[inline]
                pub fn [<$field _usize>](&self) -> usize { self.$field as usize }
                #[inline]
                pub fn [<$field _ptr>]<T>(&self) -> *mut T { self.$field as *mut T }
                #[inline]
                pub fn [<$field _const_ptr>]<T>(&self) -> *const T { self.$field as *const T }
            }
        )+
    };
}

impl SyscallArgs {
    syscall_arg_accessors!(arg0, arg1, arg2, arg3, arg4, arg5);
}
