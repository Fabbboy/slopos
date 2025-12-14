#![allow(dead_code)]
#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use crate::random;
use crate::serial;
use crate::syscall_common::{
    syscall_bounded_from_user, syscall_copy_to_user_bounded, syscall_disposition,
    syscall_return_err, syscall_return_ok, USER_IO_MAX_BYTES,
};
use crate::syscall_fs::{
    syscall_fs_close, syscall_fs_list, syscall_fs_mkdir, syscall_fs_open, syscall_fs_read,
    syscall_fs_stat, syscall_fs_unlink, syscall_fs_write,
};
use crate::syscall_types::{task_t, InterruptFrame};
use slopos_lib::klog_printf;

#[repr(C)]
pub struct user_rect_t {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: u32,
}

#[repr(C)]
pub struct user_line_t {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
    pub color: u32,
}

#[repr(C)]
pub struct user_circle_t {
    pub cx: i32,
    pub cy: i32,
    pub radius: i32,
    pub color: u32,
}

#[repr(C)]
pub struct user_text_t {
    pub x: i32,
    pub y: i32,
    pub fg_color: u32,
    pub bg_color: u32,
    pub str_ptr: *const c_char,
    pub len: u32,
}

#[repr(C)]
pub struct user_fb_info_t {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

#[repr(C)]
pub struct user_sys_info_t {
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct fate_result {
    pub token: u32,
    pub value: u32,
}

#[repr(C)]
pub struct framebuffer_info_t {
    pub initialized: u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub pixel_format: u32,
}

unsafe extern "C" {
    fn user_copy_rect_checked(dst: *mut user_rect_t, src: *const user_rect_t) -> c_int;
    fn user_copy_line_checked(dst: *mut user_line_t, src: *const user_line_t) -> c_int;
    fn user_copy_circle_checked(dst: *mut user_circle_t, src: *const user_circle_t) -> c_int;
    fn user_copy_text_header(dst: *mut user_text_t, src: *const user_text_t) -> c_int;
    fn user_copy_from_user(dst: *mut c_void, src: *const c_void, len: usize) -> c_int;
    fn yield_();
    fn schedule();
    fn task_terminate(task_id: u32) -> c_int;
    fn scheduler_is_preemption_enabled() -> c_int;
    fn get_page_allocator_stats(total: *mut u32, free: *mut u32, allocated: *mut u32);
    fn get_task_stats(total: *mut u32, active: *mut u32, context_switches: *mut u64);
    fn get_scheduler_stats(
        context_switches: *mut u64,
        yields: *mut u64,
        ready_tasks: *mut u32,
        schedule_calls: *mut u32,
    );

    fn tty_read_line(buffer: *mut c_char, buffer_size: usize) -> usize;
    fn tty_notify_input_ready();

    fn serial_write(port: u16, buf: *const c_char, len: usize);

    fn pit_sleep_ms(ms: u32);
    fn pit_poll_delay_ms(ms: u32);

    fn fate_spin() -> fate_result;
    fn fate_set_pending(res: fate_result, task_id: u32) -> c_int;
    fn fate_take_pending(task_id: u32, out: *mut fate_result) -> c_int;
    fn fate_apply_outcome(res: *const fate_result, resolution: u32, award: bool);

    fn graphics_draw_rect_filled_fast(x: i32, y: i32, w: i32, h: i32, color: u32) -> c_int;
    fn graphics_draw_line(x0: i32, y0: i32, x1: i32, y1: i32, color: u32) -> c_int;
    fn graphics_draw_circle(cx: i32, cy: i32, r: i32, color: u32) -> c_int;
    fn graphics_draw_circle_filled(cx: i32, cy: i32, r: i32, color: u32) -> c_int;
    fn font_draw_string(x: i32, y: i32, text: *const c_char, fg: u32, bg: u32) -> c_int;
    fn framebuffer_get_info() -> *mut framebuffer_info_t;

    fn kernel_shutdown(reason: *const c_char) -> !;
}

fn syscall_finish_gfx(frame: *mut InterruptFrame, rc: c_int) -> syscall_disposition {
    if rc == 0 {
        syscall_return_ok(frame, 0)
    } else {
        syscall_return_err(frame, u64::MAX)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_yield(_task: *mut task_t, frame: *mut InterruptFrame) -> syscall_disposition {
    let _ = syscall_return_ok(frame, 0);
    unsafe { yield_() };
    syscall_disposition::SYSCALL_DISP_OK
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_exit(task: *mut task_t, _frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        if !task.is_null() {
            (*task).exit_reason = crate::syscall_types::task_exit_reason_t::Normal;
            (*task).fault_reason = crate::syscall_types::task_fault_reason_t::None;
            (*task).exit_code = 0;
        }
        task_terminate(if task.is_null() { u32::MAX } else { (*task).task_id });
    }
    unsafe { schedule() };
    syscall_disposition::SYSCALL_DISP_NO_RETURN
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_user_write(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
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

#[unsafe(no_mangle)]
pub extern "C" fn syscall_user_read(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
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

    let mut read_len = unsafe { tty_read_line(tmp.as_mut_ptr() as *mut c_char, max_len) };
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

#[unsafe(no_mangle)]
pub extern "C" fn syscall_roulette_spin(
    task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let res = unsafe { fate_spin() };
    if task.is_null() {
        return syscall_return_err(frame, u64::MAX);
    }

    if unsafe { fate_set_pending(res, (*task).task_id) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let packed = ((res.token as u64) << 32) | res.value as u64;
    syscall_return_ok(frame, packed)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_sleep_ms(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut ms = unsafe { (*frame).rdi };
    if ms > 60000 {
        ms = 60000;
    }
    if unsafe { scheduler_is_preemption_enabled() } != 0 {
        unsafe { pit_sleep_ms(ms as u32) };
    } else {
        unsafe { pit_poll_delay_ms(ms as u32) };
    }
    syscall_return_ok(frame, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_fb_info(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let info = unsafe { framebuffer_get_info() };
    if info.is_null() || unsafe { (*info).initialized == 0 } {
        unsafe {
            klog_printf(
                slopos_lib::klog::KlogLevel::Debug,
                b"syscall_fb_info: framebuffer not initialized\n\0".as_ptr() as *const c_char,
            );
        }
        return syscall_return_err(frame, u64::MAX);
    }

    let user_info = user_fb_info_t {
        width: unsafe { (*info).width },
        height: unsafe { (*info).height },
        pitch: unsafe { (*info).pitch },
        bpp: unsafe { (*info).bpp as u8 },
        pixel_format: unsafe { (*info).pixel_format as u8 },
    };

    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rdi as *mut c_void },
        &user_info as *const _ as *const c_void,
        core::mem::size_of::<user_fb_info_t>(),
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }

    syscall_return_ok(frame, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_gfx_fill_rect(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut rect = user_rect_t {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        color: 0,
    };
    if unsafe { user_copy_rect_checked(&mut rect, (*frame).rdi as *const user_rect_t) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let rc = unsafe { graphics_draw_rect_filled_fast(rect.x, rect.y, rect.width, rect.height, rect.color) };
    syscall_finish_gfx(frame, rc)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_gfx_draw_line(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut line = user_line_t {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
        color: 0,
    };
    if unsafe { user_copy_line_checked(&mut line, (*frame).rdi as *const user_line_t) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let rc = unsafe { graphics_draw_line(line.x0, line.y0, line.x1, line.y1, line.color) };
    syscall_finish_gfx(frame, rc)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_gfx_draw_circle(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut circle = user_circle_t {
        cx: 0,
        cy: 0,
        radius: 0,
        color: 0,
    };
    if unsafe { user_copy_circle_checked(&mut circle, (*frame).rdi as *const user_circle_t) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let rc = unsafe { graphics_draw_circle(circle.cx, circle.cy, circle.radius, circle.color) };
    syscall_finish_gfx(frame, rc)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_gfx_draw_circle_filled(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut circle = user_circle_t {
        cx: 0,
        cy: 0,
        radius: 0,
        color: 0,
    };
    if unsafe { user_copy_circle_checked(&mut circle, (*frame).rdi as *const user_circle_t) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let rc = unsafe { graphics_draw_circle_filled(circle.cx, circle.cy, circle.radius, circle.color) };
    syscall_finish_gfx(frame, rc)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_font_draw(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let mut text = user_text_t {
        x: 0,
        y: 0,
        fg_color: 0,
        bg_color: 0,
        str_ptr: ptr::null(),
        len: 0,
    };
    if unsafe { user_copy_text_header(&mut text, (*frame).rdi as *const user_text_t) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    if text.len == 0 || text.len as usize >= USER_IO_MAX_BYTES {
        return syscall_return_err(frame, u64::MAX);
    }

    let mut buf = [0u8; USER_IO_MAX_BYTES];
    unsafe {
        if user_copy_from_user(
            buf.as_mut_ptr() as *mut c_void,
            text.str_ptr as *const c_void,
            text.len as usize,
        ) != 0
        {
            return syscall_return_err(frame, u64::MAX);
        }
    }
    buf[text.len as usize] = 0;
    let rc = unsafe { font_draw_string(text.x, text.y, buf.as_ptr() as *const c_char, text.fg_color, text.bg_color) };
    syscall_finish_gfx(frame, rc)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_random_next(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    let value = random::random_next();
    syscall_return_ok(frame, value as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_roulette_result(
    task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    if task.is_null() {
        return syscall_return_err(frame, u64::MAX);
    }
    let mut stored = fate_result { token: 0, value: 0 };
    if unsafe { fate_take_pending((*task).task_id, &mut stored as *mut fate_result) } != 0 {
        return syscall_return_err(frame, u64::MAX);
    }
    let token = unsafe { ((*frame).rdi >> 32) as u32 };
    if token != stored.token {
        return syscall_return_err(frame, u64::MAX);
    }
    unsafe {
        fate_apply_outcome(&stored, 0, true);
    }
    syscall_return_ok(frame, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_sys_info(
    _task: *mut task_t,
    frame: *mut InterruptFrame,
) -> syscall_disposition {
    if unsafe { (*frame).rdi } == 0 {
        return syscall_return_err(frame, u64::MAX);
    }

    let mut info = user_sys_info_t {
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
        get_task_stats(
            &mut info.total_tasks,
            &mut info.active_tasks,
            &mut info.task_context_switches,
        );
        get_scheduler_stats(
            &mut info.scheduler_context_switches,
            &mut info.scheduler_yields,
            &mut info.ready_tasks,
            &mut info.schedule_calls,
        );
    }

    if syscall_copy_to_user_bounded(
        unsafe { (*frame).rdi as *mut c_void },
        &info as *const _ as *const c_void,
        core::mem::size_of::<user_sys_info_t>(),
    ) != 0
    {
        return syscall_return_err(frame, u64::MAX);
    }

    syscall_return_ok(frame, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_halt(_task: *mut task_t, _frame: *mut InterruptFrame) -> syscall_disposition {
    unsafe {
        kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct syscall_entry {
    pub handler: Option<extern "C" fn(*mut task_t, *mut InterruptFrame) -> syscall_disposition>,
    pub name: *const c_char,
}

unsafe impl Sync for syscall_entry {}

static SYSCALL_TABLE: [syscall_entry; 32] = {
    use self::lib_syscall_numbers::*;
    let mut table: [syscall_entry; 32] = [syscall_entry {
        handler: None,
        name: core::ptr::null(),
    }; 32];
    table[SYSCALL_YIELD as usize] = syscall_entry {
        handler: Some(syscall_yield),
        name: b"yield\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_EXIT as usize] = syscall_entry {
        handler: Some(syscall_exit),
        name: b"exit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_WRITE as usize] = syscall_entry {
        handler: Some(syscall_user_write),
        name: b"write\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_READ as usize] = syscall_entry {
        handler: Some(syscall_user_read),
        name: b"read\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ROULETTE as usize] = syscall_entry {
        handler: Some(syscall_roulette_spin),
        name: b"roulette\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SLEEP_MS as usize] = syscall_entry {
        handler: Some(syscall_sleep_ms),
        name: b"sleep_ms\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_INFO as usize] = syscall_entry {
        handler: Some(syscall_fb_info),
        name: b"fb_info\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_FILL_RECT as usize] = syscall_entry {
        handler: Some(syscall_gfx_fill_rect),
        name: b"gfx_fill_rect\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_LINE as usize] = syscall_entry {
        handler: Some(syscall_gfx_draw_line),
        name: b"gfx_draw_line\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_CIRCLE as usize] = syscall_entry {
        handler: Some(syscall_gfx_draw_circle),
        name: b"gfx_draw_circle\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GFX_DRAW_CIRCLE_FILLED as usize] = syscall_entry {
        handler: Some(syscall_gfx_draw_circle_filled),
        name: b"gfx_draw_circle_filled\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FONT_DRAW as usize] = syscall_entry {
        handler: Some(syscall_font_draw),
        name: b"font_draw\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_RANDOM_NEXT as usize] = syscall_entry {
        handler: Some(syscall_random_next),
        name: b"random_next\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_ROULETTE_RESULT as usize] = syscall_entry {
        handler: Some(syscall_roulette_result),
        name: b"roulette_result\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_OPEN as usize] = syscall_entry {
        handler: Some(syscall_fs_open),
        name: b"fs_open\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_CLOSE as usize] = syscall_entry {
        handler: Some(syscall_fs_close),
        name: b"fs_close\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_READ as usize] = syscall_entry {
        handler: Some(syscall_fs_read),
        name: b"fs_read\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_WRITE as usize] = syscall_entry {
        handler: Some(syscall_fs_write),
        name: b"fs_write\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_STAT as usize] = syscall_entry {
        handler: Some(syscall_fs_stat),
        name: b"fs_stat\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_MKDIR as usize] = syscall_entry {
        handler: Some(syscall_fs_mkdir),
        name: b"fs_mkdir\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_UNLINK as usize] = syscall_entry {
        handler: Some(syscall_fs_unlink),
        name: b"fs_unlink\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FS_LIST as usize] = syscall_entry {
        handler: Some(syscall_fs_list),
        name: b"fs_list\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SYS_INFO as usize] = syscall_entry {
        handler: Some(syscall_sys_info),
        name: b"sys_info\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_HALT as usize] = syscall_entry {
        handler: Some(syscall_halt),
        name: b"halt\0".as_ptr() as *const c_char,
    };
    table
};

pub mod lib_syscall_numbers {
    #![allow(non_upper_case_globals)]
    pub const SYSCALL_YIELD: u64 = 0;
    pub const SYSCALL_EXIT: u64 = 1;
    pub const SYSCALL_WRITE: u64 = 2;
    pub const SYSCALL_READ: u64 = 3;
    pub const SYSCALL_ROULETTE: u64 = 4;
    pub const SYSCALL_SLEEP_MS: u64 = 5;
    pub const SYSCALL_FB_INFO: u64 = 6;
    pub const SYSCALL_GFX_FILL_RECT: u64 = 7;
    pub const SYSCALL_GFX_DRAW_LINE: u64 = 8;
    pub const SYSCALL_GFX_DRAW_CIRCLE: u64 = 9;
    pub const SYSCALL_GFX_DRAW_CIRCLE_FILLED: u64 = 10;
    pub const SYSCALL_FONT_DRAW: u64 = 11;
    pub const SYSCALL_RANDOM_NEXT: u64 = 12;
    pub const SYSCALL_ROULETTE_RESULT: u64 = 13;
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
}

#[unsafe(no_mangle)]
pub extern "C" fn syscall_lookup(sysno: u64) -> *const syscall_entry {
    if (sysno as usize) >= SYSCALL_TABLE.len() {
        return ptr::null();
    }
    let entry = &SYSCALL_TABLE[sysno as usize];
    if entry.handler.is_none() {
        ptr::null()
    } else {
        entry as *const syscall_entry
    }
}
