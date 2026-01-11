//! Syscall context extraction and helpers
//!
//! Provides safe wrappers around Task and InterruptFrame pointer access,
//! centralizing all unsafe dereferences into a single module.

use crate::syscall_common::{SyscallDisposition, syscall_return_err, syscall_return_ok};
use crate::syscall_types::{
    InterruptFrame, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, Task,
};
use slopos_core::wl_currency;

/// Arguments extracted from InterruptFrame registers (System V AMD64 ABI)
#[derive(Clone, Copy)]
pub struct SyscallArgs {
    pub arg0: u64, // rdi
    pub arg1: u64, // rsi
    pub arg2: u64, // rdx
    pub arg3: u64, // rcx
    pub arg4: u64, // r8
    pub arg5: u64, // r9
}

/// Safe wrapper around syscall task and frame pointers.
/// Centralizes all unsafe pointer access for syscall handlers.
pub struct SyscallContext {
    task_ptr: *mut Task,
    frame_ptr: *mut InterruptFrame,
    args: SyscallArgs,
}

impl SyscallContext {
    /// Create context from raw pointers. Returns None if frame is null.
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
                arg3: f.rcx,
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

    /// Returns true if task pointer is valid (non-null)
    #[inline]
    pub fn has_task(&self) -> bool {
        !self.task_ptr.is_null()
    }

    /// Get task_id if task is valid
    #[inline]
    pub fn task_id(&self) -> Option<u32> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { (*self.task_ptr).task_id })
        }
    }

    /// Get process_id if task is valid
    #[inline]
    pub fn process_id(&self) -> Option<u32> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { (*self.task_ptr).process_id })
        }
    }

    /// Check if task has a specific flag
    #[inline]
    pub fn has_flag(&self, flag: u16) -> bool {
        if self.task_ptr.is_null() {
            return false;
        }
        unsafe { (*self.task_ptr).flags & flag != 0 }
    }

    /// Check if task has compositor flag
    #[inline]
    pub fn is_compositor(&self) -> bool {
        self.has_flag(TASK_FLAG_COMPOSITOR)
    }

    /// Check if task has display exclusive flag
    #[inline]
    pub fn is_display_exclusive(&self) -> bool {
        self.has_flag(TASK_FLAG_DISPLAY_EXCLUSIVE)
    }

    /// Get syscall arguments
    #[inline]
    pub fn args(&self) -> &SyscallArgs {
        &self.args
    }

    /// Get raw frame pointer (for functions that need it)
    #[inline]
    pub fn frame_ptr(&self) -> *mut InterruptFrame {
        self.frame_ptr
    }

    /// Get raw task pointer (for functions that need it)
    #[inline]
    pub fn task_ptr(&self) -> *mut Task {
        self.task_ptr
    }

    /// Get mutable reference to task (unsafe operation exposed safely)
    #[inline]
    pub fn task_mut(&self) -> Option<&mut Task> {
        if self.task_ptr.is_null() {
            None
        } else {
            Some(unsafe { &mut *self.task_ptr })
        }
    }

    // =========================================================================
    // Return helpers
    // =========================================================================

    /// Return success with value
    #[inline]
    pub fn ok(&self, value: u64) -> SyscallDisposition {
        syscall_return_ok(self.frame_ptr, value)
    }

    /// Return error (sets rax to u64::MAX)
    #[inline]
    pub fn err(&self) -> SyscallDisposition {
        syscall_return_err(self.frame_ptr, u64::MAX)
    }

    /// Return success with W award
    #[inline]
    pub fn ok_win(&self, value: u64) -> SyscallDisposition {
        wl_currency::award_win();
        self.ok(value)
    }

    /// Return error with L award
    #[inline]
    pub fn err_loss(&self) -> SyscallDisposition {
        wl_currency::award_loss();
        self.err()
    }

    // =========================================================================
    // Requirement checking helpers (return error if check fails)
    // =========================================================================

    /// Require valid task, return error with loss if null
    #[inline]
    pub fn require_task(&self) -> Result<(), SyscallDisposition> {
        if self.task_ptr.is_null() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    /// Require task_id, return error with loss if task is null
    #[inline]
    pub fn require_task_id(&self) -> Result<u32, SyscallDisposition> {
        self.task_id().ok_or_else(|| self.err_loss())
    }

    /// Require process_id, return error with loss if task is null or process invalid
    #[inline]
    pub fn require_process_id(&self) -> Result<u32, SyscallDisposition> {
        match self.process_id() {
            Some(pid) if pid != crate::syscall_types::INVALID_PROCESS_ID => Ok(pid),
            _ => Err(self.err_loss()),
        }
    }

    /// Require compositor flag, return error with loss if not set
    #[inline]
    pub fn require_compositor(&self) -> Result<(), SyscallDisposition> {
        if !self.is_compositor() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    /// Require display exclusive flag, return error with loss if not set
    #[inline]
    pub fn require_display_exclusive(&self) -> Result<(), SyscallDisposition> {
        if !self.is_display_exclusive() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    /// Check a result code - if non-zero, return error with loss
    #[inline]
    pub fn check_result(&self, result: i32) -> Result<(), SyscallDisposition> {
        if result != 0 {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    /// Check a result code - if negative, return error with loss
    #[inline]
    pub fn check_negative(&self, result: i32) -> Result<(), SyscallDisposition> {
        if result < 0 {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn err_user_ptr(&self, _err: slopos_mm::user_ptr::UserPtrError) -> SyscallDisposition {
        wl_currency::award_loss();
        self.err()
    }

    #[inline]
    pub fn check_user_ptr<T>(
        &self,
        result: Result<T, slopos_mm::user_ptr::UserPtrError>,
    ) -> Result<T, SyscallDisposition> {
        result.map_err(|e| self.err_user_ptr(e))
    }
}

// Convenience type aliases for argument casting
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
