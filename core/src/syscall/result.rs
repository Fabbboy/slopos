//! Typed syscall return values.
//!
//! Handler bodies return any type implementing [`IntoSyscallResult`];
//! the dispatcher converts it to [`SyscallResult`] and is the sole site
//! that touches `rax`.

use core::convert::Infallible;
use core::ops::{ControlFlow, FromResidual, Residual, Try};

use slopos_abi::Errno;

/// Final disposition of a syscall handler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyscallResult {
    /// Success — write `rax = value`.
    Ok(u64),
    /// Failure — write `rax = errno.as_u64()`; `Errno::ERESTARTSYS` becomes the
    /// `ERRNO_ERESTARTSYS` sentinel `handle_erestartsys` resolves into a
    /// transparent restart or `EINTR`.
    Err(Errno),
    /// Handler already wrote (or rewrote) the user-mode register state
    /// or the calling task is gone. Dispatcher leaves `rax` untouched.
    NoReturn,
}

/// Convert a handler body's return type into a [`SyscallResult`].
///
/// `()` and unsigned integers are successes; a signed integer below zero
/// becomes `Err(Errno)` constructed from the negative value.
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

// `?` interop — let handler bodies use `expr?` on `Result<_, Errno>` while the
// enclosing function returns `SyscallResult` directly.

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

impl Residual<u64> for SyscallResult {
    type TryType = SyscallResult;
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
