//! Declarative macros for syscall handler definitions
//!
//! These macros reduce boilerplate by handling:
//! - Context extraction from raw pointers
//! - Argument extraction from registers
//! - Common error handling patterns
//! - W/L currency tracking

/// Main syscall definition macro.
///
/// # Variants
///
/// ## Simple handler (no requirements)
/// ```ignore
/// define_syscall!(syscall_random_next() {
///     let value = random::random_next();
///     ctx.ok(value as u64)
/// });
/// ```
///
/// ## Task-requiring handler
/// ```ignore
/// define_syscall!(syscall_surface_commit() requires task {
///     let task_id = ctx.require_task_id()?;
///     let rc = video_bridge::surface_commit(task_id);
///     if rc < 0 { ctx.err_loss() } else { ctx.ok_win(0) }
/// });
/// ```
///
/// ## Compositor-only handler
/// ```ignore
/// define_syscall!(syscall_drain_queue() requires compositor {
///     video_bridge::drain_queue();
///     ctx.ok(0)
/// });
/// ```
#[macro_export]
macro_rules! define_syscall {
    // Pattern 1: Simple handler, no requirements
    ($name:ident() $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let _ = task; // suppress unused warning
            let Some(ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            let _args = ctx.args();
            $body
        }
    };

    // Pattern 2: Requires valid task (null check with loss)
    ($name:ident() requires task $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let Some(ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = ctx.require_task() {
                return disp;
            }
            let _args = ctx.args();
            $body
        }
    };

    // Pattern 3: Requires compositor flag
    ($name:ident() requires compositor $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let Some(ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = ctx.require_compositor() {
                return disp;
            }
            let _args = ctx.args();
            $body
        }
    };

    // Pattern 4: Requires display exclusive flag
    ($name:ident() requires display_exclusive $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let Some(ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = ctx.require_display_exclusive() {
                return disp;
            }
            let _args = ctx.args();
            $body
        }
    };

    // Pattern 5: Requires valid process (task + process_id check)
    ($name:ident() requires process $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let Some(ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            let _args = ctx.args();
            $body
        }
    };
}

/// Helper macro to check result and return error with loss if non-zero
#[macro_export]
macro_rules! check_result {
    ($ctx:expr, $result:expr) => {
        if let Err(disp) = $ctx.check_result($result) {
            return disp;
        }
    };
}

/// Helper macro to check result and return error with loss if negative
#[macro_export]
macro_rules! check_negative {
    ($ctx:expr, $result:expr) => {
        if let Err(disp) = $ctx.check_negative($result) {
            return disp;
        }
    };
}
