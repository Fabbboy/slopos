//! User-copy primitives — thin shim over [`slopos_ostd::user::copy`].
//!
//! All the byte-copy logic (`rep movsb`, SMAP STAC/CLAC, page-fault
//! recovery) lives in OSTD. This module adapts the PCR-implicit signature
//! (`copy_from_user(ptr) -> Result<T, UserPtrError>`) onto OSTD's
//! explicit-`&VmSpace` API: it resolves `pcr.syscall_pid` to a
//! [`KArc<VmSpace>`] (the per-slot lock is dropped before the copy runs) and
//! maps [`slopos_ostd::user::copy::UserCopyError`] back onto the single
//! [`UserPtrError`] type kernel callers expect.

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

/// One-shot probe (latched after the first run) confirming the kernel half
/// cannot be reached through the user-VA validator — catches a page table
/// whose user/kernel boundary is shifted, before any fault-recovering copy is
/// handed a kernel address.
fn check_kernel_guard(process: slopos_ostd::process::ProcessId) -> Result<(), UserPtrError> {
    if KERNEL_GUARD_CHECKED.is_set() {
        return Ok(());
    }
    let kernel_probe = crate::memory_layout_defs::KERNEL_HALF_PROBE_VA;
    if crate::process_vm::process_vm_user_va_is_user_accessible(process, kernel_probe) {
        return Err(UserPtrError::NotMapped);
    }
    KERNEL_GUARD_CHECKED.mark_set();
    Ok(())
}

#[inline]
fn current_vm_space() -> Result<slopos_ostd::KArc<slopos_ostd::mm::vm_space::VmSpace>, UserPtrError>
{
    // The PCR carries a bare pid across the syscall boundary; resolving it here
    // to a generation-checked designator means a pid naming no live process
    // fails before it can reach a slot lookup.
    let Some(process) = slopos_ostd::process::ProcessId::resolve(current_process_id()) else {
        return Err(UserPtrError::NotMapped);
    };
    check_kernel_guard(process)?;
    crate::process_vm::process_vm_get_vm_space(process).ok_or(UserPtrError::NotMapped)
}

/// Copy a `T: Copy` from user space into kernel space.
///
/// The transfer is fault-recoverable: a concurrent `munmap` on another CPU
/// surfaces as `UserPtrError::CopyFailed`, never a kernel panic.
///
/// `T: Copy` rather than OSTD's stricter `T: Pod`: the copy is byte-level, so
/// the caller is responsible for ensuring `T`'s representation tolerates
/// arbitrary byte patterns.
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
