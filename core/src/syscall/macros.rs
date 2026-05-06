/// Declarative macro for defining syscall handlers with composable requirements.
///
/// # Syntax
///
/// ```ignore
/// define_syscall!(handler_name(ctx, args) { body });
/// define_syscall!(handler_name(ctx, args) requires(let task_id, compositor) { body });
/// define_syscall!(handler_name(ctx, args) requires(let pid: process_id) { body });
/// ```
///
/// # Available requirements
///
/// | Syntax                    | Effect                                           |
/// |---------------------------|--------------------------------------------------|
/// | `let $name`               | Infers kind from name (`task_id` or `process_id`)|
/// | `let $name: task_id`      | Binds `$name` via `require_task_id()`            |
/// | `let $name: process_id`   | Binds `$name` via `require_process_id()`         |
/// | `compositor`              | Checks `require_compositor()`, no binding        |
/// | `display_exclusive`       | Checks `require_display_exclusive()`, no binding |
#[macro_export]
macro_rules! define_syscall {
    // Entry: no requirements
    ($name:ident($ctx:ident, $args:ident) $body:block) => {
        $crate::define_syscall!(@impl $name, $ctx, $args, [], $body);
    };

    // Entry: with requirements list
    ($name:ident($ctx:ident, $args:ident) requires($($req:tt)*) $body:block) => {
        $crate::define_syscall!(@impl $name, $ctx, $args, [$($req)*], $body);
    };

    // Implementation: generate the function, then expand requirements via TT munching
    (@impl $name:ident, $ctx:ident, $args:ident, [$($req:tt)*], $body:block) => {
        pub fn $name(
            task: *mut $crate::scheduler::task_struct::Task,
            user_ctx_ptr: *mut slopos_ostd::user::context::UserContext,
        ) -> $crate::syscall::common::SyscallDisposition {
            if user_ctx_ptr.is_null() {
                return $crate::syscall::common::syscall_return_err(
                    user_ctx_ptr,
                    slopos_abi::Errno::EINVAL.as_u64(),
                );
            }
            #[allow(unused_variables)]
            let Some($ctx) = $crate::syscall::context::SyscallContext::from_user_context(
                task,
                unsafe { &mut *user_ctx_ptr },
            ) else {
                return $crate::syscall::common::syscall_return_err(
                    user_ctx_ptr,
                    slopos_abi::Errno::EINVAL.as_u64(),
                );
            };
            $crate::define_syscall!(@expand_reqs $ctx, $($req)*);
            #[allow(unused_variables)]
            let $args = $ctx.args();
            $body
        }
    };

    // TT muncher: empty — base case
    (@expand_reqs $ctx:ident,) => {};

    // `let $binding: task_id` — explicit kind annotation
    (@expand_reqs $ctx:ident, let $binding:ident : task_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_task_id() {
            Ok(id) => id,
            Err(d) => return d,
        };
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // `let $binding: process_id` — explicit kind annotation
    (@expand_reqs $ctx:ident, let $binding:ident : process_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_process_id() {
            Ok(id) => id,
            Err(d) => return d,
        };
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // `let task_id` — shorthand (binding name == requirement kind)
    (@expand_reqs $ctx:ident, let $binding:ident $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $crate::define_syscall!(@resolve_req $ctx, $binding) {
            Ok(id) => id,
            Err(d) => return d,
        };
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // `compositor` — permission check, no binding
    (@expand_reqs $ctx:ident, compositor $(, $($rest:tt)*)?) => {
        if let Err(disp) = $ctx.require_compositor() {
            return disp;
        }
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // `console_admin` — capability check (CAP_SYS_TTY_CONFIG equivalent)
    (@expand_reqs $ctx:ident, console_admin $(, $($rest:tt)*)?) => {
        if let Err(disp) = $ctx.require_console_admin() {
            return disp;
        }
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // `display_exclusive` — permission check, no binding
    (@expand_reqs $ctx:ident, display_exclusive $(, $($rest:tt)*)?) => {
        if let Err(disp) = $ctx.require_display_exclusive() {
            return disp;
        }
        $($crate::define_syscall!(@expand_reqs $ctx, $($rest)*);)?
    };

    // Resolution: map binding name to the appropriate require_* call.
    // `task_id` → require_task_id; anything else → require_process_id (common default)
    (@resolve_req $ctx:ident, task_id) => { $ctx.require_task_id() };
    (@resolve_req $ctx:ident, process_id) => { $ctx.require_process_id() };
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
