//! Declarative macros for syscall handler definitions
//!
//! These macros reduce boilerplate by handling:
//! - Context extraction from raw pointers
//! - Argument extraction from registers
//! - Common error handling patterns
//! - W/L currency tracking
//!
//! # Usage
//!
//! Due to Rust's macro hygiene, you must pass identifier names for `ctx` and `args`:
//!
//! ```ignore
//! define_syscall!(syscall_random_next(ctx, args) {
//!     let value = random::random_next();
//!     ctx.ok(value as u64)
//! });
//! ```
//!
//! For handlers requiring task_id or process_id, also pass those identifiers:
//!
//! ```ignore
//! define_syscall!(syscall_surface_commit(ctx, args, task_id) requires task_id {
//!     let rc = video_bridge::surface_commit(task_id);
//!     if rc < 0 { ctx.err_loss() } else { ctx.ok_win(0) }
//! });
//! ```

/// Main syscall definition macro.
#[macro_export]
macro_rules! define_syscall {
    // Pattern 1: Simple handler, no requirements
    // Usage: define_syscall!(name(ctx, args) { body })
    ($name:ident($ctx:ident, $args:ident) $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            let _ = task;
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // Pattern 2: Requires valid task_id (extracts and validates)
    // Usage: define_syscall!(name(ctx, args, task_id) requires task_id { body })
    ($name:ident($ctx:ident, $args:ident, $task_id:ident) requires task_id $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            #[allow(unused_variables)]
            let $task_id = match $ctx.require_task_id() {
                Ok(id) => id,
                Err(d) => return d,
            };
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // Pattern 3: Requires valid process_id (extracts and validates != INVALID)
    // Usage: define_syscall!(name(ctx, args, process_id) requires process_id { body })
    ($name:ident($ctx:ident, $args:ident, $process_id:ident) requires process_id $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            #[allow(unused_variables)]
            let $process_id = match $ctx.require_process_id() {
                Ok(id) => id,
                Err(d) => return d,
            };
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // Pattern 4: Requires both task_id and process_id
    // Usage: define_syscall!(name(ctx, args, task_id, process_id) requires task_and_process { body })
    ($name:ident($ctx:ident, $args:ident, $task_id:ident, $process_id:ident) requires task_and_process $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            #[allow(unused_variables)]
            let $task_id = match $ctx.require_task_id() {
                Ok(id) => id,
                Err(d) => return d,
            };
            #[allow(unused_variables)]
            let $process_id = match $ctx.require_process_id() {
                Ok(id) => id,
                Err(d) => return d,
            };
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // Pattern 5: Requires compositor flag
    // Usage: define_syscall!(name(ctx, args) requires compositor { body })
    ($name:ident($ctx:ident, $args:ident) requires compositor $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = $ctx.require_compositor() {
                return disp;
            }
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // Pattern 6: Requires display exclusive flag
    // Usage: define_syscall!(name(ctx, args) requires display_exclusive { body })
    ($name:ident($ctx:ident, $args:ident) requires display_exclusive $body:block) => {
        pub fn $name(
            task: *mut $crate::syscall_types::Task,
            frame: *mut $crate::syscall_types::InterruptFrame,
        ) -> $crate::syscall_common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall_context::SyscallContext::new(task, frame) else {
                return $crate::syscall_common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = $ctx.require_display_exclusive() {
                return disp;
            }
            #[allow(unused_variables)]
            let $args = $ctx.args();
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
