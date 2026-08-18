//! Error types for kernel-userland communication

use core::ffi::c_int;

/// Kernel error types that map to POSIX errno at the syscall boundary.
/// Implementors return a **negative** errno value (e.g., -EINVAL = -22).
pub trait KernelErrno {
    fn to_errno(&self) -> i32;
}

/// Generates `as_c_int`, `from_c_int`, `is_success` and `is_error` for
/// `#[repr(i32)]` error enums.
macro_rules! impl_kernel_error {
    ($ty:ty, fallback: $fallback:ident, variants: { $($val:literal => $variant:ident),* $(,)? }) => {
        impl $ty {
            #[inline]
            pub fn as_c_int(self) -> c_int {
                self as c_int
            }

            #[inline]
            pub fn from_c_int(val: c_int) -> Self {
                match val {
                    $($val => Self::$variant,)*
                    _ => Self::$fallback,
                }
            }

            #[inline]
            pub fn is_success(self) -> bool {
                matches!(self, Self::Success)
            }

            #[inline]
            pub fn is_error(self) -> bool {
                !self.is_success()
            }
        }

        impl $crate::KernelErrno for $ty {
            #[inline]
            fn to_errno(&self) -> i32 {
                self.as_c_int()
            }
        }
    };
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemfdError {
    #[default]
    Success = 0,
    AllocationFailed = -1,
    MappingFailed = -2,
    InvalidToken = -3,
    PermissionDenied = -4,
    BufferLimitReached = -5,
    MappingLimitReached = -6,
    InvalidSize = -7,
}

impl_kernel_error!(MemfdError, fallback: InvalidToken, variants: {
    0 => Success,
    -1 => AllocationFailed,
    -2 => MappingFailed,
    -3 => InvalidToken,
    -4 => PermissionDenied,
    -5 => BufferLimitReached,
    -6 => MappingLimitReached,
    -7 => InvalidSize,
});
