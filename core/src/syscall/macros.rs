#[macro_export]
macro_rules! define_syscall {
    ($name:ident($ctx:ident, $args:ident) $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            let _ = task;
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
            };
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    ($name:ident($ctx:ident, $args:ident, $task_id:ident) requires task_id $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
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

    ($name:ident($ctx:ident, $args:ident, $process_id:ident) requires process_id $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
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

    ($name:ident($ctx:ident, $args:ident, $task_id:ident, $process_id:ident) requires task_and_process $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
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

    ($name:ident($ctx:ident, $args:ident) requires compositor $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
            };
            if let Err(disp) = $ctx.require_compositor() {
                return disp;
            }
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    ($name:ident($ctx:ident, $args:ident) requires display_exclusive $body:block) => {
        pub fn $name(
            task: *mut slopos_abi::task::Task,
            frame: *mut slopos_lib::InterruptFrame,
        ) -> $crate::syscall::common::SyscallDisposition {
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::new(task, frame) else {
                return $crate::syscall::common::syscall_return_err(frame, u64::MAX);
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

#[macro_export]
macro_rules! check_result {
    ($ctx:expr, $result:expr) => {
        if let Err(disp) = $ctx.check_result($result) {
            return disp;
        }
    };
}

#[macro_export]
macro_rules! check_negative {
    ($ctx:expr, $result:expr) => {
        if let Err(disp) = $ctx.check_negative($result) {
            return disp;
        }
    };
}

#[macro_export]
macro_rules! require_nonnull {
    ($ctx:expr, $ptr:expr) => {
        if $ptr.is_null() || ($ptr as u64) == 0 {
            return $ctx.err();
        }
    };
}

#[macro_export]
macro_rules! require_nonzero {
    ($ctx:expr, $val:expr) => {
        if $val == 0 {
            return $ctx.err();
        }
    };
}

#[macro_export]
macro_rules! try_or_err {
    ($ctx:expr, $result:expr) => {
        match $result {
            Ok(v) => v,
            Err(_) => return $ctx.err(),
        }
    };
}

#[macro_export]
macro_rules! some_or_err {
    ($ctx:expr, $option:expr) => {
        match $option {
            Some(v) => v,
            None => return $ctx.err(),
        }
    };
}
