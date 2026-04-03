//! Error types for kernel-userland communication

use core::ffi::c_int;

/// Trait for kernel error types that map to POSIX errno at the syscall boundary.
///
/// Implementors return a **negative** errno value (e.g., -EINVAL = -22).
pub trait KernelErrno {
    fn to_errno(&self) -> i32;
}

/// Implement common methods for kernel error enums.
///
/// Generates `as_c_int()`, `from_c_int()`, `is_success()`, and `is_error()` methods
/// for `#[repr(i32)]` error enums that follow the kernel's error convention.
macro_rules! impl_kernel_error {
    ($ty:ty, fallback: $fallback:ident, variants: { $($val:literal => $variant:ident),* $(,)? }) => {
        impl $ty {
            /// Convert to C-style integer for syscall returns.
            #[inline]
            pub fn as_c_int(self) -> c_int {
                self as c_int
            }

            /// Convert from C-style integer.
            #[inline]
            pub fn from_c_int(val: c_int) -> Self {
                match val {
                    $($val => Self::$variant,)*
                    _ => Self::$fallback,
                }
            }

            /// Check if this is a success result.
            #[inline]
            pub fn is_success(self) -> bool {
                matches!(self, Self::Success)
            }

            /// Check if this is an error result.
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

/// Shared memory operation errors
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShmError {
    /// Operation succeeded
    #[default]
    Success = 0,
    /// Failed to allocate physical memory for buffer
    AllocationFailed = -1,
    /// Failed to map buffer into address space
    MappingFailed = -2,
    /// Invalid or expired buffer token
    InvalidToken = -3,
    /// Operation not permitted (e.g., non-owner trying to destroy)
    PermissionDenied = -4,
    /// Maximum number of shared buffers reached
    BufferLimitReached = -5,
    /// Maximum number of mappings per buffer reached
    MappingLimitReached = -6,
    /// Invalid size (zero or too large)
    InvalidSize = -7,
}

impl_kernel_error!(ShmError, fallback: InvalidToken, variants: {
    0 => Success,
    -1 => AllocationFailed,
    -2 => MappingFailed,
    -3 => InvalidToken,
    -4 => PermissionDenied,
    -5 => BufferLimitReached,
    -6 => MappingLimitReached,
    -7 => InvalidSize,
});
