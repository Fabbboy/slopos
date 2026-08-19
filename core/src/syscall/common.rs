//! Shared syscall infrastructure: dispatch-table entry type, user-string
//! copy helpers, fixed I/O cap.

use core::ffi::{c_char, c_int};

use slopos_abi::Errno;
use slopos_ostd::authority::Capability;
use slopos_ostd::sync::KernelSync;

use slopos_mm::paging_defs::PAGE_SIZE_4KB_USIZE;
use slopos_mm::user_copy::{copy_bytes_from_user, copy_bytes_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtrError};

use crate::syscall::context::SyscallContext;
use crate::syscall::result::SyscallResult;

pub const USER_IO_MAX_BYTES: usize = 512;
pub use slopos_abi::fs::USER_PATH_MAX;

/// Convert a C-style negative return code into an [`Errno`], clamping
/// out-of-range values to `EINVAL`.
pub fn errno_from_neg(rc: i32) -> Errno {
    Errno::from_raw(rc).unwrap_or(Errno::EINVAL)
}

pub fn errno_from_neg64(rc: i64) -> Errno {
    Errno::from_raw(rc as i32).unwrap_or(Errno::EINVAL)
}

pub type SyscallHandler = fn(&SyscallContext) -> SyscallResult;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SyscallEntry {
    pub handler: Option<SyscallHandler>,
    /// Diagnostic label (NUL-terminated static string). `KernelSync` because
    /// raw pointers are `!Send + !Sync`; the target is `'static` text.
    pub name: KernelSync<*const c_char>,
    /// What this operation requires of its caller.
    ///
    /// Reaches this table *through the handler* — `define_syscall!` emits it
    /// into a same-named module and `syscall_table!` reads `$handler::DEF` —
    /// so the dispatcher's decision and the handler's own witness cannot
    /// disagree. There is exactly one artifact, which is what makes the
    /// totality assert a `rustc` error rather than a script with an allowlist.
    pub cap: Capability,
}

impl SyscallEntry {
    /// A slot no handler was registered into.
    pub const EMPTY: Self = Self {
        handler: None,
        name: KernelSync::new(core::ptr::null()),
        cap: Capability::Unimplemented,
    };
}

/// Copy a NUL-terminated string out of user memory, bounded by `dst`.
///
/// Copies a page at a time: reading the full capacity in one go would reject a
/// short string sitting near the end of its mapping.
pub fn syscall_copy_user_str(dst: &mut [u8], user_src: u64) -> Result<(), UserPtrError> {
    if dst.is_empty() {
        return Err(UserPtrError::Null);
    }

    let cap = dst.len().saturating_sub(1);
    dst[cap] = 0;

    let mut copied = 0usize;
    while copied < cap {
        let addr = user_src
            .checked_add(copied as u64)
            .ok_or(UserPtrError::Null)?;
        let page_remaining = PAGE_SIZE_4KB_USIZE - (addr as usize % PAGE_SIZE_4KB_USIZE);
        let chunk = page_remaining.min(cap - copied);

        let user_bytes = UserBytes::try_new(addr, chunk)?;
        copy_bytes_from_user(user_bytes, &mut dst[copied..copied + chunk])?;

        if dst[copied..copied + chunk].contains(&0) {
            return Ok(());
        }
        copied += chunk;
    }
    Ok(())
}

pub fn syscall_copy_user_str_to_cstr(dst: &mut [i8], user_src: u64) -> c_int {
    let dst_u8 = slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(dst);
    match syscall_copy_user_str(dst_u8, user_src) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

pub fn syscall_bounded_from_user(
    dst: &mut [u8],
    user_src: u64,
    requested_len: u64,
    cap_len: usize,
) -> Result<usize, UserPtrError> {
    if dst.is_empty() || requested_len == 0 {
        return Err(UserPtrError::Null);
    }

    let mut len = requested_len as usize;
    if len > cap_len {
        len = cap_len;
    }
    if len > dst.len() {
        len = dst.len();
    }

    let user_bytes = UserBytes::try_new(user_src, len)?;
    copy_bytes_from_user(user_bytes, &mut dst[..len])?;
    Ok(len)
}

pub fn syscall_copy_to_user_bounded(user_dst: u64, src: &[u8]) -> Result<(), UserPtrError> {
    if src.is_empty() {
        return Ok(());
    }

    let user_bytes = UserBytes::try_new(user_dst, src.len())?;
    copy_bytes_to_user(user_bytes, src)?;
    Ok(())
}
