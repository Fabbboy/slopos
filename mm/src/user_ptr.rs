//! User pointer validation types for safe kernel/userland boundary crossing.
//!
//! These types ensure user-provided pointers are validated before use,
//! preventing kernel panics from malformed addresses. This is the Rust
//! equivalent of Linux's `access_ok()` + `__user` annotation.

use core::marker::PhantomData;

use slopos_abi::addr::VirtAddr;
use slopos_abi::arch::x86_64::memory::{USER_SPACE_END_VA, USER_SPACE_START_VA};

/// Error type for user pointer validation.
///
/// Each variant maps to a specific validation failure that would otherwise
/// cause a kernel panic if we used `VirtAddr::new()` directly.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UserPtrError {
    /// Pointer is null (address == 0)
    Null = 1,
    /// Address is not canonical (bits 48-63 don't match bit 47)
    NonCanonical = 2,
    /// Address is outside user space range [0, USER_SPACE_END_VA)
    OutOfUserRange = 3,
    /// Address + length would overflow u64
    Overflow = 4,
    /// Page is not mapped or not user-accessible in page tables
    NotMapped = 5,
    /// Copy operation failed during actual memory transfer
    CopyFailed = 6,
}

/// A validated user-space virtual address.
///
/// This type guarantees at construction time that:
/// - Address is not null (unless explicitly allowed)
/// - Address is canonical (valid x86_64 virtual address)
/// - Address is within user space bounds `[0, USER_SPACE_END_VA)`
/// - If a length was specified, `address + length` doesn't overflow
///
/// This does NOT guarantee the memory is mapped. Page table
/// validation must be done separately.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct UserVirtAddr(VirtAddr);

impl UserVirtAddr {
    /// Validate a user pointer with length check.
    ///
    /// # Errors
    ///
    /// Returns `Err` if:
    /// - `addr == 0` → [`UserPtrError::Null`]
    /// - Address is not canonical → [`UserPtrError::NonCanonical`]
    /// - Address is outside user space → [`UserPtrError::OutOfUserRange`]
    /// - `addr + len` overflows or exceeds user space → [`UserPtrError::Overflow`]
    pub fn try_new(addr: u64, len: usize) -> Result<Self, UserPtrError> {
        if addr == 0 {
            return Err(UserPtrError::Null);
        }

        if !VirtAddr::is_canonical(addr) {
            return Err(UserPtrError::NonCanonical);
        }

        if addr < USER_SPACE_START_VA || addr >= USER_SPACE_END_VA {
            return Err(UserPtrError::OutOfUserRange);
        }

        let end = addr.checked_add(len as u64).ok_or(UserPtrError::Overflow)?;
        if end > USER_SPACE_END_VA {
            return Err(UserPtrError::Overflow);
        }

        Ok(Self(VirtAddr(addr)))
    }

    /// Get the raw u64 value.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0.as_u64()
    }

    /// Convert to a const pointer of type T.
    #[inline]
    pub const fn as_ptr<T>(self) -> *const T {
        self.0.as_ptr()
    }

    /// Convert to a mutable pointer of type T.
    #[inline]
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0.as_mut_ptr()
    }
}

/// A typed user pointer with compile-time type information.
///
/// This is the Rust equivalent of Linux's `__user T*` annotation.
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct UserPtr<T> {
    addr: UserVirtAddr,
    _marker: PhantomData<*const T>,
}

impl<T> UserPtr<T> {
    /// Create a typed user pointer from a raw address.
    ///
    /// Validates that the address can hold at least `size_of::<T>()` bytes.
    pub fn try_new(addr: u64) -> Result<Self, UserPtrError> {
        let validated = UserVirtAddr::try_new(addr, core::mem::size_of::<T>())?;
        Ok(Self {
            addr: validated,
            _marker: PhantomData,
        })
    }

    /// Get the underlying UserVirtAddr.
    #[inline]
    pub const fn addr(self) -> UserVirtAddr {
        self.addr
    }

    /// Get the raw u64 address.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.addr.as_u64()
    }

    /// Get as const pointer.
    #[inline]
    pub const fn as_ptr(self) -> *const T {
        self.addr.as_ptr()
    }

    /// Get as mutable pointer.
    #[inline]
    pub const fn as_mut_ptr(self) -> *mut T {
        self.addr.as_mut_ptr()
    }
}

/// A validated user buffer/slice with element count.
#[derive(Copy, Clone, Debug)]
pub struct UserSlice<T> {
    base: UserVirtAddr,
    len: usize,
    _marker: PhantomData<*const T>,
}

impl<T> UserSlice<T> {
    pub fn try_new(addr: u64, count: usize) -> Result<Self, UserPtrError> {
        let byte_len = count
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(UserPtrError::Overflow)?;
        let validated = UserVirtAddr::try_new(addr, byte_len)?;
        Ok(Self {
            base: validated,
            len: count,
            _marker: PhantomData,
        })
    }

    #[inline]
    pub const fn base(&self) -> UserVirtAddr {
        self.base
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }
}

/// Convenience type for byte slices.
pub type UserBytes = UserSlice<u8>;
