//! Validated user-space pointer types.
//!
//! [`UserPtr<T>`], [`UserSlice<T>`], and the underlying
//! [`UserVirtAddr`] are the only typed handles that may carry a
//! user-supplied address into kernel code. The validating constructor
//! [`UserVirtAddr::try_new`] checks: non-null, canonical x86_64,
//! within `[USER_SPACE_START_VA, USER_SPACE_END_VA)`, and
//! `addr + len` does not overflow — a kernel-half address is rejected
//! at construction so a `UserPtr<T>` value can never point into the
//! higher half (Inv. 5).
//!
//! Two public construction paths exist:
//!
//!   1. [`crate::user::context::UserContext::user_ptr_arg`] /
//!      [`crate::user::context::UserContext::user_slice_arg`] /
//!      [`crate::user::context::UserContext::user_bytes_arg`] —
//!      the canonical syscall-entry surface that takes a register
//!      index and validates the value the user loaded.
//!   2. [`UserPtr::try_new`] / [`UserSlice::try_new`] /
//!      [`UserVirtAddr::try_new`] — the same validating logic, used
//!      by kernel callers that derive a secondary user pointer from
//!      an already-validated one (e.g. advancing through an array,
//!      or stepping from `rsp` into a signal frame). The
//!      kernel-half-rejection guarantee is identical.

use core::marker::PhantomData;

use slopos_abi::addr::VirtAddr;

/// Lower bound (inclusive) of the user virtual-address range.
pub const USER_SPACE_START_VA: u64 = 0x0000_0000_0000_0000;

/// Upper bound (exclusive) of the user virtual-address range. On
/// x86_64 with 48-bit canonical addresses, the user half occupies
/// `[0x0, 0x0000_8000_0000_0000)` and the kernel half occupies
/// `[0xffff_8000_0000_0000, 0xffff_ffff_ffff_ffff]`.
pub const USER_SPACE_END_VA: u64 = 0x0000_8000_0000_0000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UserPtrError {
    Null = 1,
    NonCanonical = 2,
    OutOfUserRange = 3,
    Overflow = 4,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct UserVirtAddr(VirtAddr);

impl UserVirtAddr {
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

    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0.as_u64()
    }

    #[inline]
    pub const fn as_ptr<T>(self) -> *const T {
        self.0.as_ptr()
    }

    #[inline]
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0.as_mut_ptr()
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct UserPtr<T> {
    addr: UserVirtAddr,
    _marker: PhantomData<*const T>,
}

impl<T> UserPtr<T> {
    pub fn try_new(addr: u64) -> Result<Self, UserPtrError> {
        let validated = UserVirtAddr::try_new(addr, core::mem::size_of::<T>())?;
        Ok(Self {
            addr: validated,
            _marker: PhantomData,
        })
    }

    #[inline]
    pub const fn addr(self) -> UserVirtAddr {
        self.addr
    }

    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.addr.as_u64()
    }

    #[inline]
    pub const fn as_ptr(self) -> *const T {
        self.addr.as_ptr()
    }

    #[inline]
    pub const fn as_mut_ptr(self) -> *mut T {
        self.addr.as_mut_ptr()
    }
}

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

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn byte_len(&self) -> usize {
        self.len.saturating_mul(core::mem::size_of::<T>())
    }
}

pub type UserBytes = UserSlice<u8>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_rejected() {
        assert_eq!(UserVirtAddr::try_new(0, 1), Err(UserPtrError::Null));
    }

    #[test]
    fn non_canonical_rejected() {
        // bit 47 set but bits 48-63 zero → not canonical.
        let just_above_user = 0x0000_8000_0000_1000_u64;
        assert_eq!(
            UserVirtAddr::try_new(just_above_user, 1),
            Err(UserPtrError::NonCanonical)
        );
        // bit 47 zero, bits 48-63 nonzero → not canonical.
        let truly_non_canonical = 0x0001_0000_0000_0000_u64;
        assert_eq!(
            UserVirtAddr::try_new(truly_non_canonical, 1),
            Err(UserPtrError::NonCanonical)
        );
    }

    #[test]
    fn kernel_half_canonical_but_out_of_user_range() {
        // Bit 47 = 1, bits 48-63 all = 1 → canonical.
        let kernel_half = 0xffff_8000_0000_0000_u64;
        assert_eq!(
            UserVirtAddr::try_new(kernel_half, 1),
            Err(UserPtrError::OutOfUserRange)
        );
    }

    #[test]
    fn at_user_end_rejected() {
        // USER_SPACE_END_VA has bit 47 set and bits 48-63 zero — the
        // first check that fires is the canonicality test.
        assert_eq!(
            UserVirtAddr::try_new(USER_SPACE_END_VA, 1),
            Err(UserPtrError::NonCanonical)
        );
        // Just below the canonical boundary is in-range and canonical:
        // confirm the boundary is exclusive.
        let inside = USER_SPACE_END_VA - 0x1000;
        assert!(UserVirtAddr::try_new(inside, 1).is_ok());
    }

    #[test]
    fn straddles_user_end_overflow() {
        let near_end = USER_SPACE_END_VA - 4;
        assert_eq!(
            UserVirtAddr::try_new(near_end, 16),
            Err(UserPtrError::Overflow)
        );
    }

    #[test]
    fn happy_path() {
        let addr = 0x0000_4000_dead_b000_u64;
        let v = UserVirtAddr::try_new(addr, 4096).unwrap();
        assert_eq!(v.as_u64(), addr);
    }

    #[test]
    fn user_ptr_carries_size() {
        let p = UserPtr::<u64>::try_new(0x0000_4000_0000_1000).unwrap();
        assert_eq!(p.as_u64(), 0x0000_4000_0000_1000);
    }

    #[test]
    fn user_slice_overflow_in_count_mul() {
        let r = UserSlice::<u32>::try_new(0x0000_4000_0000_1000, usize::MAX);
        assert_eq!(r.unwrap_err(), UserPtrError::Overflow);
    }
}
