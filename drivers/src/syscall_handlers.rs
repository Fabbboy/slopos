use core::ffi::{c_char, c_void};
use core::ptr;

use crate::input_event;
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
use slopos_lib::klog_debug;

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
use slopos_mm::paging;

use crate::scheduler_callbacks::{
    call_fate_apply_outcome, call_fate_set_pending, call_fate_spin, call_fate_take_pending,
    call_get_scheduler_stats, call_get_task_stats, call_schedule,
    call_scheduler_is_preemption_enabled, call_task_terminate, call_yield,
};

use crate::irq;
use crate::pit::{pit_get_frequency, pit_poll_delay_ms, pit_sleep_ms};
use crate::scheduler_callbacks::{call_kernel_reboot, call_kernel_shutdown};
use crate::tty::{tty_get_focus, tty_read_char_blocking, tty_read_line, tty_set_focus};

fn task_has_flag(task: *mut Task, flag: u16) -> bool {
    if task.is_null() {
        return false;
    }
    unsafe { (*task).flags & flag != 0 }
}

// Macro: require compositor flag or return error with loss
macro_rules! require_compositor {
    ($task:expr, $frame:expr) => {
        if !task_has_flag($task, TASK_FLAG_COMPOSITOR) {
            wl_currency::award_loss();
            return syscall_return_err($frame, u64::MAX);
        }
    };
}

// Macro: check result != 0, return error with loss
macro_rules! check_result {
    ($result:expr, $frame:expr) => {
        if $result != 0 {
            wl_currency::award_loss();
            return syscall_return_err($frame, u64::MAX);
        }
    };
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

pub fn syscall_tty_set_focus(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    require_compositor!(task, frame);
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

pub fn syscall_enumerate_windows(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    require_compositor!(task, frame);
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
    require_compositor!(task, frame);
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
    require_compositor!(task, frame);
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
    require_compositor!(task, frame);
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
    let disp = if rc.is_ok() {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    } else {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    };
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

// ============================================================================
// Shared Memory Syscalls for Wayland-like Compositor
// ============================================================================

/// SHM_CREATE: Allocate a shared memory buffer
/// rdi = size in bytes, rsi = flags
/// Returns: token on success, u64::MAX on failure
pub fn syscall_shm_create(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let size = unsafe { (*frame).rdi };
    let flags = unsafe { (*frame).rsi as u32 };
    let task_id = unsafe { (*task).task_id };

    let token = slopos_mm::shared_memory::shm_create(task_id, size, flags);
    if token == 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    wl_currency::award_win();
    syscall_return_ok(frame, token as u64)
}

/// SHM_MAP: Map a shared buffer into caller's address space
/// rdi = token, rsi = access (0=RO, 1=RW)
/// Returns: virtual address on success, 0 on failure
pub fn syscall_shm_map(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let token = unsafe { (*frame).rdi as u32 };
    let access_val = unsafe { (*frame).rsi as u32 };
    let task_id = unsafe { (*task).task_id };

    let access = match slopos_mm::shared_memory::ShmAccess::from_u32(access_val) {
        Some(a) => a,
        None => {
            wl_currency::award_loss();
            return syscall_return_err(frame, 0);
        }
    };

    let vaddr = slopos_mm::shared_memory::shm_map(task_id, token, access);
    if vaddr == 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, 0);
    }

    wl_currency::award_win();
    syscall_return_ok(frame, vaddr)
}

/// SHM_UNMAP: Unmap a shared buffer from caller's address space
/// rdi = virtual address
/// Returns: 0 on success, u64::MAX on failure
pub fn syscall_shm_unmap(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let vaddr = unsafe { (*frame).rdi };
    let task_id = unsafe { (*task).task_id };

    let result = slopos_mm::shared_memory::shm_unmap(task_id, vaddr);
    check_result!(result, frame);
    wl_currency::award_win();
    syscall_return_ok(frame, 0)
}

/// SHM_DESTROY: Free a shared buffer (owner only)
/// rdi = token
/// Returns: 0 on success, u64::MAX on failure
pub fn syscall_shm_destroy(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let token = unsafe { (*frame).rdi as u32 };
    let task_id = unsafe { (*task).task_id };

    let result = slopos_mm::shared_memory::shm_destroy(task_id, token);
    check_result!(result, frame);
    wl_currency::award_win();
    syscall_return_ok(frame, 0)
}

/// SURFACE_ATTACH: Register a shared buffer as a window surface
/// rdi = token, rsi = width, rdx = height
/// Returns: 0 on success, u64::MAX on failure
pub fn syscall_surface_attach(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let token = unsafe { (*frame).rdi as u32 };
    let width = unsafe { (*frame).rsi as u32 };
    let height = unsafe { (*frame).rdx as u32 };
    let task_id = unsafe { (*task).task_id };

    // Register the buffer dimensions with the shared memory subsystem
    let result = slopos_mm::shared_memory::surface_attach(task_id, token, width, height);
    check_result!(result, frame);

    // Also register the surface with the video subsystem so it appears in enumerate_windows
    let video_result = video_bridge::register_surface(task_id, width, height, token);
    check_result!(video_result, frame);

    wl_currency::award_win();
    syscall_return_ok(frame, 0)
}

/// FB_FLIP: Copy compositor output buffer to MMIO framebuffer
/// rdi = token (compositor's output buffer)
/// Returns: 0 on success, u64::MAX on failure
/// Only compositor task can call this.
pub fn syscall_fb_flip(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    // Only compositor can flip
    require_compositor!(task, frame);

    let token = unsafe { (*frame).rdi as u32 };

    // Get buffer physical address and size
    let phys_addr = slopos_mm::shared_memory::shm_get_phys_addr(token);
    let size = slopos_mm::shared_memory::shm_get_size(token);

    if phys_addr == 0 || size == 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    // Get framebuffer info
    let fb_info = video_bridge::framebuffer_get_info();
    if fb_info.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    // Copy from shared buffer to framebuffer
    let result = video_bridge::fb_flip_from_shm(phys_addr, size);
    check_result!(result, frame);
    wl_currency::award_win();
    syscall_return_ok(frame, 0)
}

/// DRAIN_QUEUE: Process pending client operations (compositor only)
/// Called by compositor at start of each frame to process queued client operations.
/// Returns: 0 on success
pub fn syscall_drain_queue(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    // Only compositor can drain the queue
    require_compositor!(task, frame);

    video_bridge::drain_queue();
    syscall_return_ok(frame, 0)
}

/// SHM_ACQUIRE: Compositor acquires a buffer reference
/// Increments refcount and clears released flag.
/// arg0: token
/// Returns: 0 on success, -1 on failure
pub fn syscall_shm_acquire(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    // Only compositor can acquire buffers
    require_compositor!(task, frame);

    let token = unsafe { (*frame).rdi as u32 };
    let result = slopos_mm::shared_memory::shm_acquire(token);
    if result < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, result as u64)
    } else {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    }
}

/// SHM_RELEASE: Compositor releases a buffer reference
/// Decrements refcount and sets released flag.
/// arg0: token
/// Returns: 0 on success, -1 on failure
pub fn syscall_shm_release(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    // Only compositor can release buffers
    require_compositor!(task, frame);

    let token = unsafe { (*frame).rdi as u32 };
    let result = slopos_mm::shared_memory::shm_release(token);
    if result < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, result as u64)
    } else {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    }
}

/// SHM_POLL_RELEASED: Client polls to check if buffer was released by compositor
/// arg0: token
/// Returns: 1 if released, 0 if not released, -1 on error
pub fn syscall_shm_poll_released(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let token = unsafe { (*frame).rdi as u32 };
    let result = slopos_mm::shared_memory::shm_poll_released(token);

    // Don't award W/L for polling - it's informational
    syscall_return_ok(frame, result as u64)
}

// =============================================================================
// Frame Callback Syscalls (Wayland wl_surface.frame)
// =============================================================================

/// SURFACE_FRAME: Request a frame callback (client API)
/// Called by clients to request notification when frame is presented.
/// arg0: (none - uses caller's task_id)
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_frame(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    let task_id = unsafe { (*task).task_id };
    let rc = video_bridge::surface_request_frame_callback(task_id);

    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    }
}

/// POLL_FRAME_DONE: Poll for frame completion (client API)
/// Returns presentation timestamp if frame was presented, 0 if still pending.
/// arg0: (none - uses caller's task_id)
/// Returns: timestamp (ms since boot) if done, 0 if pending
pub fn syscall_poll_frame_done(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, 0);
    }

    let task_id = unsafe { (*task).task_id };
    let timestamp = video_bridge::surface_poll_frame_done(task_id);

    // Don't award W/L for polling - it's informational
    syscall_return_ok(frame, timestamp)
}

/// MARK_FRAMES_DONE: Compositor signals frame completion (compositor only)
/// Called by compositor after presenting a frame to notify all clients.
/// arg0: present_time_ms
/// Returns: 0
pub fn syscall_mark_frames_done(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    require_compositor!(task, frame);

    let present_time_ms = unsafe { (*frame).rdi };
    video_bridge::surface_mark_frames_done(present_time_ms);

    syscall_return_ok(frame, 0)
}

// =============================================================================
// Pixel Format Negotiation Syscalls (Wayland wl_shm)
// =============================================================================

/// SHM_GET_FORMATS: Get bitmap of supported pixel formats
/// Returns: bitmap where bit N is set if PixelFormat N is supported
pub fn syscall_shm_get_formats(_task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let formats = slopos_mm::shared_memory::shm_get_formats();
    syscall_return_ok(frame, formats as u64)
}

/// SHM_CREATE_WITH_FORMAT: Create a shared buffer with specific pixel format
/// arg0: size in bytes
/// arg1: format (PixelFormat enum value)
/// Returns: token on success, 0 on failure
pub fn syscall_shm_create_with_format(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let size = unsafe { (*frame).rdi };
    let format_val = unsafe { (*frame).rsi as u32 };
    let task_id = unsafe { (*task).task_id };

    let format = match slopos_mm::shared_memory::PixelFormat::from_u32(format_val) {
        Some(f) => f,
        None => {
            wl_currency::award_loss();
            return syscall_return_err(frame, 0);
        }
    };

    let token = slopos_mm::shared_memory::shm_create_with_format(task_id, size, format);
    if token == 0 {
        wl_currency::award_loss();
        return syscall_return_err(frame, 0);
    }

    wl_currency::award_win();
    syscall_return_ok(frame, token as u64)
}

// =============================================================================
// Damage Tracking Syscalls (Wayland wl_surface.damage)
// =============================================================================

/// SURFACE_DAMAGE: Add damage region to surface's back buffer
/// arg0: x
/// arg1: y
/// arg2: width
/// arg3: height
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_damage(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        wl_currency::award_loss();
        return syscall_return_err(frame, u64::MAX);
    }

    let task_id = unsafe { (*task).task_id };
    let x = unsafe { (*frame).rdi as i32 };
    let y = unsafe { (*frame).rsi as i32 };
    let width = unsafe { (*frame).rdx as i32 };
    let height = unsafe { (*frame).rcx as i32 };

    let rc = video_bridge::surface_add_damage(task_id, x, y, width, height);
    if rc < 0 {
        wl_currency::award_loss();
        syscall_return_err(frame, u64::MAX)
    } else {
        wl_currency::award_win();
        syscall_return_ok(frame, 0)
    }
}

/// BUFFER_AGE: Get back buffer age for damage accumulation
/// Returns: buffer age (0 = undefined, N = N frames old)
pub fn syscall_buffer_age(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, 0);
    }

    let task_id = unsafe { (*task).task_id };
    let age = video_bridge::surface_get_buffer_age(task_id);

    // Don't award W/L for querying buffer age - it's informational
    syscall_return_ok(frame, age as u64)
}

// =============================================================================
// Surface Role Syscalls (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

pub const SYSCALL_SURFACE_SET_ROLE: u64 = 57;
pub const SYSCALL_SURFACE_SET_PARENT: u64 = 58;
pub const SYSCALL_SURFACE_SET_REL_POS: u64 = 59;
pub const SYSCALL_SURFACE_SET_TITLE: u64 = 63;

/// SURFACE_SET_ROLE: Set the role of a surface (toplevel, popup, subsurface)
/// Args: rdi = role (0=None, 1=Toplevel, 2=Popup, 3=Subsurface)
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_set_role(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let task_id = unsafe { (*task).task_id };
    let role = unsafe { (*frame).rdi } as u8;

    let result = video_bridge::surface_set_role(task_id, role);
    syscall_return_ok(frame, result as u64)
}

/// SURFACE_SET_PARENT: Set the parent surface for a subsurface
/// Args: rdi = parent_task_id
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_set_parent(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let task_id = unsafe { (*task).task_id };
    let parent_task_id = unsafe { (*frame).rdi } as u32;

    let result = video_bridge::surface_set_parent(task_id, parent_task_id);
    syscall_return_ok(frame, result as u64)
}

/// SURFACE_SET_REL_POS: Set the relative position of a subsurface
/// Args: rdi = rel_x, rsi = rel_y
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_set_rel_pos(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let task_id = unsafe { (*task).task_id };
    let rel_x = unsafe { (*frame).rdi } as i32;
    let rel_y = unsafe { (*frame).rsi } as i32;

    let result = video_bridge::surface_set_relative_position(task_id, rel_x, rel_y);
    syscall_return_ok(frame, result as u64)
}

/// SURFACE_SET_TITLE: Set the window title
/// Args: rdi = pointer to title string (UTF-8), rsi = length
/// Returns: 0 on success, -1 on failure
pub fn syscall_surface_set_title(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let task_id = unsafe { (*task).task_id };
    let title_ptr = unsafe { (*frame).rdi } as *const u8;
    let title_len = unsafe { (*frame).rsi } as usize;

    if title_ptr.is_null() || title_len == 0 {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    // Read title from user memory (max 31 chars)
    let copy_len = title_len.min(31);
    let title_slice = unsafe { core::slice::from_raw_parts(title_ptr, copy_len) };

    let result = video_bridge::surface_set_title(task_id, title_slice);
    syscall_return_ok(frame, result as u64)
}

// =============================================================================
// Input Event Syscalls (Wayland-like input protocol)
// =============================================================================

pub const SYSCALL_INPUT_POLL: u64 = 60;
pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;

/// INPUT_POLL: Poll for an input event (non-blocking)
/// Args: rdi = pointer to InputEvent structure to fill
/// Returns: 1 if event was returned, 0 if no events, -1 on error
pub fn syscall_input_poll(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let task_id = unsafe { (*task).task_id };
    let event_ptr = unsafe { (*frame).rdi } as *mut input_event::InputEvent;

    if event_ptr.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    // Auto-set pointer focus to compositor if not set
    // This ensures the compositor receives mouse events without needing to know its own task_id
    if task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        if input_event::input_get_pointer_focus() == 0 {
            input_event::input_set_pointer_focus(task_id, 0);
        }
    }

    // Poll for an event
    if let Some(event) = input_event::input_poll(task_id) {
        // Copy event to userspace
        unsafe {
            *event_ptr = event;
        }
        syscall_return_ok(frame, 1)
    } else {
        syscall_return_ok(frame, 0)
    }
}

/// INPUT_POLL_BATCH: Poll for multiple input events at once (non-blocking)
/// Much more efficient than calling INPUT_POLL in a loop - single lock acquisition.
/// Args: rdi = pointer to InputEvent array, rsi = max_count
/// Returns: number of events written to buffer
pub fn syscall_input_poll_batch(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, 0);
    }

    let task_id = unsafe { (*task).task_id };
    let buffer_ptr = unsafe { (*frame).rdi as *mut input_event::InputEvent };
    let max_count = unsafe { (*frame).rsi as usize };

    if buffer_ptr.is_null() || max_count == 0 {
        return syscall_return_ok(frame, 0);
    }

    // Auto-set pointer focus to compositor if not set
    if task_has_flag(task, TASK_FLAG_COMPOSITOR) {
        if input_event::input_get_pointer_focus() == 0 {
            input_event::input_set_pointer_focus(task_id, 0);
        }
    }

    let count = input_event::input_drain_batch(task_id, buffer_ptr, max_count);
    syscall_return_ok(frame, count as u64)
}

/// INPUT_HAS_EVENTS: Check if task has pending input events
/// Returns: number of pending events (0 if none)
pub fn syscall_input_has_events(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, 0);
    }

    let task_id = unsafe { (*task).task_id };
    let count = input_event::input_event_count(task_id);

    syscall_return_ok(frame, count as u64)
}

/// INPUT_SET_FOCUS: Set keyboard or pointer focus (compositor only)
/// Args: rdi = target task ID, rsi = focus type (0=keyboard, 1=pointer)
/// Returns: 0 on success, -1 on failure
pub fn syscall_input_set_focus(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    if task.is_null() {
        return syscall_return_ok(frame, (-1i64) as u64);
    }

    let target_task_id = unsafe { (*frame).rdi } as u32;
    let focus_type = unsafe { (*frame).rsi } as u32;

    // Get current time for enter/leave events
    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    let timestamp_ms = (ticks * 1000) / freq as u64;

    match focus_type {
        0 => {
            // Keyboard focus
            input_event::input_set_keyboard_focus(target_task_id);
        }
        1 => {
            // Pointer focus
            input_event::input_set_pointer_focus(target_task_id, timestamp_ms);
        }
        _ => {
            return syscall_return_ok(frame, (-1i64) as u64);
        }
    }

    syscall_return_ok(frame, 0)
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
    // Shared memory syscalls for Wayland-like compositor
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
    // Frame callback protocol
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
    // Pixel format negotiation
    table[SYSCALL_SHM_GET_FORMATS as usize] = SyscallEntry {
        handler: Some(syscall_shm_get_formats),
        name: b"shm_get_formats\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_SHM_CREATE_WITH_FORMAT as usize] = SyscallEntry {
        handler: Some(syscall_shm_create_with_format),
        name: b"shm_create_with_format\0".as_ptr() as *const c_char,
    };
    // Damage tracking
    table[SYSCALL_SURFACE_DAMAGE as usize] = SyscallEntry {
        handler: Some(syscall_surface_damage),
        name: b"surface_damage\0".as_ptr() as *const c_char,
    };
    table[SYSCALL_BUFFER_AGE as usize] = SyscallEntry {
        handler: Some(syscall_buffer_age),
        name: b"buffer_age\0".as_ptr() as *const c_char,
    };
    // Surface roles (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
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
    // Input event protocol (Wayland-like per-task queues)
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
    pub const SYSCALL_ENUMERATE_WINDOWS: u64 = 30;
    pub const SYSCALL_SET_WINDOW_POSITION: u64 = 31;
    pub const SYSCALL_SET_WINDOW_STATE: u64 = 32;
    pub const SYSCALL_RAISE_WINDOW: u64 = 33;
    pub const SYSCALL_SURFACE_COMMIT: u64 = 38;
    pub const SYSCALL_GET_TIME_MS: u64 = 39;
    // Shared memory syscalls
    pub const SYSCALL_SHM_CREATE: u64 = 40;
    pub const SYSCALL_SHM_MAP: u64 = 41;
    pub const SYSCALL_SHM_UNMAP: u64 = 42;
    pub const SYSCALL_SHM_DESTROY: u64 = 43;
    pub const SYSCALL_SURFACE_ATTACH: u64 = 44;
    pub const SYSCALL_FB_FLIP: u64 = 45;
    pub const SYSCALL_DRAIN_QUEUE: u64 = 46;
    // Buffer reference counting (Wayland-style)
    pub const SYSCALL_SHM_ACQUIRE: u64 = 47;
    pub const SYSCALL_SHM_RELEASE: u64 = 48;
    pub const SYSCALL_SHM_POLL_RELEASED: u64 = 49;
    // Frame callback protocol (Wayland wl_surface.frame)
    pub const SYSCALL_SURFACE_FRAME: u64 = 50;
    pub const SYSCALL_POLL_FRAME_DONE: u64 = 51;
    pub const SYSCALL_MARK_FRAMES_DONE: u64 = 52;
    // Pixel format negotiation (Wayland wl_shm)
    pub const SYSCALL_SHM_GET_FORMATS: u64 = 53;
    pub const SYSCALL_SHM_CREATE_WITH_FORMAT: u64 = 54;
    // Damage tracking (Wayland wl_surface.damage)
    pub const SYSCALL_SURFACE_DAMAGE: u64 = 55;
    pub const SYSCALL_BUFFER_AGE: u64 = 56;
    // Surface roles (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
    pub const SYSCALL_SURFACE_SET_ROLE: u64 = 57;
    pub const SYSCALL_SURFACE_SET_PARENT: u64 = 58;
    pub const SYSCALL_SURFACE_SET_REL_POS: u64 = 59;
    pub const SYSCALL_SURFACE_SET_TITLE: u64 = 63;
    // Input event protocol (Wayland-like per-task queues)
    pub const SYSCALL_INPUT_POLL: u64 = 60;
    pub const SYSCALL_INPUT_POLL_BATCH: u64 = 34;
    pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
    pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;
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
