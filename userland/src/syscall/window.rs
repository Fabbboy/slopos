//! Window and surface management syscalls.

use super::numbers::*;
use super::raw::{syscall1, syscall2, syscall3};
use slopos_abi::damage::DamageRect;
use slopos_abi::{DisplayInfo, WindowInfo};

#[inline(always)]
pub fn fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn fb_flip(token: u32) -> i64 {
    unsafe { syscall3(SYSCALL_FB_FLIP, token as u64, 0, 0) as i64 }
}

#[inline(always)]
pub fn fb_flip_damage(token: u32, damage: &[DamageRect]) -> i64 {
    if damage.is_empty() {
        return fb_flip(token);
    }
    unsafe {
        syscall3(
            SYSCALL_FB_FLIP,
            token as u64,
            damage.as_ptr() as u64,
            damage.len() as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn enumerate_windows(windows: &mut [WindowInfo]) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_ENUMERATE_WINDOWS,
            windows.as_mut_ptr() as u64,
            windows.len() as u64,
        ) as i64
    }
}
