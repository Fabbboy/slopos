//! Typed syscall return values.
//!
//! Handler bodies return any type implementing [`IntoSyscallResult`];
//! the dispatch glue converts to [`SyscallResult`] and writes the
//! caller-side register. The dispatcher is the sole site that touches
//! `rax`; handlers never call `ctx.ok()` / `ctx.err()` again.

use core::convert::Infallible;
use core::ops::{ControlFlow, FromResidual, Try};

use slopos_abi::Errno;

/// Final disposition of a syscall handler. Produced by the macro
/// after running [`IntoSyscallResult::into_syscall_result`] on the body.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallResult {
    /// Success — write `rax = value`.
    Ok(u64),
    /// Failure — write `rax = errno.as_u64()`. The dispatcher detects
    /// `Errno::ERESTARTSYS` and emits the `ERRNO_ERESTARTSYS` sentinel
    /// so `handle_erestartsys` can decide between transparent restart
    /// and `EINTR`.
    Err(Errno),
    /// Handler already wrote (or rewrote) the user-mode register state
    /// or the calling task is gone. Dispatcher leaves `rax` untouched.
    NoReturn,
}

/// Convert a handler body's return type into a [`SyscallResult`].
///
/// Implemented for the natural return shapes that handler bodies use:
///
/// * `()` — success returning `0`.
/// * Unsigned integers — success returning the value.
/// * Signed integers — success returning the value if `>= 0`, otherwise
///   `Err(Errno)` constructed from the negative value.
/// * `Result<T, Errno>` — explicit success / failure.
/// * `SyscallResult` — passthrough (used by handlers that want
///   `NoReturn`).
pub trait IntoSyscallResult {
    fn into_syscall_result(self) -> SyscallResult;
}

impl IntoSyscallResult for SyscallResult {
    #[inline]
    fn into_syscall_result(self) -> SyscallResult {
        self
    }
}

impl IntoSyscallResult for () {
    #[inline]
    fn into_syscall_result(self) -> SyscallResult {
        SyscallResult::Ok(0)
    }
}

macro_rules! impl_into_unsigned {
    ($($t:ty),+) => {
        $(impl IntoSyscallResult for $t {
            #[inline]
            fn into_syscall_result(self) -> SyscallResult {
                SyscallResult::Ok(self as u64)
            }
        })+
    };
}
impl_into_unsigned!(u8, u16, u32, u64, usize);

macro_rules! impl_into_signed {
    ($($t:ty),+) => {
        $(impl IntoSyscallResult for $t {
            #[inline]
            fn into_syscall_result(self) -> SyscallResult {
                if self < 0 {
                    match Errno::from_raw(self as i32) {
                        Some(e) => SyscallResult::Err(e),
                        None => SyscallResult::Err(Errno::EINVAL),
                    }
                } else {
                    SyscallResult::Ok(self as u64)
                }
            }
        })+
    };
}
impl_into_signed!(i8, i16, i32, i64, isize);

impl<T: IntoSyscallResult> IntoSyscallResult for Result<T, Errno> {
    #[inline]
    fn into_syscall_result(self) -> SyscallResult {
        match self {
            Ok(v) => v.into_syscall_result(),
            Err(e) => SyscallResult::Err(e),
        }
    }
}

impl<T: IntoSyscallResult> IntoSyscallResult for Result<T, SyscallResult> {
    #[inline]
    fn into_syscall_result(self) -> SyscallResult {
        match self {
            Ok(v) => v.into_syscall_result(),
            Err(r) => r,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// `?` interop — let handler bodies use `expr?` on `Result<_, Errno>`
// while the enclosing function returns `SyscallResult` directly. This
// keeps the macro's wrapper allocation-free (no inner closure) so the
// per-handler stack frame stays under the 2 KiB frame gate.
// ─────────────────────────────────────────────────────────────────────

impl Try for SyscallResult {
    type Output = u64;
    type Residual = SyscallResult;

    #[inline]
    fn from_output(output: Self::Output) -> Self {
        SyscallResult::Ok(output)
    }

    #[inline]
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            SyscallResult::Ok(v) => ControlFlow::Continue(v),
            other => ControlFlow::Break(other),
        }
    }
}

impl FromResidual<SyscallResult> for SyscallResult {
    #[inline]
    fn from_residual(residual: SyscallResult) -> Self {
        residual
    }
}

impl FromResidual<Result<Infallible, Errno>> for SyscallResult {
    #[inline]
    fn from_residual(residual: Result<Infallible, Errno>) -> Self {
        match residual {
            Err(e) => SyscallResult::Err(e),
        }
    }
}
