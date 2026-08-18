//! `getrlimit` / `setrlimit` / `prlimit` — the enforced resource ceilings.
//!
//! The numbers reported are the ones the kernel actually consults, not
//! `RLIM_INFINITY` placeholders.

use core::ffi::c_int;

use slopos_abi::quota::{
    RLIM64_INFINITY, RLIMIT_AS, RLIMIT_DATA, RLIMIT_MEMLOCK, RLIMIT_NOFILE, RLIMIT_NPROC, RLimit64,
};
use slopos_abi::syscall::SYSCALL_PRLIMIT64;

use crate::errno;
use crate::pal::raw::syscall4;

pub use slopos_abi::quota::{
    RLIMIT_AS as RLIMIT_AS_C, RLIMIT_DATA as RLIMIT_DATA_C, RLIMIT_MEMLOCK as RLIMIT_MEMLOCK_C,
    RLIMIT_NOFILE as RLIMIT_NOFILE_C, RLIMIT_NPROC as RLIMIT_NPROC_C,
};

/// `struct rlimit`, which is `struct rlimit64` on a 64-bit target.
pub type RLimit = RLimit64;

pub const RLIM_INFINITY: u64 = RLIM64_INFINITY;

/// Every resource this kernel publishes. A resource outside this set is
/// `EINVAL`, deliberately: an unimplemented limit must not be
/// indistinguishable from an unlimited one.
pub const RLIMIT_ALL: [u32; 5] = [
    RLIMIT_DATA,
    RLIMIT_NPROC,
    RLIMIT_NOFILE,
    RLIMIT_MEMLOCK,
    RLIMIT_AS,
];

/// `pid` must be 0 or the caller's own: there is no privilege principal in
/// this kernel, so acting on another process would be unconditional.
pub fn prlimit(
    pid: c_int,
    resource: u32,
    new_limit: Option<&RLimit>,
    old_limit: Option<&mut RLimit>,
) -> c_int {
    let new_ptr = new_limit.map_or(0, |r| r as *const RLimit as u64);
    let old_ptr = old_limit.map_or(0, |r| r as *mut RLimit as u64);
    // SAFETY: both pointers are either null or derived from a live borrow of a
    // correctly-typed value, which is what the kernel side reads and writes.
    let ret = unsafe {
        syscall4(
            SYSCALL_PRLIMIT64,
            pid as u64,
            resource as u64,
            new_ptr,
            old_ptr,
        )
    };
    finish(ret)
}

pub fn getrlimit(resource: u32, rlim: &mut RLimit) -> c_int {
    prlimit(0, resource, None, Some(rlim))
}

/// Lowering succeeds; raising the hard limit is `EPERM`.
pub fn setrlimit(resource: u32, rlim: &RLimit) -> c_int {
    prlimit(0, resource, Some(rlim), None)
}

fn finish(ret: u64) -> c_int {
    let signed = ret as i64;
    if signed < 0 {
        errno::errno_set(-signed as i32);
        return -1;
    }
    0
}
