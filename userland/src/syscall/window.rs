//! Window and surface management syscalls.

use super::numbers::*;
use super::raw::{syscall0, syscall1, syscall2, syscall3, syscall4};
use slopos_abi::damage::DamageRect;
use slopos_abi::{DisplayInfo, SurfaceRole, WindowInfo};

#[inline(always)]
pub fn fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

#[inline(always)]
pub fn fb_flip(token: u32) -> i64 {
    unsafe { syscall3(SYSCALL_FB_FLIP, token as u64, 0, 0) as i64 }
}

#[inline(always)]
pub fn drain_queue() -> i64 {
    unsafe { syscall0(SYSCALL_DRAIN_QUEUE) as i64 }
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
pub fn surface_attach(token: u32, width: u32, height: u32) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_SURFACE_ATTACH,
            token as u64,
            width as u64,
            height as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn surface_commit() -> i64 {
    unsafe { syscall0(SYSCALL_SURFACE_COMMIT) as i64 }
}

#[inline(always)]
pub fn surface_frame() -> i64 {
    unsafe { syscall0(SYSCALL_SURFACE_FRAME) as i64 }
}

#[inline(always)]
pub fn poll_frame_done() -> u64 {
    unsafe { syscall0(SYSCALL_POLL_FRAME_DONE) }
}

#[inline(always)]
pub fn mark_frames_done(present_time_ms: u64) {
    unsafe {
        syscall1(SYSCALL_MARK_FRAMES_DONE, present_time_ms);
    }
}

#[inline(always)]
pub fn surface_damage(x: i32, y: i32, width: i32, height: i32) -> i64 {
    unsafe {
        syscall4(
            SYSCALL_SURFACE_DAMAGE,
            x as u64,
            y as u64,
            width as u64,
            height as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn buffer_age() -> u8 {
    unsafe { syscall0(SYSCALL_BUFFER_AGE) as u8 }
}

#[inline(always)]
pub fn surface_set_role(role: SurfaceRole) -> i64 {
    unsafe { syscall1(SYSCALL_SURFACE_SET_ROLE, role as u64) as i64 }
}

#[inline(always)]
pub fn surface_set_parent(parent_task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SURFACE_SET_PARENT, parent_task_id as u64) as i64 }
}

#[inline(always)]
pub fn surface_set_relative_position(rel_x: i32, rel_y: i32) -> i64 {
    unsafe { syscall2(SYSCALL_SURFACE_SET_REL_POS, rel_x as u64, rel_y as u64) as i64 }
}

#[inline(always)]
pub fn surface_set_title(title: &str) -> i64 {
    let bytes = title.as_bytes();
    unsafe {
        syscall2(
            SYSCALL_SURFACE_SET_TITLE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn surface_set_app_id(app_id: &str) -> i64 {
    let bytes = app_id.as_bytes();
    unsafe {
        syscall2(
            SYSCALL_SURFACE_SET_APP_ID,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
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

#[inline(always)]
pub fn set_window_position(task_id: u32, x: i32, y: i32) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_SET_WINDOW_POSITION,
            task_id as u64,
            x as u64,
            y as u64,
        ) as i64
    }
}

#[inline(always)]
pub fn set_window_state(task_id: u32, state: u8) -> i64 {
    unsafe { syscall2(SYSCALL_SET_WINDOW_STATE, task_id as u64, state as u64) as i64 }
}

#[inline(always)]
pub fn raise_window(task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_RAISE_WINDOW, task_id as u64) as i64 }
}

#[inline(always)]
pub fn set_cursor_shape(shape: u8) -> i64 {
    unsafe { syscall1(SYSCALL_SET_CURSOR_SHAPE, shape as u64) as i64 }
}
