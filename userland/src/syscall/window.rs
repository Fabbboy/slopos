//! Window and surface management syscalls.

use super::numbers::*;
use super::raw::{syscall1, syscall3};
use slopos_abi::DisplayInfo;
use slopos_abi::damage::DamageRect;

#[inline(always)]
pub fn fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

/// Present a memfd-backed buffer to the display.
#[inline(always)]
pub fn fb_flip(memfd_fd: u32) -> i64 {
    unsafe { syscall3(SYSCALL_FB_FLIP, memfd_fd as u64, 0, 0) as i64 }
}

/// Present a memfd-backed buffer with damage regions.
#[inline(always)]
pub fn fb_flip_damage(memfd_fd: u32, damage: &[DamageRect]) -> i64 {
    if damage.is_empty() {
        return fb_flip(memfd_fd);
    }
    unsafe {
        syscall3(
            SYSCALL_FB_FLIP,
            memfd_fd as u64,
            damage.as_ptr() as u64,
            damage.len() as u64,
        ) as i64
    }
}
