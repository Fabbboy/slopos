//! User pointer types — thin re-export of [`slopos_ostd::user::ptr`].
//!
//! [`UserPtr`], [`UserSlice`], [`UserBytes`], and [`UserVirtAddr`] live
//! in OSTD; this module simply forwards the type identities so existing
//! kernel callers (`slopos_core::syscall`, `slopos_fs`, `slopos_net`,
//! …) keep their `slopos_mm::user_ptr::UserPtr` import paths.
//!
//! [`UserPtrError`] stays defined locally because the kernel callers'
//! "validate + copy" combined path returns a single error enum that
//! covers both pointer-validation failures and runtime copy faults
//! (page unmapped after validation, SMAP-recovered fault). OSTD's
//! own `UserPtrError` covers only the validation half — the
//! `From<slopos_ostd::user::ptr::UserPtrError>` impl below bridges
//! the two.
//!
//! See `slopos_ostd::user::ptr` for the validation rules
//! (non-null, canonical x86_64, in-user-range, no-overflow).

pub use slopos_ostd::user::ptr::{UserBytes, UserPtr, UserSlice, UserVirtAddr};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UserPtrError {
    /// Pointer is null (address == 0).
    Null = 1,
    /// Address is not canonical (bits 48-63 don't match bit 47).
    NonCanonical = 2,
    /// Address is outside `[USER_SPACE_START_VA, USER_SPACE_END_VA)`.
    OutOfUserRange = 3,
    /// `addr + len` overflows or leaves the user range.
    Overflow = 4,
    /// Page is not mapped or not user-accessible in the page tables.
    NotMapped = 5,
    /// `rep movsb` faulted mid-copy (the user pages were unmapped or
    /// the permissions changed concurrently).
    CopyFailed = 6,
}

impl From<slopos_ostd::user::ptr::UserPtrError> for UserPtrError {
    fn from(e: slopos_ostd::user::ptr::UserPtrError) -> Self {
        use slopos_ostd::user::ptr::UserPtrError as O;
        match e {
            O::Null => Self::Null,
            O::NonCanonical => Self::NonCanonical,
            O::OutOfUserRange => Self::OutOfUserRange,
            O::Overflow => Self::Overflow,
        }
    }
}

impl From<slopos_ostd::user::copy::UserCopyError> for UserPtrError {
    fn from(e: slopos_ostd::user::copy::UserCopyError) -> Self {
        use slopos_ostd::user::copy::UserCopyError as C;
        match e {
            C::NotMapped | C::NotUserAccessible | C::NotUserWritable | C::InvalidSpace => {
                Self::NotMapped
            }
            C::OutOfUserRange => Self::OutOfUserRange,
            C::Fault { .. } => Self::CopyFailed,
        }
    }
}
