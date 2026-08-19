//! `define_syscall!` — typed-argument syscall handler macro.
//!
//! Each invocation expands to a function with signature
//!
//! ```ignore
//! pub fn $name(ctx: &$crate::syscall::context::SyscallContext)
//!     -> $crate::syscall::result::SyscallResult
//! ```
//!
//! plus a **same-named module** holding `pub const DEF: SyscallEntry` — a
//! function and a module occupy different namespaces, so both can be `$name`.
//! `syscall_table!` reads `$handler::DEF`, which is what makes the
//! classification and the handler one artifact.
//!
//! The first identifier in the parameter list is the *context name* the body
//! sees (canonically `ctx`); subsequent `ident: Type` pairs are typed arguments
//! parsed from `ctx.regs()`. The `cap(...)` clause is **mandatory** and names
//! exactly one `slopos_ostd::authority::Capability`; omitting it is a macro-arm
//! mismatch, i.e. a compile error at the handler's own definition site, in the
//! crate that owns it. Requirement clauses live in a trailing `requires(...)`
//! block, and the body's return type must implement
//! [`crate::syscall::result::IntoSyscallResult`].
//!
//! # Grammar
//!
//! ```ignore
//! // no typed args
//! define_syscall!(name(ctx) cap(NoneSelf) -> RetType { body });
//!
//! // typed args
//! define_syscall!(name(ctx, a: Ty1, b: Ty2) cap(NoneFd) -> RetType { body });
//!
//! // requirement clause
//! define_syscall!(name(ctx, fd: Fd, buf: UserBytes) cap(NoneFd)
//!     requires(let pid: process_id) -> RetType { body });
//!
//! // raw form — no typed arg parsing at all
//! define_syscall!(raw $name(ctx) cap(NoneSelf) -> SyscallResult { body });
//! ```

/// Emit the `SyscallEntry` constant for one handler, in a module sharing the
/// handler's name.
#[macro_export]
#[doc(hidden)]
macro_rules! __syscall_def {
    ($name:ident, $cap:ident) => {
        #[allow(non_snake_case)]
        pub mod $name {
            /// The dispatch-table entry for this handler.
            ///
            /// Registering the handler without this is impossible:
            /// `syscall_table!` names `$handler::DEF`, so a slot can only be
            /// filled by something that carries its own classification.
            pub const DEF: $crate::syscall::common::SyscallEntry =
                $crate::syscall::common::SyscallEntry {
                    handler: ::core::option::Option::Some(super::$name),
                    name: ::slopos_ostd::sync::KernelSync::new(
                        concat!(stringify!($name), "\0").as_ptr() as *const ::core::ffi::c_char,
                    ),
                    cap: ::slopos_ostd::authority::Capability::$cap,
                };
        }
    };
}

#[macro_export]
macro_rules! define_syscall {
    (raw $name:ident ($ctx:ident) cap($cap:ident) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result($body)
        }
        $crate::__syscall_def!($name, $cap);
    };

    // The body runs inside a `move` closure returning the user-declared `$ret`,
    // so it can use both `?` on `Result<_, Errno>` and `return Err(errno)` with
    // natural variant names.
    ($name:ident ( $ctx:ident ) cap($cap:ident) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
        $crate::__syscall_def!($name, $cap);
    };

    ($name:ident ( $ctx:ident ) cap($cap:ident) requires ( $($req:tt)* ) -> $ret:ty $body:block) => {
        #[allow(unused_variables)]
        pub fn $name(
            $ctx: &$crate::syscall::context::SyscallContext,
        ) -> $crate::syscall::result::SyscallResult {
            $crate::define_syscall!(@reqs $ctx, $($req)*);
            let __body_value: $ret = (move || -> $ret { $body })();
            <$ret as $crate::syscall::result::IntoSyscallResult>::into_syscall_result(__body_value)
        }
        $crate::__syscall_def!($name, $cap);
    };

    ($name:ident ( $ctx:ident, $($arg_name:ident : $arg_ty:ty),+ $(,)? )
        cap($cap:ident)
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
        $crate::__syscall_def!($name, $cap);
    };

    ($name:ident ( $ctx:ident, $($arg_name:ident : $arg_ty:ty),+ $(,)? )
        cap($cap:ident)
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
        $crate::__syscall_def!($name, $cap);
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
    // The retired `compositor` / `display_exclusive` / `console_admin` arms
    // discarded the `Ok`, so the checked value was never the value used. A new
    // clause must bind, like the `task_id` and `process_id` arms above.
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
