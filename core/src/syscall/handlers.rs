use core::ffi::c_char;
use core::ptr;

use slopos_abi::DisplayInfo;
use slopos_abi::InputEvent;
use slopos_abi::WindowInfo;
use slopos_abi::fate::FateResult;
use slopos_abi::syscall::*;

use crate::exec;

use crate::platform;
use crate::syscall::common::{
    SyscallDisposition, SyscallEntry, USER_IO_MAX_BYTES, syscall_bounded_from_user,
    syscall_copy_to_user_bounded, syscall_copy_user_str, syscall_return_err,
};
use crate::syscall::context::SyscallContext;
use crate::syscall::fs::{
    syscall_fs_close, syscall_fs_list, syscall_fs_mkdir, syscall_fs_open, syscall_fs_read,
    syscall_fs_stat, syscall_fs_unlink, syscall_fs_write,
};
use crate::syscall_services::{fate as fate_svc, input, tty, video};
use crate::{
    fate_apply_outcome, fate_set_pending, fate_spin, fate_take_pending, get_scheduler_stats,
    get_task_stats, schedule, scheduler_is_preemption_enabled, task_terminate, yield_,
};

use slopos_abi::task::{Task, TaskExitReason, TaskFaultReason};
use slopos_lib::InterruptFrame;
use slopos_lib::klog_debug;
use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::paging;
use slopos_mm::user_copy::copy_to_user;
use slopos_mm::user_ptr::UserPtr;

pub fn syscall_yield(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let _ = ctx.ok(0);
    yield_();
    SyscallDisposition::Ok
}

define_syscall!(syscall_random_next(ctx, args) {
    let value = platform::rng_next();
    ctx.ok(value)
});

define_syscall!(syscall_get_time_ms(ctx, args) {
    let ms = platform::get_time_ms();
    ctx.ok(ms)
});

define_syscall!(syscall_shm_get_formats(ctx, args) {
    let formats = slopos_mm::shared_memory::shm_get_formats();
    ctx.ok(formats as u64)
});

pub fn syscall_halt(_task: *mut Task, _frame: *mut InterruptFrame) -> SyscallDisposition {
    platform::kernel_shutdown(b"user halt\0".as_ptr() as *const c_char);
    #[allow(unreachable_code)]
    SyscallDisposition::Ok
}

define_syscall!(syscall_sleep_ms(ctx, args) {
    let mut ms = args.arg0;
    if ms > 60000 {
        ms = 60000;
    }
    if scheduler_is_preemption_enabled() != 0 {
        crate::platform::timer_sleep_ms(ms as u32);
    } else {
        crate::platform::timer_poll_delay_ms(ms as u32);
    }
    ctx.ok(0)
});

pub fn syscall_exit(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let ctx = SyscallContext::new(task, frame);
    let task_id = ctx.as_ref().and_then(|c| c.task_id()).unwrap_or(u32::MAX);
    klog_debug!("SYSCALL_EXIT: task {} entering exit", task_id);
    if let Some(ref c) = ctx {
        if let Some(t) = c.task_mut() {
            t.exit_reason = TaskExitReason::Normal;
            t.fault_reason = TaskFaultReason::None;
            t.exit_code = 0;
        }
    }
    klog_debug!("SYSCALL_EXIT: task {} calling task_terminate", task_id);
    task_terminate(task_id);
    klog_debug!("SYSCALL_EXIT: task {} calling schedule", task_id);
    schedule();
    klog_debug!(
        "SYSCALL_EXIT: task {} schedule returned (should not happen)",
        task_id
    );
    SyscallDisposition::NoReturn
}

define_syscall!(syscall_surface_commit(ctx, args, task_id) requires task_id {
    let rc = video::surface_commit(task_id);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_surface_frame(ctx, args, task_id) requires task_id {
    let rc = video::surface_request_frame_callback(task_id);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_poll_frame_done(ctx, args, task_id) requires task_id {
    let timestamp = video::surface_poll_frame_done(task_id);
    ctx.ok(timestamp)
});

define_syscall!(syscall_buffer_age(ctx, args, task_id) requires task_id {
    let age = video::surface_get_buffer_age(task_id);
    ctx.ok(age as u64)
});

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
    let rc = video::surface_add_damage(task_id, x, y, width, height);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_shm_create(ctx, args, process_id) requires process_id {
    let size = args.arg0;
    let flags = args.arg1_u32();
    let token = slopos_mm::shared_memory::shm_create(process_id, size, flags);
    if token == 0 { ctx.err() } else { ctx.ok(token as u64) }
});

define_syscall!(syscall_shm_map(ctx, args, process_id) requires process_id {
    let token = args.arg0_u32();
    let access_val = args.arg1_u32();
    let access = match slopos_mm::shared_memory::ShmAccess::from_u32(access_val) {
        Some(a) => a,
        None => return ctx.err(),
    };
    let vaddr = slopos_mm::shared_memory::shm_map(process_id, token, access);
    if vaddr == 0 { ctx.err() } else { ctx.ok(vaddr) }
});

define_syscall!(syscall_shm_unmap(ctx, args, process_id) requires process_id {
    let vaddr = args.arg0;
    let result = slopos_mm::shared_memory::shm_unmap(process_id, vaddr);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_shm_destroy(ctx, args, process_id) requires process_id {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_destroy(process_id, token);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_surface_attach(ctx, args, task_id, process_id) requires task_and_process {
    let token = args.arg0_u32();
    let width = args.arg1_u32();
    let height = args.arg2_u32();
    let result = slopos_mm::shared_memory::surface_attach(process_id, token, width, height);
    check_result!(ctx, result);
    let video_result = video::register_surface(task_id, width, height, token);
    check_result!(ctx, video_result);
    ctx.ok(0)
});

define_syscall!(syscall_shm_create_with_format(ctx, args, task_id) requires task_id {
    let size = args.arg0;
    let format_val = args.arg1_u32();
    let format = match slopos_mm::shared_memory::PixelFormat::from_u32(format_val) {
        Some(f) => f,
        None => return ctx.err(),
    };
    let token = slopos_mm::shared_memory::shm_create_with_format(task_id, size, format);
    if token == 0 { ctx.err() } else { ctx.ok(token as u64) }
});

define_syscall!(syscall_surface_set_role(ctx, args, task_id) requires task_id {
    let role = args.arg0 as u8;
    let result = video::surface_set_role(task_id, role);
    ctx.ok(result as u64)
});

define_syscall!(syscall_surface_set_parent(ctx, args, task_id) requires task_id {
    let parent_task_id = args.arg0_u32();
    let result = video::surface_set_parent(task_id, parent_task_id);
    ctx.ok(result as u64)
});

define_syscall!(syscall_surface_set_rel_pos(ctx, args, task_id) requires task_id {
    let rel_x = args.arg0_i32();
    let rel_y = args.arg1_i32();
    let result = video::surface_set_relative_position(task_id, rel_x, rel_y);
    ctx.ok(result as u64)
});

pub fn syscall_surface_set_title(
    task: *mut Task,
    frame: *mut InterruptFrame,
) -> SyscallDisposition {
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
    let result = video::surface_set_title(task_id, title_slice);
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
    let event_ptr = args.arg0_ptr::<InputEvent>();

    if event_ptr.is_null() {
        return ctx.ok((-1i64) as u64);
    }

    if ctx.is_compositor() && input::input_get_pointer_focus() == 0 {
        input::input_set_pointer_focus(task_id, 0);
    }

    if let Some(event) = input::input_poll(task_id) {
        unsafe {
            *event_ptr = event;
        }
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
    let buffer_ptr = args.arg0_ptr::<InputEvent>();
    let max_count = args.arg1_usize();

    if buffer_ptr.is_null() || max_count == 0 {
        return ctx.ok(0);
    }

    if ctx.is_compositor() && input::input_get_pointer_focus() == 0 {
        input::input_set_pointer_focus(task_id, 0);
    }

    let count = input::input_drain_batch(task_id, buffer_ptr, max_count);
    ctx.ok(count as u64)
}

define_syscall!(syscall_input_has_events(ctx, args, task_id) requires task_id {
    let count = input::input_event_count(task_id);
    ctx.ok(count as u64)
});

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

    let timestamp_ms = platform::get_time_ms();

    match focus_type {
        0 => input::input_set_keyboard_focus(target_task_id),
        1 => input::input_set_pointer_focus(target_task_id, timestamp_ms),
        _ => return ctx.ok((-1i64) as u64),
    }
    ctx.ok(0)
}

define_syscall!(syscall_input_set_focus_with_offset(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let offset_x = args.arg1 as i32;
    let offset_y = args.arg2 as i32;
    let timestamp_ms = platform::get_time_ms();
    input::input_set_pointer_focus_with_offset(target_task_id, offset_x, offset_y, timestamp_ms);
    ctx.ok(0)
});

define_syscall!(syscall_input_get_pointer_pos(ctx, args) requires compositor {
    let (x, y) = input::input_get_pointer_position();
    let result = ((x as u32 as u64) << 32) | (y as u32 as u64);
    ctx.ok(result)
});

define_syscall!(syscall_input_get_button_state(ctx, args) requires compositor {
    let buttons = input::input_get_button_state();
    ctx.ok(buttons as u64)
});

define_syscall!(syscall_tty_set_focus(ctx, args) requires compositor {
    let target = args.arg0_u32();
    if tty::tty_set_focus(target) != 0 {
        ctx.err()
    } else {
        ctx.ok(tty::tty_get_focus() as u64)
    }
});

define_syscall!(syscall_enumerate_windows(ctx, args) requires compositor {
    let out_buffer = args.arg0_ptr::<WindowInfo>();
    let max_count = args.arg1_u32();
    if out_buffer.is_null() || max_count == 0 {
        return ctx.err();
    }
    let count = video::surface_enumerate_windows(out_buffer, max_count);
    ctx.ok(count as u64)
});

define_syscall!(syscall_set_window_position(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let x = args.arg1_i32();
    let y = args.arg2_i32();
    let rc = video::surface_set_window_position(target_task_id, x, y);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_set_window_state(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let state = args.arg1 as u8;
    let rc = video::surface_set_window_state(target_task_id, state);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_raise_window(ctx, args) requires compositor {
    let target_task_id = args.arg0_u32();
    let rc = video::surface_raise_window(target_task_id);
    if rc < 0 { ctx.err() } else { ctx.ok(0) }
});

define_syscall!(syscall_fb_flip(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let phys_addr = slopos_mm::shared_memory::shm_get_phys_addr(token);
    let size = slopos_mm::shared_memory::shm_get_size(token);
    if phys_addr.is_null() || size == 0 {
        return ctx.err();
    }
    if video::get_display_info().is_none() {
        return ctx.err();
    }
    let result = video::fb_flip_from_shm(phys_addr, size);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_drain_queue(ctx, args) requires compositor {
    video::drain_queue();
    ctx.ok(0)
});

define_syscall!(syscall_shm_acquire(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_acquire(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_shm_release(ctx, args) requires compositor {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_release(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_mark_frames_done(ctx, args) requires compositor {
    let present_time_ms = args.arg0;
    video::surface_mark_frames_done(present_time_ms);
    ctx.ok(0)
});

define_syscall!(syscall_roulette_draw(ctx, args) requires display_exclusive {
    let fate = args.arg0_u32();
    let original_dir = paging::get_current_page_directory();
    let kernel_dir = paging::paging_get_kernel_directory();
    let _ = paging::switch_page_directory(kernel_dir);
    let rc = video::roulette_draw(fate);
    let disp = if rc.is_ok() {
        ctx.ok(0)
    } else {
        ctx.err()
    };
    let _ = paging::switch_page_directory(original_dir);
    disp
});

pub fn syscall_roulette_spin(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let res = fate_spin();
    let task_id = match ctx.task_id() {
        Some(id) => id,
        None => return ctx.err(),
    };

    if fate_set_pending(res, task_id) != 0 {
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
    if fate_take_pending(task_id, &mut stored) != 0 {
        return ctx.err();
    }

    let args = ctx.args();
    let token = (args.arg0 >> 32) as u32;
    if token != stored.token {
        return ctx.err();
    }

    let is_win = (stored.value & 1) == 1;

    if is_win {
        fate_apply_outcome(&stored as *const FateResult, 0, true);
        fate_svc::fate_notify_outcome(&stored as *const FateResult);
        ctx.ok(0)
    } else {
        fate_apply_outcome(&stored as *const FateResult, 0, false);
        platform::kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const c_char);
    }
}

pub fn syscall_user_write(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let args = ctx.args();
    let mut tmp = [0u8; USER_IO_MAX_BYTES];

    if args.arg0 == 0 {
        return ctx.err();
    }
    let write_len =
        match syscall_bounded_from_user(&mut tmp, args.arg0, args.arg1, USER_IO_MAX_BYTES) {
            Ok(len) => len,
            Err(_) => return ctx.err(),
        };

    platform::console_puts(&tmp[..write_len]);
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

    let mut read_len = tty::tty_read_line(tmp.as_mut_ptr(), max_len);
    if max_len > 0 {
        read_len = read_len.min(max_len.saturating_sub(1));
        tmp[read_len] = 0;
    }

    let copy_len = read_len.saturating_add(1).min(max_len);
    if syscall_copy_to_user_bounded(args.arg0, &tmp[..copy_len]).is_err() {
        return ctx.err();
    }

    ctx.ok(read_len as u64)
}

define_syscall!(syscall_user_read_char(ctx, args) {
    let mut c = 0u8;
    if tty::tty_read_char_blocking(&mut c as *mut u8) != 0 {
        return ctx.err();
    }
    ctx.ok(c as u64)
});

pub fn syscall_fb_info(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let display_info = match video::get_display_info() {
        Some(info) => info,
        None => {
            klog_debug!("syscall_fb_info: framebuffer not initialized");
            return ctx.err();
        }
    };

    let args = ctx.args();
    let user_ptr = match UserPtr::<DisplayInfo>::try_new(args.arg0) {
        Ok(p) => p,
        Err(_) => return ctx.err(),
    };
    if copy_to_user(user_ptr, &display_info).is_err() {
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

    let user_ptr = match UserPtr::<UserSysInfo>::try_new(args.arg0) {
        Ok(p) => p,
        Err(_) => return ctx.err(),
    };
    if copy_to_user(user_ptr, &info).is_err() {
        return ctx.err();
    }

    ctx.ok(0)
}

pub type SpawnTaskFn = fn(&[u8]) -> i32;

static SPAWN_TASK_CALLBACK: slopos_lib::IrqMutex<Option<SpawnTaskFn>> =
    slopos_lib::IrqMutex::new(None);

pub fn register_spawn_task_callback(callback: SpawnTaskFn) {
    *SPAWN_TASK_CALLBACK.lock() = Some(callback);
}

pub fn syscall_spawn_task(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };
    let args = ctx.args();

    let name_ptr = args.arg0 as *const u8;
    let name_len = args.arg1 as usize;

    if name_ptr.is_null() || name_len == 0 || name_len > 64 {
        return ctx.err();
    }

    let mut name_buf = [0u8; 64];
    let copied_len =
        match syscall_bounded_from_user(&mut name_buf, name_ptr as u64, name_len as u64, 64) {
            Ok(len) => len,
            Err(_) => return ctx.err(),
        };

    let callback = *SPAWN_TASK_CALLBACK.lock();
    let result = match callback {
        Some(spawn_fn) => spawn_fn(&name_buf[..copied_len]),
        None => -1,
    };

    if result > 0 {
        ctx.ok(result as u64)
    } else {
        ctx.err()
    }
}

pub fn syscall_exec(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let process_id = match ctx.require_process_id() {
        Ok(id) => id,
        Err(d) => return d,
    };

    let args = ctx.args();
    let path_ptr = args.arg0;

    if path_ptr == 0 {
        return ctx.err();
    }

    let mut path_buf = [0u8; exec::EXEC_MAX_PATH];
    if syscall_copy_user_str(&mut path_buf, path_ptr).is_err() {
        return ctx.err();
    }

    let path_len = path_buf
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(path_buf.len());
    let path = &path_buf[..path_len];

    let mut entry_point = 0u64;
    let mut stack_ptr = 0u64;

    match exec::do_exec(
        process_id,
        path,
        None,
        None,
        &mut entry_point,
        &mut stack_ptr,
    ) {
        Ok(()) => {
            unsafe {
                (*frame).rip = entry_point;
                (*frame).rsp = stack_ptr;
                (*frame).rax = 0;
                (*frame).rdi = 0;
                (*frame).rsi = 0;
                (*frame).rdx = 0;
                (*frame).rcx = 0;
                (*frame).r8 = 0;
                (*frame).r9 = 0;
                (*frame).r10 = 0;
                (*frame).r11 = 0;
            }
            SyscallDisposition::Ok
        }
        Err(e) => {
            unsafe {
                (*frame).rax = e as i32 as u64;
            }
            SyscallDisposition::Ok
        }
    }
}

define_syscall!(syscall_brk(ctx, args, process_id) requires process_id {
    let new_brk = args.arg0;
    let result = slopos_mm::process_vm::process_vm_brk(process_id, new_brk);
    ctx.ok(result)
});

pub fn syscall_fork(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let child_id = crate::scheduler::task::task_fork(task);
    if child_id == slopos_abi::task::INVALID_TASK_ID {
        return ctx.err();
    }

    ctx.ok(child_id as u64)
}

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
    table[SYSCALL_EXEC as usize] = SyscallEntry {
        handler: Some(syscall_exec),
        name: b"exec\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_BRK as usize] = SyscallEntry {
        handler: Some(syscall_brk),
        name: b"brk\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_FORK as usize] = SyscallEntry {
        handler: Some(syscall_fork),
        name: b"fork\0".as_ptr() as *const c_char,
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
