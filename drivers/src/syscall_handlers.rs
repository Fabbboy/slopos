use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::random;
use crate::serial;
use crate::wl_currency;
use crate::syscall_common::{
    SyscallDisposition, USER_IO_MAX_BYTES, syscall_bounded_from_user, syscall_copy_to_user_bounded,
    syscall_return_err, syscall_return_ok,
};
use crate::syscall_fs::{
    syscall_fs_close, syscall_fs_list, syscall_fs_mkdir, syscall_fs_open, syscall_fs_read,
    syscall_fs_stat, syscall_fs_unlink, syscall_fs_write,
};
use crate::syscall_types::{
    InterruptFrame, Task, TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE,
};
use crate::video_bridge;
use slopos_lib::{klog_debug, SYSCALL_FB_BLIT, SYSCALL_FB_FILL_RECT, SYSCALL_FB_FONT_DRAW};
use slopos_mm::user_copy_helpers::{UserCircle, UserLine, UserRect, UserText};

#[repr(C)]
pub struct UserFbInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

#[repr(C)]
pub struct UserSysInfo {
    pub total_pages: u32,
    pub free_pages: u32,
    pub allocated_pages: u32,
    pub total_tasks: u32,
    pub active_tasks: u32,
    pub task_context_switches: u64,
    pub scheduler_context_switches: u64,
    pub scheduler_yields: u64,
    pub ready_tasks: u32,
    pub schedule_calls: u32,
}

use crate::fate::{self, FateResult};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::user_copy::user_copy_from_user;
use slopos_mm::paging;
use slopos_mm::user_copy_helpers::{
    UserBlit, user_copy_blit_checked, user_copy_circle_checked, user_copy_line_checked,
    user_copy_rect_checked, user_copy_text_header,
};
use crate::video_bridge::{VideoError, VideoResult};
use core::sync::atomic::{AtomicU8, Ordering};
const GFX_LOG_MAX_TASKS: usize = 32;
static GFX_FAIL_LOGGED: [AtomicU8; GFX_LOG_MAX_TASKS] = {
    const ZERO: AtomicU8 = AtomicU8::new(0);
    [ZERO; GFX_LOG_MAX_TASKS]
};

use crate::scheduler_callbacks::{
    call_fate_apply_outcome, call_fate_set_pending, call_fate_spin, call_fate_take_pending,
    call_get_scheduler_stats, call_get_task_stats, call_schedule,
    call_scheduler_is_preemption_enabled, call_task_terminate, call_yield,
};

use crate::irq;
use crate::pit::{pit_get_frequency, pit_poll_delay_ms, pit_sleep_ms};
use crate::scheduler_callbacks::{call_kernel_reboot, call_kernel_shutdown};
use crate::tty::{tty_get_focus, tty_read_char_blocking, tty_read_line, tty_set_focus};

fn syscall_finish_gfx(frame: *mut InterruptFrame, rc: VideoResult) -> SyscallDisposition {
    if rc.is_ok() {
        syscall_return_ok(frame, 0)
    } else {
        syscall_return_err(frame, u64::MAX)
    }
}

fn video_result_from_font(rc: c_int) -> VideoResult {
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}

fn task_has_flag(task: *mut Task, flag: u16) -> bool {
    if task.is_null() {
        return false;
    }
    unsafe { (*task).flags & flag != 0 }
}

fn log_gfx_failure(task_id: u32, label: &str) {
    let idx = task_id as usize;
    if idx < GFX_LOG_MAX_TASKS && GFX_FAIL_LOGGED[idx].swap(1, Ordering::Relaxed) == 0 {
        klog_debug!("gfx: {} failed for task {}", label, task_id);
    }
}

pub fn syscall_yield(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let _ = syscall_return_ok(frame, 0);
    unsafe { call_yield() };
    SyscallDisposition::Ok
}

pub fn syscall_exit(task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        if !task.is_null() {
            (*task).exit_reason = crate::syscall_types::TaskExitReason::Normal;
            (*task).fault_reason = crate::syscall_types::TaskFaultReason::None;
            (*task).exit_code = 0;
        }
        call_task_terminate(if task.is_null() {
            u32::MAX
        } else {
            (*task).task_id
        });
    }
    unsafe { call_schedule() };
    SyscallDisposition::NoReturn
}

pub fn syscall_user_write(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let mut write_len: usize = 0;

    unsafe {
        if (*frame).rdi == 0
            || syscall_bounded_from_user(
                tmp.as_mut_ptr() as *mut c_void,
                tmp.len(),
                (*frame).rdi as *const c_void,
                (*frame).rsi,
                USER_IO_MAX_BYTES,
                &mut write_len as *mut usize,
            ) != 0
        {
            return syscall_return_err(frame, u64::MAX);
        }
    }

    // Best-effort raw serial write; ignore port for the Rust serial wrapper.
    let text = core::str::from_utf8(&tmp[..write_len]).unwrap_or("");
    serial::write_str(text);
    syscall_return_ok(frame, write_len as u64)
}

pub fn syscall_user_read(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    unsafe {
        if (*frame).rdi == 0 || (*frame).rsi == 0 {
            return syscall_return_err(frame, u64::MAX);
        }
    }

    let max_len = unsafe {
        if (*frame).rsi as usize > USER_IO_MAX_BYTES {
            USER_IO_MAX_BYTES
        } else {
            (*frame).rsi as usize
        }
    };

    let mut read_len = tty_read_line(tmp.as_mut_ptr(), max_len);
    if max_len > 0 {
        read_len = read_len.min(max_len.saturating_sub(1));
        tmp[read_len] = 0;
    }

    let copy_len = read_len.saturating_add(1).min(max_len);
    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rdi as *mut c_void },
        tmp.as_ptr() as *const c_void,
        copy_len,
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }

    syscall_return_ok(frame, read_len as u64)
}

pub fn syscall_user_read_char(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut c = 0u8;
    if tty_read_char_blocking(&mut c as *mut u8) != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    syscall_return_ok(frame, c as u64)
}

pub fn syscall_mouse_read(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut x: i32 = 0;
    let mut y: i32 = 0;
    let mut buttons: u8 = 0;

    if crate::mouse::mouse_read_event(&mut x, &mut y, &mut buttons) {
        // Pack result: buttons in low byte, x and y in return value
        // Return x in lower 32 bits of rax, caller reads y and buttons via rdi
        unsafe {
            if (*frame).rdi != 0 {
                let event_ptr = (*frame).rdi as *mut u8;
                // Write x (4 bytes)
                core::ptr::write_unaligned(event_ptr as *mut i32, x);
                // Write y (4 bytes)
                core::ptr::write_unaligned(event_ptr.add(4) as *mut i32, y);
                // Write buttons (1 byte)
                core::ptr::write(event_ptr.add(8), buttons);
            }
        }
        syscall_return_ok(frame, 1)
    } else {
        syscall_return_ok(frame, 0)
    }
}

pub fn syscall_tty_set_focus(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let target = unsafe { (*frame).rdi as u32 };
    if tty_set_focus(target) != 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    wl_currency::award_win();
    syscall_return_ok(frame, tty_get_focus() as u64)
}

pub fn syscall_roulette_spin(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let res = unsafe { call_fate_spin() };
    if task.is_null() {
        return syscall_return_err(frame, u64::MAX);
    }

    if unsafe { call_fate_set_pending(res, (*task).task_id) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let packed = ((res.token as u64) << 32) | res.value as u64;
    syscall_return_ok(frame, packed)
}

pub fn syscall_sleep_ms(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut ms = unsafe { (*frame).rdi };
    if ms > 60000 {
        ms = 60000;
    }
    if unsafe { call_scheduler_is_preemption_enabled() } != 0 {
        pit_sleep_ms(ms as u32);
    } else {
        pit_poll_delay_ms(ms as u32);
    }
    syscall_return_ok(frame, 0)
}

/// Returns the current time in milliseconds since boot.
/// Used for frame pacing in the compositor (60Hz target).
pub fn syscall_get_time_ms(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    let ms = (ticks * 1000) / freq as u64;
    syscall_return_ok(frame, ms)
}

pub fn syscall_fb_info(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let info = video_bridge::framebuffer_get_info();
    if info.is_null() {
        return syscall_return_err(frame, u64::MAX);
    }
    let info_local = unsafe { core::ptr::read(info) };
    if info_local.initialized == 0 {
        klog_debug!("syscall_fb_info: framebuffer not initialized");
        return syscall_return_err(frame, u64::MAX);
    }

    let user_info = UserFbInfo {
        width: info_local.width,
        height: info_local.height,
        pitch: info_local.pitch,
        bpp: info_local.bpp as u8,
        pixel_format: info_local.pixel_format as u8,
    };

    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rdi as *mut c_void },
        &user_info as *const _ as *const c_void,
        core::mem::size_of::<UserFbInfo>(),
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }

    syscall_return_ok(frame, 0)
}

pub fn syscall_gfx_fill_rect(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut rect = UserRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        color: 0,
    };
    if unsafe { user_copy_rect_checked(&mut rect, (*frame).rdi as *const UserRect) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::surface_draw_rect_filled_fast(
        task_id,
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        rect.color,
    );
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "fill_rect");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_gfx_draw_line(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut line = UserLine {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
        color: 0,
    };
    if unsafe { user_copy_line_checked(&mut line, (*frame).rdi as *const UserLine) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc =
        video_bridge::surface_draw_line(task_id, line.x0, line.y0, line.x1, line.y1, line.color);
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "draw_line");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_gfx_draw_circle(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut circle = UserCircle {
        cx: 0,
        cy: 0,
        radius: 0,
        color: 0,
    };
    if unsafe { user_copy_circle_checked(&mut circle, (*frame).rdi as *const UserCircle) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc =
        video_bridge::surface_draw_circle(task_id, circle.cx, circle.cy, circle.radius, circle.color);
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "draw_circle");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_gfx_draw_circle_filled(
    task: *mut Task,
    frame: *mut InterruptFrame,
) -> SyscallDisposition {
    let mut circle = UserCircle {
        cx: 0,
        cy: 0,
        radius: 0,
        color: 0,
    };
    if unsafe { user_copy_circle_checked(&mut circle, (*frame).rdi as *const UserCircle) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::surface_draw_circle_filled(
        task_id,
        circle.cx,
        circle.cy,
        circle.radius,
        circle.color,
    );
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "draw_circle_filled");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_font_draw(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut text = UserText {
        x: 0,
        y: 0,
        fg_color: 0,
        bg_color: 0,
        str_ptr: ptr::null(),
        len: 0,
    };
    if unsafe { user_copy_text_header(&mut text, (*frame).rdi as *const UserText) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if text.len == 0 || text.len as usize >= USER_IO_MAX_BYTES {
        return syscall_return_err(frame, u64::MAX);
    }

    let mut buf = [0u8; USER_IO_MAX_BYTES];
    if user_copy_from_user(
        buf.as_mut_ptr() as *mut c_void,
        text.str_ptr as *const c_void,
        text.len as usize,
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }
    buf[text.len as usize] = 0;
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_result_from_font(video_bridge::surface_font_draw_string(
        task_id,
        text.x,
        text.y,
        buf.as_ptr() as *const c_char,
        text.fg_color,
        text.bg_color,
    ));
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "font_draw");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_gfx_blit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let mut blit = UserBlit {
        src_x: 0,
        src_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: 0,
        height: 0,
    };
    if unsafe { user_copy_blit_checked(&mut blit, (*frame).rdi as *const UserBlit) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::surface_blit(
        task_id,
        blit.src_x,
        blit.src_y,
        blit.dst_x,
        blit.dst_y,
        blit.width,
        blit.height,
    );
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
        log_gfx_failure(task_id, "blit");
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_compositor_present(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::compositor_present();
    let disp = match rc {
        Ok(_) => syscall_return_ok(frame, 0),
        Err(_) => {
            wl_currency::award_loss();
            syscall_return_err(frame, u64::MAX)
        }
    };
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_enumerate_windows(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let out_buffer = unsafe { (*frame).rdi as *mut video_bridge::WindowInfo };
    let max_count = unsafe { (*frame).rsi as u32 };
    if out_buffer.is_null() || max_count == 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let count = video_bridge::surface_enumerate_windows(out_buffer, max_count);
    syscall_return_ok(frame, count as u64)
}

pub fn syscall_set_window_position(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let target_task_id = unsafe { (*frame).rdi as u32 };
    let x = unsafe { (*frame).rsi as i32 };
    let y = unsafe { (*frame).rdx as i32 };
    let rc = video_bridge::surface_set_window_position(target_task_id, x, y);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        syscall_return_ok(frame, 0)
    }
}

pub fn syscall_set_window_state(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let target_task_id = unsafe { (*frame).rdi as u32 };
    let state = unsafe { (*frame).rsi as u8 };
    let rc = video_bridge::surface_set_window_state(target_task_id, state);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        syscall_return_ok(frame, 0)
    }
}

pub fn syscall_raise_window(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let target_task_id = unsafe { (*frame).rdi as u32 };
    let rc = video_bridge::surface_raise_window(target_task_id);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        syscall_return_ok(frame, 0)
    }
}

/// Commit surface back buffer to front buffer (Wayland-style double buffering)
/// This is called by regular tasks to make their drawings visible to the compositor
pub fn syscall_surface_commit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let task_id = unsafe { (*task).task_id };
    let rc = video_bridge::surface_commit(task_id);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    }
}

pub fn syscall_compositor_present_with_damage(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let damage_ptr = unsafe { (*frame).rdi as *const video_bridge::DamageRegion };
    let damage_count = unsafe { (*frame).rsi as u32 };

    if damage_ptr.is_null() || damage_count == 0 || damage_count > 64 {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    // Copy damage regions from userspace to kernel buffer BEFORE switching page directories
    let mut kernel_buffer = [video_bridge::DamageRegion {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    }; 64];

    if user_copy_from_user(
        kernel_buffer.as_mut_ptr() as *mut c_void,
        damage_ptr as *const c_void,
        (damage_count as usize) * core::mem::size_of::<video_bridge::DamageRegion>(),
    ) != 0
    {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    // Now safe to switch to kernel page directory
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);

    let rc = video_bridge::compositor_present_with_damage(&kernel_buffer[..damage_count as usize]);

    let disp = match rc {
        Ok(_) => syscall_return_ok(frame, 0),
        Err(_) => {
            wl_currency::award_loss();
            syscall_return_err(frame, u64::MAX)
        }
    };
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_fb_fill_rect(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let mut rect = UserRect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        color: 0,
    };
    if unsafe { user_copy_rect_checked(&mut rect, (*frame).rdi as *const UserRect) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::draw_rect_filled_fast(rect.x, rect.y, rect.width, rect.height, rect.color);
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_fb_font_draw(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let mut text = UserText {
        x: 0,
        y: 0,
        fg_color: 0,
        bg_color: 0,
        str_ptr: ptr::null(),
        len: 0,
    };
    if unsafe { user_copy_text_header(&mut text, (*frame).rdi as *const UserText) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if text.len == 0 || text.len as usize >= USER_IO_MAX_BYTES {
        return syscall_return_err(frame, u64::MAX);
    }

    // Copy string data from userspace into kernel buffer
    let mut buf = [0u8; USER_IO_MAX_BYTES];
    if user_copy_from_user(
        buf.as_mut_ptr() as *mut c_void,
        text.str_ptr as *const c_void,
        text.len as usize,
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }
    buf[text.len as usize] = 0;

    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::font_draw_string(text.x, text.y, buf.as_ptr() as *const c_char, text.fg_color, text.bg_color);
    let _ = paging::switch_page_directory(original_dir);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        syscall_return_ok(frame, 0)
    }
}

pub fn syscall_fb_blit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let mut blit = UserBlit {
        src_x: 0,
        src_y: 0,
        dst_x: 0,
        dst_y: 0,
        width: 0,
        height: 0,
    };
    if unsafe { user_copy_blit_checked(&mut blit, (*frame).rdi as *const UserBlit) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::framebuffer_blit(
        blit.src_x,
        blit.src_y,
        blit.dst_x,
        blit.dst_y,
        blit.width,
        blit.height,
    );
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_random_next(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let value = random::random_next();
    syscall_return_ok(frame, value as u64)
}

pub fn syscall_roulette_draw(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if !task_has_flag(task, TASK_FLAG_DISPLAY_EXCLUSIVE) {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }
    let fate = unsafe { (*frame).rdi as u32 };
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::roulette_draw(fate);
    if rc.is_ok() {
        wl_currency::award_win();
    } else {
        wl_currency::award_loss();
    }
    let disp = syscall_finish_gfx(frame, rc);
    let _ = paging::switch_page_directory(original_dir);
    disp
}

pub fn syscall_roulette_result(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_err(frame, u64::MAX);
    }
    let mut stored = FateResult { token: 0, value: 0 };
    if unsafe { call_fate_take_pending((*task).task_id, &mut stored) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let token = unsafe { ((*frame).rdi >> 32) as u32 };
    if token != stored.token {
        return syscall_return_err(frame, u64::MAX);
    }

    // Check if the fate value is even (loss) or odd (win)
    let is_win = (stored.value & 1) == 1;

    unsafe {
        if is_win {
        // Win: award the win and continue
            call_fate_apply_outcome(&stored, 0, true);
            fate::fate_notify_outcome(&stored as *const FateResult);
            syscall_return_ok(frame, 0)
        } else {
            // Loss: award the loss and reboot to spin again
            call_fate_apply_outcome(&stored, 0, false);
            call_kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const c_char);
        }
    }
}

pub fn syscall_sys_info(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if unsafe { (*frame).rdi } == 0 {
        return syscall_return_err(frame, u64::MAX);
    }

    let mut info = UserSysInfo {
        total_pages: 0,
        free_pages: 0,
        allocated_pages: 0,
        total_tasks: 0,
        active_tasks: 0,
        task_context_switches: 0,
        scheduler_context_switches: 0,
        scheduler_yields: 0,
        ready_tasks: 0,
        schedule_calls: 0,
    };

    unsafe {
        get_page_allocator_stats(
            &mut info.total_pages,
            &mut info.free_pages,
            &mut info.allocated_pages,
        );
        call_get_task_stats(
            &mut info.total_tasks,
            &mut info.active_tasks,
            &mut info.task_context_switches,
        );
        call_get_scheduler_stats(
            &mut info.scheduler_context_switches,
            &mut info.scheduler_yields,
            &mut info.ready_tasks,
            &mut info.schedule_calls,
        );
    }

    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rdi as *mut c_void },
        &info as *const _ as *const c_void,
        core::mem::size_of::<UserSysInfo>(),
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }

    syscall_return_ok(frame, 0)
}

pub fn syscall_halt(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    unsafe {
        call_kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    }
    #[allow(unreachable_code)]
    // This should never be reached, but Rust needs a return value
    SyscallDisposition::Ok
}

use crate::syscall_common::SyscallHandler;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct SyscallEntry {
    pub handler: Option<SyscallHandler>,
    pub name: *const c_char,
}

unsafe impl Sync for SyscallEntry {}

static SYSCALL_TABLE: [SyscallEntry; 64] = {
    use self::lib_syscall_numbers::*;
    let mut table: [SyscallEntry; 64] = [SyscallEntry {
        handler: None,
        name: core::ptr::null(),
    }; 64];
    table[SYSCALL_YIELD as usize] = SyscallEntry {
        handler: Some(syscall_yield),
        name: b"yield\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_EXIT as usize] = SyscallEntry {
        handler: Some(syscall_exit),
        name: b"exit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_WRITE as usize] = SyscallEntry {
        handler: Some(syscall_user_write),
        name: b"write\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_READ as usize] = SyscallEntry {
        handler: Some(syscall_user_read),
        name: b"read\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_READ_CHAR as usize] = SyscallEntry {
        handler: Some(syscall_user_read_char),
        name: b"read_char\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_TTY_SET_FOCUS as usize] = SyscallEntry {
        handler: Some(syscall_tty_set_focus),
        name: b"tty_set_focus\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ROULETTE as usize] = SyscallEntry {
        handler: Some(syscall_roulette_spin),
        name: b"roulette\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SLEEP_MS as usize] = SyscallEntry {
        handler: Some(syscall_sleep_ms),
        name: b"sleep_ms\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_INFO as usize] = SyscallEntry {
        handler: Some(syscall_fb_info),
        name: b"fb_info\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_FILL_RECT as usize] = SyscallEntry {
        handler: Some(syscall_gfx_fill_rect),
        name: b"gfx_fill_rect\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_LINE as usize] = SyscallEntry {
        handler: Some(syscall_gfx_draw_line),
        name: b"gfx_draw_line\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_CIRCLE as usize] = SyscallEntry {
        handler: Some(syscall_gfx_draw_circle),
        name: b"gfx_draw_circle\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_CIRCLE_FILLED as usize] = SyscallEntry {
        handler: Some(syscall_gfx_draw_circle_filled),
        name: b"gfx_draw_circle_filled\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FONT_DRAW as usize] = SyscallEntry {
        handler: Some(syscall_font_draw),
        name: b"font_draw\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_BLIT as usize] = SyscallEntry {
        handler: Some(syscall_gfx_blit),
        name: b"gfx_blit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_COMPOSITOR_PRESENT as usize] = SyscallEntry {
        handler: Some(syscall_compositor_present),
        name: b"compositor_present\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_RANDOM_NEXT as usize] = SyscallEntry {
        handler: Some(syscall_random_next),
        name: b"random_next\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ROULETTE_DRAW as usize] = SyscallEntry {
        handler: Some(syscall_roulette_draw),
        name: b"roulette_draw\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ROULETTE_RESULT as usize] = SyscallEntry {
        handler: Some(syscall_roulette_result),
        name: b"roulette_result\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_OPEN as usize] = SyscallEntry {
        handler: Some(syscall_fs_open),
        name: b"fs_open\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_CLOSE as usize] = SyscallEntry {
        handler: Some(syscall_fs_close),
        name: b"fs_close\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_READ as usize] = SyscallEntry {
        handler: Some(syscall_fs_read),
        name: b"fs_read\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_WRITE as usize] = SyscallEntry {
        handler: Some(syscall_fs_write),
        name: b"fs_write\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_STAT as usize] = SyscallEntry {
        handler: Some(syscall_fs_stat),
        name: b"fs_stat\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_MKDIR as usize] = SyscallEntry {
        handler: Some(syscall_fs_mkdir),
        name: b"fs_mkdir\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_UNLINK as usize] = SyscallEntry {
        handler: Some(syscall_fs_unlink),
        name: b"fs_unlink\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_LIST as usize] = SyscallEntry {
        handler: Some(syscall_fs_list),
        name: b"fs_list\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SYS_INFO as usize] = SyscallEntry {
        handler: Some(syscall_sys_info),
        name: b"sys_info\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_HALT as usize] = SyscallEntry {
        handler: Some(syscall_halt),
        name: b"halt\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_MOUSE_READ as usize] = SyscallEntry {
        handler: Some(syscall_mouse_read),
        name: b"mouse_read\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ENUMERATE_WINDOWS as usize] = SyscallEntry {
        handler: Some(syscall_enumerate_windows),
        name: b"enumerate_windows\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SET_WINDOW_POSITION as usize] = SyscallEntry {
        handler: Some(syscall_set_window_position),
        name: b"set_window_position\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SET_WINDOW_STATE as usize] = SyscallEntry {
        handler: Some(syscall_set_window_state),
        name: b"set_window_state\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_RAISE_WINDOW as usize] = SyscallEntry {
        handler: Some(syscall_raise_window),
        name: b"raise_window\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_FILL_RECT as usize] = SyscallEntry {
        handler: Some(syscall_fb_fill_rect),
        name: b"fb_fill_rect\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_FONT_DRAW as usize] = SyscallEntry {
        handler: Some(syscall_fb_font_draw),
        name: b"fb_font_draw\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_COMPOSITOR_PRESENT_DAMAGE as usize] = SyscallEntry {
        handler: Some(syscall_compositor_present_with_damage),
        name: b"compositor_present_damage\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_BLIT as usize] = SyscallEntry {
        handler: Some(syscall_fb_blit),
        name: b"fb_blit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_COMMIT as usize] = SyscallEntry {
        handler: Some(syscall_surface_commit),
        name: b"surface_commit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GET_TIME_MS as usize] = SyscallEntry {
        handler: Some(syscall_get_time_ms),
        name: b"get_time_ms\0".as_ptr() as *const c_char,
    };
    table
};

pub mod lib_syscall_numbers {
    #![allow(non_upper_case_globals)]
    pub const SYSCALL_YIELD: u64 = 0;
    pub const SYSCALL_EXIT: u64 = 1;
    pub const SYSCALL_WRITE: u64 = 2;
    pub const SYSCALL_READ: u64 = 3;
    pub const SYSCALL_READ_CHAR: u64 = 25;
    pub const SYSCALL_ROULETTE: u64 = 4;
    pub const SYSCALL_SLEEP_MS: u64 = 5;
    pub const SYSCALL_FB_INFO: u64 = 6;
    pub const SYSCALL_GFX_FILL_RECT: u64 = 7;
    pub const SYSCALL_GFX_DRAW_LINE: u64 = 8;
    pub const SYSCALL_GFX_DRAW_CIRCLE: u64 = 9;
    pub const SYSCALL_GFX_DRAW_CIRCLE_FILLED: u64 = 10;
    pub const SYSCALL_FONT_DRAW: u64 = 11;
    pub const SYSCALL_GFX_BLIT: u64 = 26;
    pub const SYSCALL_COMPOSITOR_PRESENT: u64 = 27;
    pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
    pub const SYSCALL_RANDOM_NEXT: u64 = 12;
    pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
    pub const SYSCALL_ROULETTE_DRAW: u64 = 24;
    pub const SYSCALL_FS_OPEN: u64 = 14;
    pub const SYSCALL_FS_CLOSE: u64 = 15;
    pub const SYSCALL_FS_READ: u64 = 16;
    pub const SYSCALL_FS_WRITE: u64 = 17;
    pub const SYSCALL_FS_STAT: u64 = 18;
    pub const SYSCALL_FS_MKDIR: u64 = 19;
    pub const SYSCALL_FS_UNLINK: u64 = 20;
    pub const SYSCALL_FS_LIST: u64 = 21;
    pub const SYSCALL_SYS_INFO: u64 = 22;
    pub const SYSCALL_HALT: u64 = 23;
    pub const SYSCALL_MOUSE_READ: u64 = 29;
    pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;
    pub const SYSCALL_SET_WINDOW_POSITION: u64 = 31;
    pub const SYSCALL_SET_WINDOW_STATE: u64 = 32;
    pub const SYSCALL_RAISE_WINDOW: u64 = 33;
    pub const SYSCALL_COMPOSITOR_PRESENT_DAMAGE: u64 = 36;
    pub const SYSCALL_SURFACE_COMMIT: u64 = 38;
    pub const SYSCALL_GET_TIME_MS: u64 = 39;
}

pub fn syscall_lookup(sysno: u64) -> *const SyscallEntry {
    if (sysno as usize) >= SYSCALL_TABLE.len() {
        return ptr::null();
    }
    let entry = &SYSCALL_TABLE[sysno as usize];
    if entry.handler.is_none() {
        ptr::null()
    } else {
        entry as *const SyscallEntry
    }
}
