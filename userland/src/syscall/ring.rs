//! SlopRing syscall wrappers (SLOPRING § 6) — the two raw entry points
//! `slibc-ring` (the userland runtime) builds on.

use super::raw::{syscall2, syscall4};
use slopos_abi::ring::RingParams;
use slopos_abi::syscall::{SYSCALL_RING_ENTER, SYSCALL_RING_REGISTER, SYSCALL_RING_SETUP};

/// `ring_setup(entries, params*)` — create a ring (SLOPRING § 6.1).
/// Returns the ring fd (`>= 0`) or a negated errno. `params` is filled
/// in with the ring geometry + the user VA the region was mapped at.
#[inline(always)]
pub fn ring_setup(entries: u32, params: &mut RingParams) -> i32 {
    unsafe { syscall2(SYSCALL_RING_SETUP, entries as u64, params as *mut _ as u64) as i32 }
}

/// `ring_enter(ring_fd, to_submit, min_complete, flags)` — submit and/or
/// harvest (SLOPRING § 6.2). Returns the submission count or a negated
/// errno.
#[inline(always)]
pub fn ring_enter(ring_fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> i32 {
    unsafe {
        syscall4(
            SYSCALL_RING_ENTER,
            ring_fd as u64,
            to_submit as u64,
            min_complete as u64,
            flags as u64,
        ) as i32
    }
}

/// `ring_register(ring_fd, op, arg, nr_args)` — register provided/fixed
/// buffers with a ring (SLOPRING § 13, ABI v2). Returns 0 on success or a
/// negated errno. Phase 3 ships the ABI only: every `op` returns
/// `-ENOSYS` (Phase 4 implements the provided/fixed buffer paths). The
/// shared `syscall4` wrapper already clobbers XMM/YMM, so user FP/SIMD
/// state cannot be corrupted across the boundary.
#[inline(always)]
pub fn ring_register(ring_fd: i32, op: u32, arg: u64, nr_args: u32) -> i32 {
    unsafe {
        syscall4(
            SYSCALL_RING_REGISTER,
            ring_fd as u64,
            op as u64,
            arg,
            nr_args as u64,
        ) as i32
    }
}
