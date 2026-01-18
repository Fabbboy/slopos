use crate::syscall::common::{SyscallDisposition, syscall_return_err, syscall_return_ok};
use slopos_abi::task::{
    INVALID_PROCESS_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, Task,
};
use slopos_lib::InterruptFrame;

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
    frame_ptr: *mut InterruptFrame,
    args: SyscallArgs,
}

impl SyscallContext {
    pub fn new(task: *mut Task, frame: *mut InterruptFrame) -> Option<Self> {
        if frame.is_null() {
            return None;
        }

        let args = unsafe {
            let f = &*frame;
            SyscallArgs {
                arg0: f.rdi,
                arg1: f.rsi,
                arg2: f.rdx,
                arg3: f.r10,
                arg4: f.r8,
                arg5: f.r9,
            }
        };

        Some(Self {
            task_ptr: task,
            frame_ptr: frame,
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

    #[inline]
    pub fn args(&self) -> &SyscallArgs {
        &self.args
    }

    #[inline]
    pub fn frame_ptr(&self) -> *mut InterruptFrame {
        self.frame_ptr
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
        syscall_return_ok(self.frame_ptr, value)
    }

    #[inline]
    pub fn err(&self) -> SyscallDisposition {
        syscall_return_err(self.frame_ptr, u64::MAX)
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
        self.err()
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

impl SyscallArgs {
    #[inline]
    pub fn arg0_u32(&self) -> u32 {
        self.arg0 as u32
    }
    #[inline]
    pub fn arg0_i32(&self) -> i32 {
        self.arg0 as i32
    }
    #[inline]
    pub fn arg0_ptr<T>(&self) -> *mut T {
        self.arg0 as *mut T
    }
    #[inline]
    pub fn arg0_const_ptr<T>(&self) -> *const T {
        self.arg0 as *const T
    }

    #[inline]
    pub fn arg1_u32(&self) -> u32 {
        self.arg1 as u32
    }
    #[inline]
    pub fn arg1_i32(&self) -> i32 {
        self.arg1 as i32
    }
    #[inline]
    pub fn arg1_usize(&self) -> usize {
        self.arg1 as usize
    }
    #[inline]
    pub fn arg1_ptr<T>(&self) -> *mut T {
        self.arg1 as *mut T
    }

    #[inline]
    pub fn arg2_u32(&self) -> u32 {
        self.arg2 as u32
    }
    #[inline]
    pub fn arg2_i32(&self) -> i32 {
        self.arg2 as i32
    }
    #[inline]
    pub fn arg2_usize(&self) -> usize {
        self.arg2 as usize
    }

    #[inline]
    pub fn arg3_u32(&self) -> u32 {
        self.arg3 as u32
    }
    #[inline]
    pub fn arg3_i32(&self) -> i32 {
        self.arg3 as i32
    }
}
