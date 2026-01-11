use crate::syscall::common::{SyscallDisposition, syscall_return_err, syscall_return_ok};
use crate::wl_currency;
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
    pub fn ok_win(&self, value: u64) -> SyscallDisposition {
        wl_currency::award_win();
        self.ok(value)
    }

    #[inline]
    pub fn err_loss(&self) -> SyscallDisposition {
        wl_currency::award_loss();
        self.err()
    }

    #[inline]
    pub fn require_task(&self) -> Result<(), SyscallDisposition> {
        if self.task_ptr.is_null() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn require_task_id(&self) -> Result<u32, SyscallDisposition> {
        self.task_id().ok_or_else(|| self.err_loss())
    }

    #[inline]
    pub fn require_process_id(&self) -> Result<u32, SyscallDisposition> {
        match self.process_id() {
            Some(pid) if pid != INVALID_PROCESS_ID => Ok(pid),
            _ => Err(self.err_loss()),
        }
    }

    #[inline]
    pub fn require_compositor(&self) -> Result<(), SyscallDisposition> {
        if !self.is_compositor() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn require_display_exclusive(&self) -> Result<(), SyscallDisposition> {
        if !self.is_display_exclusive() {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

    #[inline]
    pub fn check_result(&self, result: i32) -> Result<(), SyscallDisposition> {
        if result != 0 {
            Err(self.err_loss())
        } else {
            Ok(())
        }
    }

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
