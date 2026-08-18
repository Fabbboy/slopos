//! Window and surface management syscalls.

use super::numbers::*;
use super::raw::{syscall1, syscall2, syscall3};
use slopos_abi::DisplayInfo;
use slopos_abi::damage::DamageRect;

#[inline(always)]
pub fn fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn fb_flip(memfd_fd: u32) -> i64 {
    unsafe { syscall3(SYSCALL_FB_FLIP, memfd_fd as u64, 0, 0) as i64 }
}

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

/// Upload a 64×64 BGRA hardware-cursor image; `hot_x`/`hot_y` are the hotspot
/// within it. Returns 0, or negative when there is no hardware cursor and the
/// caller must composite in software.
#[inline(always)]
pub fn cursor_set_image(image: &[u8], hot_x: u32, hot_y: u32) -> i64 {
    let hotspot = ((hot_x & 0xFFFF) << 16) | (hot_y & 0xFFFF);
    unsafe {
        syscall3(
            SYSCALL_CURSOR_SET_IMAGE,
            image.as_ptr() as u64,
            image.len() as u64,
            hotspot as u64,
        ) as i64
    }
}

/// Move the hardware cursor to absolute display coordinates.
#[inline(always)]
pub fn cursor_move(x: u32, y: u32) -> i64 {
    let pos = ((x & 0xFFFF) << 16) | (y & 0xFFFF);
    unsafe { syscall1(SYSCALL_CURSOR_MOVE, pos as u64) as i64 }
}

#[inline(always)]
pub fn set_display_mode(width: u32, height: u32) -> i64 {
    unsafe { syscall2(SYSCALL_SET_DISPLAY_MODE, width as u64, height as u64) as i64 }
}
