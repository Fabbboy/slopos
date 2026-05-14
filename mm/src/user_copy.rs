//! User-copy primitives — thin shim over [`slopos_ostd::user::copy`].
//!
//! All the byte-copy logic (`rep movsb` between
//! `__ostd_usercopy_start..__ostd_usercopy_end`, SMAP STAC/CLAC,
//! page-fault recovery) lives in OSTD. This module adapts the legacy
//! PCR-implicit signature
//! (`copy_from_user(ptr) -> Result<T, UserPtrError>`) onto OSTD's
//! explicit-`&VmSpace` API by:
//!
//!   1. Reading `pcr.syscall_pid` to identify the running user
//!      process.
//!   2. Acquiring a [`KArc<VmSpace>`] clone via
//!      [`crate::process_vm::process_vm_get_vm_space`] (the per-slot
//!      lock is dropped before the copy runs).
//!   3. Delegating to OSTD's `copy_*_user`.
//!   4. Mapping OSTD's [`slopos_ostd::user::copy::UserCopyError`]
//!      back onto the legacy [`UserPtrError`] enum (preserves the
//!      kernel callers' single-error-type contract; see
//!      `mm/src/user_ptr.rs`).
//!
//! The page-fault recovery branch in `boot/src/idt.rs` queries OSTD's
//! `is_ostd_usercopy_ip` directly. No SMAP-STAC asm lives in
//! `slopos_mm` anymore.

use slopos_ostd::sync::InitFlag;

use crate::user_ptr::{UserBytes, UserPtr, UserPtrError};

static KERNEL_GUARD_CHECKED: InitFlag = InitFlag::new();

#[inline]
fn current_process_id() -> u32 {
    slopos_arch::pcr::current_syscall_pid()
}

pub fn set_test_process_id(pid: u32) {
    slopos_arch::pcr::set_current_syscall_pid(pid);
}

/// One-shot probe (latched after the first run) confirming the
/// kernel-half cannot be reached through the user-VA validator —
/// catches a misconfigured page-table whose user/kernel boundary is
/// shifted, before we hand any fault-recovering copy a kernel
/// address. Identical to the legacy probe, retained here because the
/// shim still wraps the OSTD copy with kernel-side process-VM glue.
fn check_kernel_guard(pid: u32) -> Result<(), UserPtrError> {
    if KERNEL_GUARD_CHECKED.is_set() {
        return Ok(());
    }
    let kernel_probe = crate::memory_layout_defs::KERNEL_HEAP_VBASE;
    if crate::process_vm::process_vm_user_va_is_user_accessible(pid, kernel_probe) {
        return Err(UserPtrError::NotMapped);
    }
    KERNEL_GUARD_CHECKED.mark_set();
    Ok(())
}

#[inline]
fn current_vm_space() -> Result<slopos_ostd::KArc<slopos_ostd::mm::vm_space::VmSpace>, UserPtrError>
{
    let pid = current_process_id();
    if pid == slopos_abi::task::INVALID_PROCESS_ID {
        return Err(UserPtrError::NotMapped);
    }
    check_kernel_guard(pid)?;
    crate::process_vm::process_vm_get_vm_space(pid).ok_or(UserPtrError::NotMapped)
}

/// Copy a `T: Copy` from user space into kernel space.
///
/// Pages are walked through the per-process [`VmSpace`] and confirmed
/// present + user-readable; the actual transfer is fault-recoverable
/// via OSTD's `__ostd_raw_usercopy` asm (a concurrent `munmap` on
/// another CPU surfaces as `UserPtrError::CopyFailed`, never a
/// kernel panic).
///
/// `T: Copy` (rather than OSTD's stricter `T: Pod`) is preserved for
/// caller-API parity. The shim performs a byte-level copy; callers are
/// responsible for ensuring `T`'s representation tolerates arbitrary
/// byte patterns.
pub fn copy_from_user<T: Copy>(src: UserPtr<T>) -> Result<T, UserPtrError> {
    let space = current_vm_space()?;
    slopos_ostd::user::copy::copy_value_from_user(&space, src).map_err(Into::into)
}

/// Copy a `T: Copy` from kernel space into user space.
pub fn copy_to_user<T: Copy>(dst: UserPtr<T>, value: &T) -> Result<(), UserPtrError> {
    let space = current_vm_space()?;
    slopos_ostd::user::copy::copy_value_to_user(&space, dst, value).map_err(Into::into)
}

/// Copy raw bytes from user space.
pub fn copy_bytes_from_user(src: UserBytes, dst: &mut [u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }
    let space = current_vm_space()?;
    slopos_ostd::user::copy::copy_bytes_from_user(&space, src.base(), &mut dst[..copy_len])?;
    Ok(copy_len)
}

/// Copy raw bytes to user space.
pub fn copy_bytes_to_user(dst: UserBytes, src: &[u8]) -> Result<usize, UserPtrError> {
    let copy_len = src.len().min(dst.len());
    if copy_len == 0 {
        return Ok(0);
    }
    let space = current_vm_space()?;
    slopos_ostd::user::copy::copy_bytes_to_user(&space, dst.base(), &src[..copy_len])?;
    Ok(copy_len)
}
