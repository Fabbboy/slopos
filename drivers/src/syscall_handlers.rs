//! Syscall handler implementations
//!
//! All handlers use SyscallContext for safe pointer access.

use core::ffi::{c_char, c_void};
use core::ptr;

use slopos_abi::syscall::*;

use crate::input_event;
use crate::random;
use crate::serial;
use crate::wl_currency;
use crate::syscall_common::{
    SyscallDisposition, SyscallEntry, USER_IO_MAX_BYTES, syscall_bounded_from_user,
    syscall_copy_to_user_bounded, syscall_return_err,
};
use crate::syscall_context::SyscallContext;
use crate::syscall_fs::{
    syscall_fs_close, syscall_fs_list, syscall_fs_mkdir, syscall_fs_open, syscall_fs_read,
    syscall_fs_stat, syscall_fs_unlink, syscall_fs_write,
};
use crate::syscall_types::{
    InterruptFrame, Task, TaskExitReason, TaskFaultReason,
};
use crate::video_bridge;
use slopos_lib::klog_debug;

use crate::fate;
use slopos_abi::sched_traits::FateResult;

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::paging;

use crate::sched_bridge;

use crate::irq;
use crate::pit::{pit_get_frequency, pit_poll_delay_ms, pit_sleep_ms};
use crate::tty::{tty_get_focus, tty_read_char_blocking, tty_read_line, tty_set_focus};

// =============================================================================
// Simple handlers (no special requirements)
// =============================================================================

pub fn syscall_yield(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let _ = ctx.ok(0);
    sched_bridge::yield_cpu();
    SyscallDisposition::Ok
}

define_syscall!(syscall_random_next(ctx, args) {
    let value = random::random_next();
    ctx.ok(value as u64)
});

define_syscall!(syscall_get_time_ms(ctx, args) {
    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    let ms = (ticks * 1000) / freq as u64;
    ctx.ok(ms)
});

define_syscall!(syscall_shm_get_formats(ctx, args) {
    let formats = slopos_mm::shared_memory::shm_get_formats();
    ctx.ok(formats as u64)
});

pub fn syscall_halt(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    sched_bridge::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

define_syscall!(syscall_sleep_ms(ctx, args) {
    let mut ms = args.arg0;
    if ms > 60000 {
        ms = 60000;
    }
    if sched_bridge::scheduler_is_preemption_enabled() != 0 {
        pit_sleep_ms(ms as u32);
    } else {
        pit_poll_delay_ms(ms as u32);
    }
    ctx.ok(0)
});

// =============================================================================
// Handlers requiring valid task
// =============================================================================

pub fn syscall_exit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let ctx = SyscallContext::new(task, frame);
    if let Some(ref c) = ctx {
        if let Some(t) = c.task_mut() {
            t.exit_reason = TaskExitReason::Normal;
            t.fault_reason = TaskFaultReason::None;
            t.exit_code = 0;
        }
    }
    let task_id = ctx.as_ref().and_then(|c| c.task_id()).unwrap_or(u32::MAX);
    sched_bridge::task_terminate(task_id);
    sched_bridge::schedule();
    SyscallDisposition::NoReturn
}

define_syscall!(syscall_surface_commit(ctx, args, task_id) requires task_id {
    let rc = video_bridge::surface_commit(task_id);
    if rc < 0 { ctx.err_loss() } else { ctx.ok_win(0) }
});

define_syscall!(syscall_surface_frame(ctx, args, task_id) requires task_id {
    let rc = video_bridge::surface_request_frame_callback(task_id);
    if rc < 0 { ctx.err_loss() } else { ctx.ok_win(0) }
});

pub fn syscall_poll_frame_done(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok(0);
    }
    let timestamp = video_bridge::surface_poll_frame_done(task_id);
    ctx.ok(timestamp)
}

pub fn syscall_buffer_age(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok(0);
    }
    let age = video_bridge::surface_get_buffer_age(task_id);
    ctx.ok(age as u64)
}

define_syscall!(syscall_shm_poll_released(ctx, args) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_poll_released(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_surface_damage(ctx, args, task_id) requires task_id {
    let x = args.arg0_i32();
    let y = args.arg1_i32();
    let width = args.arg2_i32();
    let height = args.arg3_i32();
    let rc = video_bridge::surface_add_damage(task_id, x, y, width, height);
    if rc < 0 { ctx.err_loss() } else { ctx.ok_win(0) }
});

define_syscall!(syscall_shm_create(ctx, args, process_id) requires process_id {
    let size = args.arg0;
    let flags = args.arg1_u32();
    let token = slopos_mm::shared_memory::shm_create(process_id, size, flags);
    if token == 0 { ctx.err_loss() } else { ctx.ok_win(token as u64) }
});

define_syscall!(syscall_shm_map(ctx, args, process_id) requires process_id {
    let token = args.arg0_u32();
    let access_val = args.arg1_u32();
    let access = match slopos_mm::shared_memory::ShmAccess::from_u32(access_val) {
        Some(a) => a,
        None => return ctx.err_loss(),
    };
    let vaddr = slopos_mm::shared_memory::shm_map(process_id, token, access);
    if vaddr == 0 { ctx.err_loss() } else { ctx.ok_win(vaddr) }
});

define_syscall!(syscall_shm_unmap(ctx, args, process_id) requires process_id {
    let vaddr = args.arg0;
    let result = slopos_mm::shared_memory::shm_unmap(process_id, vaddr);
    check_result!(ctx, result);
    ctx.ok_win(0)
});

define_syscall!(syscall_shm_destroy(ctx, args, process_id) requires process_id {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_destroy(process_id, token);
    check_result!(ctx, result);
    ctx.ok_win(0)
});

define_syscall!(syscall_surface_attach(ctx, args, task_id, process_id) requires task_and_process {
    let token = args.arg0_u32();
    let width = args.arg1_u32();
    let height = args.arg2_u32();
    // surface_attach checks ownership using process_id
    let result = slopos_mm::shared_memory::surface_attach(process_id, token, width, height);
    check_result!(ctx, result);
    // video_bridge still uses task_id for surface registration
    let video_result = video_bridge::register_surface(task_id, width, height, token);
    check_result!(ctx, video_result);
    ctx.ok_win(0)
});

define_syscall!(syscall_shm_create_with_format(ctx, args, task_id) requires task_id {
    let size = args.arg0;
    let format_val = args.arg1_u32();
    let format = match slopos_mm::shared_memory::PixelFormat::from_u32(format_val) {
        Some(f) => f,
        None => return ctx.err_loss(),
    };
    let token = slopos_mm::shared_memory::shm_create_with_format(task_id, size, format);
    if token == 0 { ctx.err_loss() } else { ctx.ok_win(token as u64) }
});

pub fn syscall_surface_set_role(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }
    let args = ctx.args();
    let role = args.arg0 as u8;
    let result = video_bridge::surface_set_role(task_id, role);
    ctx.ok(result as u64)
}

pub fn syscall_surface_set_parent(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }
    let args = ctx.args();
    let parent_task_id = args.arg0_u32();
    let result = video_bridge::surface_set_parent(task_id, parent_task_id);
    ctx.ok(result as u64)
}

pub fn syscall_surface_set_rel_pos(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }
    let args = ctx.args();
    let rel_x = args.arg0_i32();
    let rel_y = args.arg1_i32();
    let result = video_bridge::surface_set_relative_position(task_id, rel_x, rel_y);
    ctx.ok(result as u64)
}

pub fn syscall_surface_set_title(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }
    let args = ctx.args();
    let title_ptr = args.arg0_const_ptr::<u8>();
    let title_len = args.arg1_usize();

    if title_ptr.is_null() || title_len == 0 {
        return ctx.ok((-1i64) as u64);
    }

    let copy_len = title_len.min(31);
    let title_slice = unsafe { core::slice::from_raw_parts(title_ptr, copy_len) };
    let result = video_bridge::surface_set_title(task_id, title_slice);
    ctx.ok(result as u64)
}

pub fn syscall_input_poll(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }

    let args = ctx.args();
    let event_ptr = args.arg0_ptr::<input_event::InputEvent>();

    if event_ptr.is_null() {
        return ctx.ok((-1i64) as u64);
    }

    // Auto-set pointer focus to compositor if not set
    if ctx.is_compositor() && input_event::input_get_pointer_focus() == 0 {
        input_event::input_set_pointer_focus(task_id, 0);
    }

    if let Some(event) = input_event::input_poll(task_id) {
        unsafe { *event_ptr = event; }
        ctx.ok(1)
    } else {
        ctx.ok(0)
    }
}

pub fn syscall_input_poll_batch(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok(0);
    }

    let args = ctx.args();
    let buffer_ptr = args.arg0_ptr::<input_event::InputEvent>();
    let max_count = args.arg1_usize();

    if buffer_ptr.is_null() || max_count == 0 {
        return ctx.ok(0);
    }

    // Auto-set pointer focus to compositor if not set
    if ctx.is_compositor() && input_event::input_get_pointer_focus() == 0 {
        input_event::input_set_pointer_focus(task_id, 0);
    }

    let count = input_event::input_drain_batch(task_id, buffer_ptr, max_count);
    ctx.ok(count as u64)
}

pub fn syscall_input_has_events(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok(0);
    }
    let count = input_event::input_event_count(task_id);
    ctx.ok(count as u64)
}

pub fn syscall_input_set_focus(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let task_id = ctx.task_id().unwrap_or(0);
    if task_id == 0 {
        return ctx.ok((-1i64) as u64);
    }

    let args = ctx.args();
    let target_task_id = args.arg0_u32();
    let focus_type = args.arg1_u32();

    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    let timestamp_ms = (ticks * 1000) / freq as u64;

    match focus_type {
        0 => input_event::input_set_keyboard_focus(target_task_id),
        1 => input_event::input_set_pointer_focus(target_task_id, timestamp_ms),
        _ => return ctx.ok((-1i64) as u64),
    }
    ctx.ok(0)
}

// Set pointer focus with window offset for coordinate translation.
// SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET (65)
define_syscall!(syscall_input_set_focus_with_offset(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let offset_x = args.arg1 as i32;
    let offset_y = args.arg2 as i32;
    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    let timestamp_ms = (ticks * 1000) / freq as u64;
    input_event::input_set_pointer_focus_with_offset(target_task_id, offset_x, offset_y, timestamp_ms);
    ctx.ok(0)
});

// SYSCALL_INPUT_GET_POINTER_POS (66)
// Get current global pointer position (compositor use)
define_syscall!(syscall_input_get_pointer_pos(ctx, args) requires compositor {
    let (x, y) = input_event::input_get_pointer_position();
    // Pack x and y into a single u64: upper 32 = x, lower 32 = y
    let result = ((x as u32 as u64) << 32) | (y as u32 as u64);
    ctx.ok(result)
});

// SYSCALL_INPUT_GET_BUTTON_STATE (67)
// Get current global pointer button state (compositor use)
define_syscall!(syscall_input_get_button_state(ctx, args) requires compositor {
    let buttons = input_event::input_get_button_state();
    ctx.ok(buttons as u64)
});

// =============================================================================
// Compositor-only handlers
// =============================================================================

define_syscall!(syscall_tty_set_focus(ctx, args) requires compositor {
    let target = args.arg0_u32();
    if tty_set_focus(target) != 0 {
        ctx.err_loss()
    } else {
        wl_currency::award_win();
        ctx.ok(tty_get_focus() as u64)
    }
});

define_syscall!(syscall_enumerate_windows(ctx, args) requires compositor {
    let out_buffer = args.arg0_ptr::<video_bridge::WindowInfo>();
    let max_count = args.arg1_u32();
    if out_buffer.is_null() || max_count == 0 {
        return ctx.err_loss();
    }
    let count = video_bridge::surface_enumerate_windows(out_buffer, max_count);
    ctx.ok(count as u64)
});

define_syscall!(syscall_set_window_position(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let x = args.arg1_i32();
    let y = args.arg2_i32();
    let rc = video_bridge::surface_set_window_position(target_task_id, x, y);
    if rc < 0 { ctx.err_loss() } else { ctx.ok(0) }
});

define_syscall!(syscall_set_window_state(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let state = args.arg1 as u8;
    let rc = video_bridge::surface_set_window_state(target_task_id, state);
    if rc < 0 { ctx.err_loss() } else { ctx.ok(0) }
});

define_syscall!(syscall_raise_window(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let rc = video_bridge::surface_raise_window(target_task_id);
    if rc < 0 { ctx.err_loss() } else { ctx.ok(0) }
});

define_syscall!(syscall_fb_flip(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let phys_addr = slopos_mm::shared_memory::shm_get_phys_addr(token);
    let size = slopos_mm::shared_memory::shm_get_size(token);
    if phys_addr == 0 || size == 0 {
        return ctx.err_loss();
    }
    let fb_info = video_bridge::framebuffer_get_info();
    if fb_info.is_null() {
        return ctx.err_loss();
    }
    let result = video_bridge::fb_flip_from_shm(phys_addr, size);
    check_result!(ctx, result);
    ctx.ok_win(0)
});

define_syscall!(syscall_drain_queue(ctx, args) requires compositor {
    video_bridge::drain_queue();
    ctx.ok(0)
});

define_syscall!(syscall_shm_acquire(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_acquire(token);
    if result < 0 {
        wl_currency::award_loss();
        ctx.ok(result as u64)
    } else {
        ctx.ok_win(0)
    }
});

define_syscall!(syscall_shm_release(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_release(token);
    if result < 0 {
        wl_currency::award_loss();
        ctx.ok(result as u64)
    } else {
        ctx.ok_win(0)
    }
});

define_syscall!(syscall_mark_frames_done(ctx, args) requires compositor {
    let present_time_ms = args.arg0;
    video_bridge::surface_mark_frames_done(present_time_ms);
    ctx.ok(0)
});

// =============================================================================
// Display exclusive handlers
// =============================================================================

define_syscall!(syscall_roulette_draw(ctx, args) requires display_exclusive {
    let fate = args.arg0_u32();
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video_bridge::roulette_draw(fate);
    let disp = if rc.is_ok() {
        ctx.ok_win(0)
    } else {
        ctx.err_loss()
    };
    let _ = paging::switch_page_directory(original_dir);
    disp
});

// =============================================================================
// Complex handlers
// =============================================================================

pub fn syscall_roulette_spin(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let res = sched_bridge::fate_spin();
    let task_id = match ctx.task_id() {
        Some(id) => id,
        None => return ctx.err(),
    };

    if sched_bridge::fate_set_pending(res, task_id) != 0 {
        return ctx.err();
    }
    let packed = ((res.token as u64) << 32) | res.value as u64;
    ctx.ok(packed)
}

pub fn syscall_roulette_result(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let task_id = match ctx.task_id() {
        Some(id) => id,
        None => return ctx.err(),
    };

    let mut stored = FateResult { token: 0, value: 0 };
    if sched_bridge::fate_take_pending(task_id, &mut stored) != 0 {
        return ctx.err();
    }

    let args = ctx.args();
    let token = (args.arg0 >> 32) as u32;
    if token != stored.token {
        return ctx.err();
    }

    let is_win = (stored.value & 1) == 1;

    if is_win {
        sched_bridge::fate_apply_outcome(&stored, 0, true);
        fate::fate_notify_outcome(&stored as *const FateResult);
        ctx.ok(0)
    } else {
        sched_bridge::fate_apply_outcome(&stored, 0, false);
        sched_bridge::kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const c_char);
    }
}

pub fn syscall_user_write(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let args = ctx.args();
    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let mut write_len: usize = 0;

    if args.arg0 == 0
        || syscall_bounded_from_user(
            tmp.as_mut_ptr() as *mut c_void,
            tmp.len(),
            args.arg0 as *const c_void,
            args.arg1,
            USER_IO_MAX_BYTES,
            &mut write_len as *mut usize,
        ) != 0
    {
        return ctx.err();
    }

    let text = core::str::from_utf8(&tmp[..write_len]).unwrap_or("");
    serial::write_str(text);
    ctx.ok(write_len as u64)
}

pub fn syscall_user_read(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let args = ctx.args();
    if args.arg0 == 0 || args.arg1 == 0 {
        return ctx.err();
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let max_len = if args.arg1_usize() > USER_IO_MAX_BYTES {
        USER_IO_MAX_BYTES
    } else {
        args.arg1_usize()
    };

    let mut read_len = tty_read_line(tmp.as_mut_ptr(), max_len);
    if max_len > 0 {
        read_len = read_len.min(max_len.saturating_sub(1));
        tmp[read_len] = 0;
    }

    let copy_len = read_len.saturating_add(1).min(max_len);
    if syscall_copy_to_user_bounded(
        args.arg0 as *mut c_void,
        tmp.as_ptr() as *const c_void,
        copy_len,
    ) != 0
    {
        return ctx.err();
    }

    ctx.ok(read_len as u64)
}

define_syscall!(syscall_user_read_char(ctx, args) {
    let mut c = 0u8;
    if tty_read_char_blocking(&mut c as *mut u8) != 0 {
        return ctx.err();
    }
    ctx.ok(c as u64)
});

pub fn syscall_fb_info(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let info = video_bridge::framebuffer_get_info();
    if info.is_null() {
        return ctx.err();
    }
    let info_local = unsafe { core::ptr::read(info) };
    if info_local.initialized == 0 {
        klog_debug!("syscall_fb_info: framebuffer not initialized");
        return ctx.err();
    }

    let user_info = UserFbInfo {
        width: info_local.width,
        height: info_local.height,
        pitch: info_local.pitch,
        bpp: info_local.bpp as u8,
        pixel_format: info_local.pixel_format as u8,
    };

    let args = ctx.args();
    if syscall_copy_to_user_bounded(
        args.arg0 as *mut c_void,
        &user_info as *const _ as *const c_void,
        core::mem::size_of::<UserFbInfo>(),
    ) != 0
    {
        return ctx.err();
    }

    ctx.ok(0)
}

pub fn syscall_sys_info(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let args = ctx.args();
    if args.arg0 == 0 {
        return ctx.err();
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

    get_page_allocator_stats(
        &mut info.total_pages,
        &mut info.free_pages,
        &mut info.allocated_pages,
    );
    sched_bridge::get_task_stats(
        &mut info.total_tasks,
        &mut info.active_tasks,
        &mut info.task_context_switches,
    );
    sched_bridge::get_scheduler_stats(
        &mut info.scheduler_context_switches,
        &mut info.scheduler_yields,
        &mut info.ready_tasks,
        &mut info.schedule_calls,
    );

    if syscall_copy_to_user_bounded(
        args.arg0 as *mut c_void,
        &info as *const _ as *const c_void,
        core::mem::size_of::<UserSysInfo>(),
    ) != 0
    {
        return ctx.err();
    }

    ctx.ok(0)
}

// =============================================================================
// Task spawning syscall
// =============================================================================

/// Type for the task spawn callback function.
/// Takes a task name slice and returns task_id (> 0) on success, -1 on failure.
pub type SpawnTaskFn = fn(&[u8]) -> i32;

/// Global callback for spawning tasks, registered by userland at boot.
static SPAWN_TASK_CALLBACK: spin::Mutex<Option<SpawnTaskFn>> = spin::Mutex::new(None);

/// Register the task spawn callback (called by userland bootstrap at init).
pub fn register_spawn_task_callback(callback: SpawnTaskFn) {
    *SPAWN_TASK_CALLBACK.lock() = Some(callback);
}

pub fn syscall_spawn_task(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let args = ctx.args();

    // Get task name from user space
    let name_ptr = args.arg0 as *const u8;
    let name_len = args.arg1 as usize;

    // Validate and copy name from user space
    if name_ptr.is_null() || name_len == 0 || name_len > 64 {
        return ctx.err();
    }

    let mut name_buf = [0u8; 64];
    let mut copied_len: usize = 0;
    if syscall_bounded_from_user(
        name_buf.as_mut_ptr() as *mut c_void,
        name_buf.len(),
        name_ptr as *const c_void,
        name_len as u64,
        64,
        &mut copied_len,
    ) != 0
    {
        return ctx.err();
    }

    // Call the registered spawn callback
    let callback = *SPAWN_TASK_CALLBACK.lock();
    let result = match callback {
        Some(spawn_fn) => spawn_fn(&name_buf[..copied_len]),
        None => -1, // No callback registered
    };

    if result > 0 {
        ctx.ok(result as u64)
    } else {
        ctx.err()
    }
}

// =============================================================================
// Syscall table and dispatch
// =============================================================================

static SYSCALL_TABLE: [SyscallEntry; 128] = {
    let mut table: [SyscallEntry; 128] = [SyscallEntry {
        handler: None,
        name: core::ptr::null(),
    }; 128];
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
    table[SYSCALL_SURFACE_COMMIT as usize] = SyscallEntry {
        handler: Some(syscall_surface_commit),
        name: b"surface_commit\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_GET_TIME_MS as usize] = SyscallEntry {
        handler: Some(syscall_get_time_ms),
        name: b"get_time_ms\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_CREATE as usize] = SyscallEntry {
        handler: Some(syscall_shm_create),
        name: b"shm_create\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_MAP as usize] = SyscallEntry {
        handler: Some(syscall_shm_map),
        name: b"shm_map\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_UNMAP as usize] = SyscallEntry {
        handler: Some(syscall_shm_unmap),
        name: b"shm_unmap\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_DESTROY as usize] = SyscallEntry {
        handler: Some(syscall_shm_destroy),
        name: b"shm_destroy\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_ATTACH as usize] = SyscallEntry {
        handler: Some(syscall_surface_attach),
        name: b"surface_attach\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FB_FLIP as usize] = SyscallEntry {
        handler: Some(syscall_fb_flip),
        name: b"fb_flip\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_DRAIN_QUEUE as usize] = SyscallEntry {
        handler: Some(syscall_drain_queue),
        name: b"drain_queue\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_ACQUIRE as usize] = SyscallEntry {
        handler: Some(syscall_shm_acquire),
        name: b"shm_acquire\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_RELEASE as usize] = SyscallEntry {
        handler: Some(syscall_shm_release),
        name: b"shm_release\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_POLL_RELEASED as usize] = SyscallEntry {
        handler: Some(syscall_shm_poll_released),
        name: b"shm_poll_released\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_FRAME as usize] = SyscallEntry {
        handler: Some(syscall_surface_frame),
        name: b"surface_frame\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_POLL_FRAME_DONE as usize] = SyscallEntry {
        handler: Some(syscall_poll_frame_done),
        name: b"poll_frame_done\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_MARK_FRAMES_DONE as usize] = SyscallEntry {
        handler: Some(syscall_mark_frames_done),
        name: b"mark_frames_done\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_GET_FORMATS as usize] = SyscallEntry {
        handler: Some(syscall_shm_get_formats),
        name: b"shm_get_formats\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_CREATE_WITH_FORMAT as usize] = SyscallEntry {
        handler: Some(syscall_shm_create_with_format),
        name: b"shm_create_with_format\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_DAMAGE as usize] = SyscallEntry {
        handler: Some(syscall_surface_damage),
        name: b"surface_damage\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_BUFFER_AGE as usize] = SyscallEntry {
        handler: Some(syscall_buffer_age),
        name: b"buffer_age\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_SET_ROLE as usize] = SyscallEntry {
        handler: Some(syscall_surface_set_role),
        name: b"surface_set_role\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_SET_PARENT as usize] = SyscallEntry {
        handler: Some(syscall_surface_set_parent),
        name: b"surface_set_parent\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_SET_REL_POS as usize] = SyscallEntry {
        handler: Some(syscall_surface_set_rel_pos),
        name: b"surface_set_rel_pos\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SURFACE_SET_TITLE as usize] = SyscallEntry {
        handler: Some(syscall_surface_set_title),
        name: b"surface_set_title\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_POLL as usize] = SyscallEntry {
        handler: Some(syscall_input_poll),
        name: b"input_poll\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_POLL_BATCH as usize] = SyscallEntry {
        handler: Some(syscall_input_poll_batch),
        name: b"input_poll_batch\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_HAS_EVENTS as usize] = SyscallEntry {
        handler: Some(syscall_input_has_events),
        name: b"input_has_events\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_SET_FOCUS as usize] = SyscallEntry {
        handler: Some(syscall_input_set_focus),
        name: b"input_set_focus\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET as usize] = SyscallEntry {
        handler: Some(syscall_input_set_focus_with_offset),
        name: b"input_set_focus_with_offset\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_GET_POINTER_POS as usize] = SyscallEntry {
        handler: Some(syscall_input_get_pointer_pos),
        name: b"input_get_pointer_pos\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_INPUT_GET_BUTTON_STATE as usize] = SyscallEntry {
        handler: Some(syscall_input_get_button_state),
        name: b"input_get_button_state\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SPAWN_TASK as usize] = SyscallEntry {
        handler: Some(syscall_spawn_task),
        name: b"spawn_task\0".as_ptr() as *const c_char,
    };
    table
};

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
