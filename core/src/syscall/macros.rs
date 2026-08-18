//! `define_syscall!` — typed-argument syscall handler macro.
//!
//! Each invocation expands to a function with signature
//!
//! ```ignore
//! pub fn $name(ctx: &$crate::syscall::context::SyscallContext)
//!     -> $crate::syscall::result::SyscallResult
//! ```
//!
//! The first identifier in the parameter list is the *context name* the body
//! sees (canonically `ctx`); subsequent `ident: Type` pairs are typed arguments
//! parsed from `ctx.regs()`. Requirement clauses live in a trailing
//! `requires(...)` block, and the body's return type must implement
//! [`crate::syscall::result::IntoSyscallResult`].
//!
//! # Grammar
//!
//! ```ignore
//! // no typed args
//! define_syscall!(name(ctx) -> RetType { body });
//!
//! // typed args
//! define_syscall!(name(ctx, a: Ty1, b: Ty2) -> RetType { body });
//!
//! // requirement clause
//! define_syscall!(name(ctx, fd: Fd, buf: UserBytes)
//!     requires(let pid: process_id) -> RetType { body });
//!
//! // raw form — no typed arg parsing at all
//! define_syscall!(raw $name(ctx) -> SyscallResult { body });
//! ```

#[macro_export]
macro_rules! define_syscall {
    (raw $name:ident ($ctx:ident) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result($body)
        }
    };

    // The body runs inside a `move` closure returning the user-declared `$ret`,
    // so it can use both `?` on `Result<_, Errno>` and `return Err(errno)` with
    // natural variant names.
    ($name:ident ( $ctx:ident ) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
    };

    ($name:ident ( $ctx:ident ) requires ( $($req:tt)* ) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            $crate::define_syscall!(@reqs $ctx, $($req)*);
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
    };

    ($name:ident ( $ctx:ident, $($arg_name:ident : $arg_ty:ty),+ $(,)? )
        -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            let __regs = $ctx.regs();
            $crate::define_syscall!(@parse $ctx, __regs, 0usize, [$($arg_name : $arg_ty),+]);
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
    };

    ($name:ident ( $ctx:ident, $($arg_name:ident : $arg_ty:ty),+ $(,)? )
        requires ( $($req:tt)* )
        -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            $crate::define_syscall!(@reqs $ctx, $($req)*);
            let __regs = $ctx.regs();
            $crate::define_syscall!(@parse $ctx, __regs, 0usize, [$($arg_name : $arg_ty),+]);
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
    };

    (@reqs $ctx:ident,) => {};
    (@reqs $ctx:ident, ,) => {};

    (@reqs $ctx:ident, let $binding:ident : task_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_task_id() {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, $binding:ident : task_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_task_id() {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, let $binding:ident : process_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_process() {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, $binding:ident : process_id $(, $($rest:tt)*)?) => {
        #[allow(unused_variables)]
        let $binding = match $ctx.require_process() {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, compositor $(, $($rest:tt)*)?) => {
        if let Err(e) = $ctx.require_compositor() {
            return $crate::syscall::result::SyscallResult::Err(e);
        }
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, display_exclusive $(, $($rest:tt)*)?) => {
        if let Err(e) = $ctx.require_display_exclusive() {
            return $crate::syscall::result::SyscallResult::Err(e);
        }
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, console_admin $(, $($rest:tt)*)?) => {
        if let Err(e) = $ctx.require_console_admin() {
            return $crate::syscall::result::SyscallResult::Err(e);
        }
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };
    (@reqs $ctx:ident, net_admin $(, $($rest:tt)*)?) => {
        if let Err(e) = $ctx.require_net_admin() {
            return $crate::syscall::result::SyscallResult::Err(e);
        }
        $($crate::define_syscall!(@reqs $ctx, $($rest)*);)?
    };

    (@parse $ctx:ident, $regs:ident, $cursor:expr, []) => {
        const _: () = {
            assert!($cursor <= 6, "syscall args overflow available register slots");
        };
    };

    (@parse $ctx:ident, $regs:ident, $cursor:expr, [$head_name:ident : $head_ty:ty]) => {
        const _: () = {
            assert!(
                $cursor + <$head_ty as $crate::syscall::args::SyscallArg>::ARITY <= 6,
                "syscall args overflow available register slots",
            );
        };
        #[allow(unused_variables)]
        let $head_name: $head_ty = match
            <$head_ty as $crate::syscall::args::SyscallArg>::from_raw(
                &$regs[$cursor..$cursor + <$head_ty as $crate::syscall::args::SyscallArg>::ARITY],
                $ctx,
            )
        {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
    };

    (@parse $ctx:ident, $regs:ident, $cursor:expr,
        [$head_name:ident : $head_ty:ty, $($tail_name:ident : $tail_ty:ty),+ $(,)?]) => {
        #[allow(unused_variables)]
        let $head_name: $head_ty = match
            <$head_ty as $crate::syscall::args::SyscallArg>::from_raw(
                &$regs[$cursor..$cursor + <$head_ty as $crate::syscall::args::SyscallArg>::ARITY],
                $ctx,
            )
        {
            Ok(v) => v,
            Err(e) => return $crate::syscall::result::SyscallResult::Err(e),
        };
        $crate::define_syscall!(
            @parse $ctx, $regs,
            $cursor + <$head_ty as $crate::syscall::args::SyscallArg>::ARITY,
            [$($tail_name : $tail_ty),+]
        );
    };
}

#[macro_export]
macro_rules! ensure_or_err {
    ($cond:expr, $errno:expr) => {
        if !$cond {
            return $crate::syscall::result::SyscallResult::Err($errno);
        }
    };
}

#[macro_export]
macro_rules! return_err {
    ($errno:expr) => {
        return $crate::syscall::result::SyscallResult::Err($errno);
    };
}
