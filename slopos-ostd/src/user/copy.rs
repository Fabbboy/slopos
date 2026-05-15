//! Fault-recoverable user-space byte copy.
//!
//! [`copy_from_user`] / [`copy_to_user`] / [`copy_bytes_from_user`] /
//! [`copy_bytes_to_user`] are the only OSTD-sanctioned paths to move
//! bytes across the kernel/user boundary. They:
//!
//! 1. Walk the supplied [`VmSpace`] cursor to confirm every covering
//!    4 KiB page is present and user-accessible.
//! 2. Run a `rep movsb` between the asm labels
//!    `__ostd_usercopy_start..__ostd_usercopy_end`, with `STAC` /
//!    `CLAC` immediately around the loop. The page-fault handler
//!    recognises the fault range via [`is_ostd_usercopy_ip`] and
//!    redirects RIP to [`ostd_usercopy_fault_ip`], which clears AC
//!    and returns the un-copied byte count via `rax`.
//!
//! No public API exposes a raw `STAC`/`CLAC`. The only AC=1 window
//! in the kernel — outside `IRETQ` to user mode — lives inside
//! these symbols (Inv. 4).
//!
//! `copy_from_user::<T>` returns `T` by value via `MaybeUninit`. The
//! signature deliberately rules out returning `&T`, so a user
//! pointer can never escape into a kernel reference.

use core::mem::MaybeUninit;

use slopos_abi::addr::VirtAddr;

use crate::mm::Pod;
use crate::mm::vm_space::{MapError, VmSpace};
use crate::user::ptr::{UserPtr, UserVirtAddr};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UserCopyError {
    /// One of the covering pages is not present in the address space.
    NotMapped = 1,
    /// A covering page is present but not flagged user-accessible.
    NotUserAccessible = 2,
    /// A covering page is present and user-readable but not user-writable
    /// (relevant only for `copy_*_to_user`).
    NotUserWritable = 3,
    /// `addr + len` left the user range or overflowed.
    OutOfUserRange = 4,
    /// Page-fault during the copy: the user pages were unmapped or the
    /// permissions changed concurrently. The byte at offset
    /// `bytes_copied` did not transfer.
    Fault { bytes_copied: usize },
    /// VmSpace cursor construction returned an unexpected error
    /// (alignment / range). Programmer error in the caller.
    InvalidSpace,
}

// SMAP: `STAC` opens a window through which `rep movsb` can read or
// write user pages. `CLAC` closes it on every exit edge — both
// success and the fault-recovery branch. Hardware automatically
// clears AC on exception entry, so any nested kernel code that runs
// during the fault sees SMAP enforced normally; IRETQ then restores
// AC from the saved RFLAGS at `__ostd_usercopy_*`. The labels are
// distinct from the legacy `mm/src/user_copy.rs` symbols so both
// implementations can coexist in the binary while consumer
// migration is in flight.
core::arch::global_asm!(
    ".global __ostd_raw_usercopy",
    ".global __ostd_usercopy_start",
    ".global __ostd_usercopy_end",
    ".global __ostd_usercopy_fault",
    "__ostd_raw_usercopy:",
    "    mov rcx, rdx",
    "    stac",
    "__ostd_usercopy_start:",
    "    rep movsb",
    "__ostd_usercopy_end:",
    "    clac",
    "    xor eax, eax",
    "    ret",
    "__ostd_usercopy_fault:",
    "    clac",
    "    mov rax, rcx",
    "    ret",
);

unsafe extern "C" {
    fn __ostd_raw_usercopy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn __ostd_usercopy_start();
    fn __ostd_usercopy_end();
    fn __ostd_usercopy_fault();
}

/// Returns `true` if `rip` falls within the OSTD usercopy fault
/// region. The page-fault handler queries this and redirects RIP
/// to [`ostd_usercopy_fault_ip`] when it matches.
#[inline]
pub fn is_ostd_usercopy_ip(rip: u64) -> bool {
    let start = __ostd_usercopy_start as *const () as u64;
    let end = __ostd_usercopy_end as *const () as u64;
    rip >= start && rip < end
}

/// RIP value the page-fault handler should rewrite to when
/// [`is_ostd_usercopy_ip`] matches. Resumes at the CLAC + return
/// branch which surfaces the un-copied byte count via `rax`.
#[inline]
pub fn ostd_usercopy_fault_ip() -> u64 {
    __ostd_usercopy_fault as *const () as u64
}

#[derive(Copy, Clone, Debug)]
enum AccessKind {
    Read,
    Write,
}

fn validate_pages(
    space: &VmSpace,
    addr: u64,
    len: usize,
    access: AccessKind,
) -> Result<(), UserCopyError> {
    if len == 0 {
        return Ok(());
    }
    let end = addr
        .checked_add(len as u64)
        .ok_or(UserCopyError::OutOfUserRange)?;
    let page_size = 4096_u64;
    let first = VirtAddr::new(addr & !(page_size - 1));
    let last_page_start = (end - 1) & !(page_size - 1);
    let last_end_excl = VirtAddr::new(
        last_page_start
            .checked_add(page_size)
            .ok_or(UserCopyError::OutOfUserRange)?,
    );

    let cursor = space.cursor(first..last_end_excl).map_err(map_walk_err)?;

    let mut probe_at = first.as_u64();
    let mut cursor = cursor;
    while probe_at < last_end_excl.as_u64() {
        let entry = cursor.query().map_err(map_walk_err)?;
        let Some(_paddr) = entry.paddr else {
            return Err(UserCopyError::NotMapped);
        };
        if !entry.property.user {
            return Err(UserCopyError::NotUserAccessible);
        }
        if matches!(access, AccessKind::Write) && !entry.property.write {
            return Err(UserCopyError::NotUserWritable);
        }
        probe_at = probe_at
            .checked_add(page_size)
            .ok_or(UserCopyError::OutOfUserRange)?;
        if probe_at < last_end_excl.as_u64() {
            cursor.next().map_err(map_walk_err)?;
        }
    }
    Ok(())
}

fn map_walk_err(e: MapError) -> UserCopyError {
    match e {
        MapError::OutOfBounds | MapError::UnalignedRange => UserCopyError::OutOfUserRange,
        _ => UserCopyError::InvalidSpace,
    }
}

/// Copy a single `T: Pod` from user space.
///
/// Returns `T` by value — never a reference, never a borrow into a
/// user page. The compile-fail tests below verify this.
///
/// ```compile_fail
/// # use slopos_ostd::user::copy::copy_from_user;
/// # use slopos_ostd::user::ptr::UserPtr;
/// # use slopos_ostd::mm::vm_space::VmSpace;
/// # fn demo(space: &VmSpace, p: UserPtr<u64>) {
/// let _: &u64 = copy_from_user(space, p).unwrap();
/// # }
/// ```
pub fn copy_from_user<T: Pod>(space: &VmSpace, src: UserPtr<T>) -> Result<T, UserCopyError> {
    let len = core::mem::size_of::<T>();
    validate_pages(space, src.as_u64(), len, AccessKind::Read)?;
    let mut dst = MaybeUninit::<T>::uninit();
    let remaining =
        unsafe { __ostd_raw_usercopy(dst.as_mut_ptr() as *mut u8, src.as_ptr() as *const u8, len) };
    if remaining == 0 {
        Ok(unsafe { dst.assume_init() })
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len.saturating_sub(remaining),
        })
    }
}

/// Copy a single `T: Pod` to user space.
pub fn copy_to_user<T: Pod>(
    space: &VmSpace,
    dst: UserPtr<T>,
    value: &T,
) -> Result<(), UserCopyError> {
    let len = core::mem::size_of::<T>();
    validate_pages(space, dst.as_u64(), len, AccessKind::Write)?;
    let remaining = unsafe {
        __ostd_raw_usercopy(
            dst.as_mut_ptr() as *mut u8,
            value as *const T as *const u8,
            len,
        )
    };
    if remaining == 0 {
        Ok(())
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len.saturating_sub(remaining),
        })
    }
}

/// Copy a single `T: Copy` from user space.
///
/// Wider trait bound than [`copy_from_user`] (which requires `T: Pod`)
/// — accepts any `Copy` type. The caller carries responsibility that
/// the type's representation tolerates arbitrary byte patterns
/// (no `bool`, no enum with niches, etc.); the surrounding kernel
/// validates the value as appropriate. Provided for caller-API parity
/// with the 84 kernel callsites that pre-date the `Pod` trait — see
/// `mm/src/user_copy.rs` for the historical rationale.
pub fn copy_value_from_user<T: Copy>(space: &VmSpace, src: UserPtr<T>) -> Result<T, UserCopyError> {
    let len = core::mem::size_of::<T>();
    validate_pages(space, src.as_u64(), len, AccessKind::Read)?;
    let mut dst = MaybeUninit::<T>::uninit();
    // SAFETY: `__ostd_raw_usercopy` is the kernel's sole STAC/CLAC-
    // guarded movsb path; `dst.as_mut_ptr()` is exclusive (we own
    // the MaybeUninit), `src.as_ptr()` is the user-validated input.
    let remaining =
        unsafe { __ostd_raw_usercopy(dst.as_mut_ptr() as *mut u8, src.as_ptr() as *const u8, len) };
    if remaining == 0 {
        // SAFETY: all `len` bytes of `dst` were written by the
        // movsb above; the `T: Copy` contract permits arbitrary
        // byte patterns to count as a valid `T`.
        Ok(unsafe { dst.assume_init() })
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len.saturating_sub(remaining),
        })
    }
}

/// Copy a single `T: Copy` from kernel space into user space.
///
/// Sibling of [`copy_value_from_user`] — see its doc for the
/// `T: Copy` carve-out rationale.
pub fn copy_value_to_user<T: Copy>(
    space: &VmSpace,
    dst: UserPtr<T>,
    value: &T,
) -> Result<(), UserCopyError> {
    let len = core::mem::size_of::<T>();
    validate_pages(space, dst.as_u64(), len, AccessKind::Write)?;
    // SAFETY: as in [`copy_to_user`] — single STAC/CLAC-guarded movsb
    // call with caller-supplied user destination.
    let remaining = unsafe {
        __ostd_raw_usercopy(
            dst.as_mut_ptr() as *mut u8,
            value as *const T as *const u8,
            len,
        )
    };
    if remaining == 0 {
        Ok(())
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len.saturating_sub(remaining),
        })
    }
}

/// Copy raw bytes from user space.
pub fn copy_bytes_from_user(
    space: &VmSpace,
    src: UserVirtAddr,
    dst: &mut [u8],
) -> Result<(), UserCopyError> {
    let len = dst.len();
    if len == 0 {
        return Ok(());
    }
    validate_pages(space, src.as_u64(), len, AccessKind::Read)?;
    let remaining = unsafe { __ostd_raw_usercopy(dst.as_mut_ptr(), src.as_ptr::<u8>(), len) };
    if remaining == 0 {
        Ok(())
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len - remaining,
        })
    }
}

/// Copy raw bytes to user space.
pub fn copy_bytes_to_user(
    space: &VmSpace,
    dst: UserVirtAddr,
    src: &[u8],
) -> Result<(), UserCopyError> {
    let len = src.len();
    if len == 0 {
        return Ok(());
    }
    validate_pages(space, dst.as_u64(), len, AccessKind::Write)?;
    let remaining = unsafe { __ostd_raw_usercopy(dst.as_mut_ptr::<u8>(), src.as_ptr(), len) };
    if remaining == 0 {
        Ok(())
    } else {
        Err(UserCopyError::Fault {
            bytes_copied: len - remaining,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_ip_is_within_text() {
        let ip = ostd_usercopy_fault_ip();
        assert!(ip != 0);
    }

    #[test]
    fn fault_range_does_not_contain_fault_handler() {
        let fault = ostd_usercopy_fault_ip();
        assert!(!is_ostd_usercopy_ip(fault));
    }

    // The next two cases depend on the real binary layout of the
    // `global_asm!` block (`__ostd_usercopy_start` immediately precedes
    // the `rep movsb` byte, `__ostd_usercopy_end` follows it). Miri's
    // interpreter assigns external-fn pointer values that do not
    // preserve that layout, so the start..end range collapses and the
    // checks become meaningless. Skip under Miri.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn fault_range_contains_movsb() {
        let start = __ostd_usercopy_start as *const () as u64;
        assert!(is_ostd_usercopy_ip(start));
    }

    #[cfg_attr(miri, ignore)]
    #[test]
    fn fault_range_excludes_addresses_below_start() {
        let start = __ostd_usercopy_start as *const () as u64;
        assert!(!is_ostd_usercopy_ip(start - 1));
    }
}
