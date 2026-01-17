//! Userland syscall wrappers — the canonical implementation for user-mode code.
//!
//! This module provides the ONLY syscall wrapper implementation in SlopOS.
//! All user-mode applications (shell, compositor, roulette, file_manager) use
//! these wrappers to invoke kernel services via `int 0x80`.
//!
//! Architecture:
//! - `abi/src/syscall.rs` — Syscall numbers and shared types (source of truth)
//! - `userland/src/syscall.rs` — User-mode wrappers (this file)
//! - `drivers/src/syscall_*.rs` — Kernel-side handlers
//!
//! All functions are placed in `.user_text` section for proper Ring 3 execution.

use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::num::NonZeroU32;
use core::ptr::NonNull;

// Re-export all ABI types from slopos_abi for userland consumers
pub use slopos_abi::{
    DisplayInfo, INPUT_FOCUS_KEYBOARD, INPUT_FOCUS_POINTER, InputEvent, InputEventData,
    InputEventType, MAX_WINDOW_DAMAGE_REGIONS, PixelFormat, SHM_ACCESS_RO, SHM_ACCESS_RW,
    SurfaceRole, USER_FS_OPEN_APPEND, USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE,
    UserFsEntry, UserFsList, UserFsStat, WindowDamageRect, WindowInfo,
};

// Re-export syscall numbers and data structures from canonical ABI source
pub use slopos_abi::syscall::*;

// Type aliases for backwards compatibility (use WindowInfo from slopos_abi)
pub type UserWindowDamageRect = WindowDamageRect;
pub type UserWindowInfo = WindowInfo;

#[inline(always)]
#[unsafe(link_section = ".user_text")]
unsafe fn syscall_impl(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
unsafe fn syscall4_impl(num: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            lateout("rax") ret,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_yield() {
    unsafe {
        syscall_impl(SYSCALL_YIELD, 0, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_write(buf: &[u8]) -> i64 {
    unsafe { syscall_impl(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read(buf: &mut [u8]) -> i64 {
    unsafe { syscall_impl(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read_char() -> i64 {
    unsafe { syscall_impl(SYSCALL_READ_CHAR, 0, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sleep_ms(ms: u32) {
    unsafe {
        syscall_impl(SYSCALL_SLEEP_MS, ms as u64, 0, 0);
    }
}

/// Returns the current time in milliseconds since boot.
/// Used for frame pacing in the compositor (60Hz target).
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_get_time_ms() -> u64 {
    unsafe { syscall_impl(SYSCALL_GET_TIME_MS, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette() -> u64 {
    unsafe { syscall_impl(SYSCALL_ROULETTE, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_result(fate_packed: u64) {
    unsafe {
        syscall_impl(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_draw(fate: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_ROULETTE_DRAW, fate as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exit() -> ! {
    unsafe {
        syscall_impl(SYSCALL_EXIT, 0, 0, 0);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall_impl(SYSCALL_FB_INFO, out as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_tty_set_focus(task_id: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_TTY_SET_FOCUS, task_id as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_random_next() -> u32 {
    unsafe { syscall_impl(SYSCALL_RANDOM_NEXT, 0, 0, 0) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_open(path: *const c_char, flags: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_OPEN, path as u64, flags as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_close(fd: i32) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_CLOSE, fd as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_read(fd: i32, buf: *mut c_void, len: usize) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_READ, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_write(fd: i32, buf: *const c_void, len: usize) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_WRITE, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_stat(path: *const c_char, out_stat: &mut UserFsStat) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_STAT, path as u64, out_stat as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_mkdir(path: *const c_char) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_MKDIR, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_unlink(path: *const c_char) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_UNLINK, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_list(path: *const c_char, list: &mut UserFsList) -> i64 {
    unsafe { syscall_impl(SYSCALL_FS_LIST, path as u64, list as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall_impl(SYSCALL_SYS_INFO, info as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_enumerate_windows(windows: &mut [UserWindowInfo]) -> u64 {
    unsafe {
        syscall_impl(
            SYSCALL_ENUMERATE_WINDOWS,
            windows.as_mut_ptr() as u64,
            windows.len() as u64,
            0,
        )
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_set_window_position(task_id: u32, x: i32, y: i32) -> i64 {
    unsafe {
        syscall_impl(
            SYSCALL_SET_WINDOW_POSITION,
            task_id as u64,
            x as u64,
            y as u64,
        ) as i64
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_set_window_state(task_id: u32, state: u8) -> i64 {
    unsafe { syscall_impl(SYSCALL_SET_WINDOW_STATE, task_id as u64, state as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_raise_window(task_id: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_RAISE_WINDOW, task_id as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_halt() -> ! {
    unsafe {
        syscall_impl(SYSCALL_HALT, 0, 0, 0);
    }
    loop {}
}

/// Spawn a new userland task by name.
/// The name must be a null-terminated byte string matching a known task name
/// (e.g., b"file_manager\0").
/// Returns task_id (> 0) on success, 0 on failure.
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_spawn_task(name: &[u8]) -> i32 {
    unsafe {
        syscall_impl(
            SYSCALL_SPAWN_TASK,
            name.as_ptr() as u64,
            name.len() as u64,
            0,
        ) as i32
    }
}

/// Execute an ELF binary from the filesystem, replacing the current process.
/// On success, this function does not return - the process image is replaced.
/// On failure, returns a negative error code.
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exec(path: &[u8]) -> i64 {
    unsafe { syscall_impl(SYSCALL_EXEC, path.as_ptr() as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fork() -> i32 {
    unsafe { syscall_impl(SYSCALL_FORK, 0, 0, 0) as i32 }
}

/// Commit a surface's back buffer to front buffer (Wayland-style double buffering)
/// Swaps buffer pointers atomically and transfers damage tracking
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_commit() -> i64 {
    unsafe { syscall_impl(SYSCALL_SURFACE_COMMIT, 0, 0, 0) as i64 }
}

// =============================================================================
// Shared Memory Syscalls (Wayland-like compositor)
// =============================================================================

/// Create a shared memory buffer.
/// Returns a token (> 0) on success, or 0 on failure.
///
/// # Arguments
/// * `size` - Size of the buffer in bytes
/// * `flags` - Reserved for future use (pass 0)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_create(size: u64, flags: u32) -> u32 {
    unsafe { syscall_impl(SYSCALL_SHM_CREATE, size, flags as u64, 0) as u32 }
}

/// Map a shared memory buffer into the caller's address space.
/// Returns virtual address on success, or 0 on failure.
///
/// # Arguments
/// * `token` - Token from sys_shm_create
/// * `access` - SHM_ACCESS_RO (0) or SHM_ACCESS_RW (1)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_map(token: u32, access: u32) -> u64 {
    unsafe { syscall_impl(SYSCALL_SHM_MAP, token as u64, access as u64, 0) }
}

/// Unmap a shared memory buffer from the caller's address space.
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `virt_addr` - Virtual address from sys_shm_map
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_shm_unmap(virt_addr: u64) -> i64 {
    unsafe { syscall_impl(SYSCALL_SHM_UNMAP, virt_addr, 0, 0) as i64 }
}

/// Destroy a shared memory buffer (owner only).
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `token` - Token from sys_shm_create
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_destroy(token: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SHM_DESTROY, token as u64, 0, 0) as i64 }
}

/// Attach a shared memory buffer as a window surface.
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `token` - Token from sys_shm_create
/// * `width` - Surface width in pixels
/// * `height` - Surface height in pixels
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_attach(token: u32, width: u32, height: u32) -> i64 {
    unsafe {
        syscall_impl(
            SYSCALL_SURFACE_ATTACH,
            token as u64,
            width as u64,
            height as u64,
        ) as i64
    }
}

/// Copy a shared memory buffer to the framebuffer MMIO (compositor only).
/// This is the "page flip" operation - presents the compositor's output buffer.
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `token` - Token of the output buffer to present
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_flip(token: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_FB_FLIP, token as u64, 0, 0) as i64 }
}

/// Drain the compositor queue (compositor only).
/// Processes all pending client operations (commits, registers, unregisters).
/// Must be called at the start of each compositor frame, before enumerate_windows.
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_drain_queue() {
    unsafe {
        syscall_impl(SYSCALL_DRAIN_QUEUE, 0, 0, 0);
    }
}

// =============================================================================
// Buffer Reference Counting Syscalls (Wayland-style)
// =============================================================================

/// Acquire a buffer reference (compositor only).
/// Increments refcount and clears the released flag.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_acquire(token: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SHM_ACQUIRE, token as u64, 0, 0) as i64 }
}

/// Release a buffer reference (compositor only).
/// Decrements refcount and sets the released flag.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_release(token: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SHM_RELEASE, token as u64, 0, 0) as i64 }
}

/// Poll whether a buffer has been released by the compositor.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 1 if released (client can reuse), 0 if not released, -1 on error
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_poll_released(token: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SHM_POLL_RELEASED, token as u64, 0, 0) as i64 }
}

// =============================================================================
// Frame Callback Syscalls (Wayland wl_surface.frame)
// =============================================================================

/// Request a frame callback (Wayland wl_surface.frame).
///
/// Call this before surface_commit to request notification when the frame
/// has been presented. After committing, poll with sys_poll_frame_done()
/// to check when presentation occurred.
///
/// # Returns
/// 0 on success, -1 on failure
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_frame() -> i64 {
    unsafe { syscall_impl(SYSCALL_SURFACE_FRAME, 0, 0, 0) as i64 }
}

/// Poll for frame completion (Wayland frame callback done).
///
/// After calling sys_surface_frame() and sys_surface_commit(), use this
/// to check if the frame has been presented to the display.
///
/// # Returns
/// - Timestamp (ms since boot) when frame was presented, if done
/// - 0 if frame is still pending (not yet presented)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_poll_frame_done() -> u64 {
    unsafe { syscall_impl(SYSCALL_POLL_FRAME_DONE, 0, 0, 0) }
}

/// Mark all pending frame callbacks as done (compositor only).
///
/// Called by the compositor after presenting a frame to notify all
/// clients that their frames have been displayed.
///
/// # Arguments
/// * `present_time_ms` - The presentation timestamp (ms since boot)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_mark_frames_done(present_time_ms: u64) {
    unsafe {
        syscall_impl(SYSCALL_MARK_FRAMES_DONE, present_time_ms, 0, 0);
    }
}

// =============================================================================
// Pixel Format Negotiation Syscalls (Wayland wl_shm)
// =============================================================================

/// Get the bitmap of supported pixel formats.
///
/// Returns a bitmap where bit N is set if PixelFormat with value N is supported.
/// Use this to negotiate formats with the compositor.
///
/// # Example
/// ```
/// let formats = sys_shm_get_formats();
/// if formats & (1 << PixelFormat::Argb8888 as u32) != 0 {
///     // ARGB8888 is supported
/// }
/// ```
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_get_formats() -> u32 {
    unsafe { syscall_impl(SYSCALL_SHM_GET_FORMATS, 0, 0, 0) as u32 }
}

/// Create a shared memory buffer with a specific pixel format.
///
/// # Arguments
/// * `size` - Size of the buffer in bytes
/// * `format` - Pixel format for this buffer
///
/// # Returns
/// Buffer token on success (> 0), 0 on failure
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_create_with_format(size: u64, format: PixelFormat) -> u32 {
    unsafe { syscall_impl(SYSCALL_SHM_CREATE_WITH_FORMAT, size, format as u64, 0) as u32 }
}

// =============================================================================
// Damage Tracking Syscalls (Wayland wl_surface.damage)
// =============================================================================

/// Add a damage region to the surface's back buffer.
///
/// This tells the compositor which region of the surface has been modified
/// and needs to be redrawn. Multiple damage regions can be added before
/// committing.
///
/// # Arguments
/// * `x` - X coordinate of the damage region (surface-local)
/// * `y` - Y coordinate of the damage region (surface-local)
/// * `width` - Width of the damage region
/// * `height` - Height of the damage region
///
/// # Returns
/// 0 on success, -1 on failure
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_damage(x: i32, y: i32, width: i32, height: i32) -> i64 {
    unsafe {
        syscall4_impl(
            SYSCALL_SURFACE_DAMAGE,
            x as u64,
            y as u64,
            width as u64,
            height as u64,
        ) as i64
    }
}

/// Get the age of the back buffer for damage accumulation.
///
/// The buffer age indicates how many frames old the buffer content is:
/// - 0: Buffer content is undefined (must redraw everything)
/// - 1: Buffer contains the previous frame (only damaged regions need redraw)
/// - N: Buffer contains content from N frames ago (accumulate damage from N frames)
/// - 255 (u8::MAX): Buffer is too old for damage accumulation (redraw everything)
///
/// This allows efficient partial updates when using double or triple buffering.
///
/// # Returns
/// Buffer age (see description above)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_buffer_age() -> u8 {
    unsafe { syscall_impl(SYSCALL_BUFFER_AGE, 0, 0, 0) as u8 }
}

// =============================================================================
// Surface Role Syscalls (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Set the role of a surface.
/// Role can only be set once per surface (Wayland semantics).
/// Returns 0 on success, -1 if role already set or invalid.
pub fn sys_surface_set_role(role: SurfaceRole) -> i64 {
    unsafe { syscall_impl(SYSCALL_SURFACE_SET_ROLE, role as u64, 0, 0) as i64 }
}

/// Set the parent surface for a subsurface.
/// Only valid for surfaces with role Subsurface.
/// Returns 0 on success, -1 on failure.
pub fn sys_surface_set_parent(parent_task_id: u32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SURFACE_SET_PARENT, parent_task_id as u64, 0, 0) as i64 }
}

/// Set the relative position of a subsurface.
/// Position is relative to the parent surface's top-left corner.
/// Only valid for surfaces with role Subsurface.
pub fn sys_surface_set_relative_position(rel_x: i32, rel_y: i32) -> i64 {
    unsafe { syscall_impl(SYSCALL_SURFACE_SET_REL_POS, rel_x as u64, rel_y as u64, 0) as i64 }
}

/// Set the window title.
/// Title is UTF-8, max 31 characters (null-terminated in 32-byte buffer).
pub fn sys_surface_set_title(title: &str) -> i64 {
    let bytes = title.as_bytes();
    unsafe {
        syscall_impl(
            SYSCALL_SURFACE_SET_TITLE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
            0,
        ) as i64
    }
}

// =============================================================================
// Input Event Protocol (Wayland-like per-task input queues)
// =============================================================================

/// Poll for an input event (non-blocking).
/// Returns Some(event) if an event was available, None if queue is empty.
pub fn sys_input_poll(event_out: &mut InputEvent) -> Option<InputEvent> {
    let result = unsafe {
        syscall_impl(
            SYSCALL_INPUT_POLL,
            event_out as *mut InputEvent as u64,
            0,
            0,
        )
    };
    if result == 1 { Some(*event_out) } else { None }
}

/// Poll for multiple input events at once (non-blocking batch operation).
/// Much more efficient than calling sys_input_poll() in a loop - single syscall,
/// single lock acquisition, avoiding lock ping-pong with IRQ handlers.
///
/// # Arguments
/// * `events` - Mutable slice to receive events
///
/// # Returns
/// Number of events actually written to the buffer (0 if queue was empty)
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_input_poll_batch(events: &mut [InputEvent]) -> u64 {
    unsafe {
        syscall_impl(
            SYSCALL_INPUT_POLL_BATCH,
            events.as_mut_ptr() as u64,
            events.len() as u64,
            0,
        )
    }
}

/// Check if the current task has pending input events.
/// Returns the number of pending events.
pub fn sys_input_has_events() -> u32 {
    unsafe { syscall_impl(SYSCALL_INPUT_HAS_EVENTS, 0, 0, 0) as u32 }
}

/// Set keyboard or pointer focus to a task (compositor only).
/// focus_type: 0 = keyboard, 1 = pointer
/// Returns 0 on success, -1 on failure.
pub fn sys_input_set_focus(target_task_id: u32, focus_type: u32) -> i64 {
    unsafe {
        syscall_impl(
            SYSCALL_INPUT_SET_FOCUS,
            target_task_id as u64,
            focus_type as u64,
            0,
        ) as i64
    }
}

/// Set keyboard focus to a task (compositor convenience function).
pub fn sys_input_set_keyboard_focus(target_task_id: u32) -> i64 {
    sys_input_set_focus(target_task_id, INPUT_FOCUS_KEYBOARD)
}

/// Set pointer focus to a task (compositor convenience function).
pub fn sys_input_set_pointer_focus(target_task_id: u32) -> i64 {
    sys_input_set_focus(target_task_id, INPUT_FOCUS_POINTER)
}

/// Set pointer focus to a task with window offset for coordinate translation.
/// The offset is subtracted from screen coordinates to get window-local coordinates.
/// For a window at screen position (100, 50), pass offset_x=100, offset_y=50.
pub fn sys_input_set_pointer_focus_with_offset(
    target_task_id: u32,
    offset_x: i32,
    offset_y: i32,
) -> i64 {
    unsafe {
        syscall_impl(
            SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET,
            target_task_id as u64,
            offset_x as u64,
            offset_y as u64,
        ) as i64
    }
}

/// Get the current global pointer position (compositor only).
/// Returns (x, y) in screen coordinates.
pub fn sys_input_get_pointer_pos() -> (i32, i32) {
    let result = unsafe { syscall_impl(SYSCALL_INPUT_GET_POINTER_POS, 0, 0, 0) };
    let x = (result >> 32) as i32;
    let y = result as i32;
    (x, y)
}

/// Get the current global pointer button state (compositor only).
/// Returns button state as u8 (bit 0 = left, bit 1 = right, bit 2 = middle).
pub fn sys_input_get_button_state() -> u8 {
    unsafe { syscall_impl(SYSCALL_INPUT_GET_BUTTON_STATE, 0, 0, 0) as u8 }
}

pub use slopos_abi::ShmError;

/// Safe wrapper for an owned shared memory buffer (read-write access).
///
/// This type provides safe access to a shared memory buffer that this process owns.
/// The buffer is automatically unmapped and destroyed when dropped.
///
/// # Safety Guarantees
/// - All slice access is bounds-checked
/// - Buffer is automatically cleaned up on drop
/// - Token is guaranteed non-zero (valid)
pub struct ShmBuffer {
    token: NonZeroU32,
    ptr: NonNull<u8>,
    size: usize,
}

impl ShmBuffer {
    /// Create a new shared memory buffer with the specified size.
    ///
    /// The buffer is zero-initialized and mapped with read-write access.
    ///
    /// # Errors
    /// - `ShmError::InvalidSize` if size is 0
    /// - `ShmError::AllocationFailed` if kernel cannot allocate memory
    /// - `ShmError::MappingFailed` if kernel cannot map the buffer
    #[unsafe(link_section = ".user_text")]
    pub fn create(size: usize) -> Result<Self, ShmError> {
        if size == 0 {
            return Err(ShmError::InvalidSize);
        }

        // Create the buffer in kernel
        let token_raw = sys_shm_create(size as u64, 0);
        let token = NonZeroU32::new(token_raw).ok_or(ShmError::AllocationFailed)?;

        // Map with read-write access
        let ptr_raw = sys_shm_map(token_raw, SHM_ACCESS_RW);
        if ptr_raw == 0 {
            // Cleanup: destroy the buffer we just created
            sys_shm_destroy(token_raw);
            return Err(ShmError::MappingFailed);
        }

        let ptr = NonNull::new(ptr_raw as *mut u8).ok_or_else(|| {
            sys_shm_destroy(token_raw);
            ShmError::MappingFailed
        })?;

        Ok(Self { token, ptr, size })
    }

    /// Get the buffer token for passing to kernel syscalls.
    #[inline]
    pub fn token(&self) -> u32 {
        self.token.get()
    }

    /// Get the size of the buffer in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get a read-only slice view of the entire buffer.
    ///
    /// # Safety
    /// This is safe because:
    /// - The pointer was validated at construction time
    /// - The size was recorded at construction time
    /// - The buffer cannot be unmapped while this ShmBuffer exists
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid and size is correct (both set at construction)
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    /// Get a mutable slice view of the entire buffer.
    ///
    /// # Safety
    /// This is safe because:
    /// - We have exclusive ownership (ShmBuffer is not Clone/Copy)
    /// - The pointer and size were validated at construction
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is valid, size is correct, and we have exclusive access
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.size) }
    }

    /// Attach this buffer as a window surface with the compositor.
    ///
    /// This registers the buffer as a drawable surface.
    ///
    /// # Errors
    /// Returns error if the kernel rejects the attach operation.
    #[unsafe(link_section = ".user_text")]
    pub fn attach_surface(&self, width: u32, height: u32) -> Result<(), ShmError> {
        let result = sys_surface_attach(self.token.get(), width, height);
        if result < 0 {
            Err(ShmError::PermissionDenied)
        } else {
            Ok(())
        }
    }
}

impl Drop for ShmBuffer {
    #[unsafe(link_section = ".user_text")]
    fn drop(&mut self) {
        // Unmap from our address space
        unsafe {
            sys_shm_unmap(self.ptr.as_ptr() as u64);
        }
        // Destroy the buffer (we are the owner)
        sys_shm_destroy(self.token.get());
    }
}

/// Safe wrapper for a read-only shared memory buffer reference.
///
/// This type provides safe read-only access to a shared memory buffer
/// that was created by another process. Used by the compositor to read
/// client surface buffers.
///
/// # Safety Guarantees
/// - All slice access is bounds-checked
/// - Buffer is automatically unmapped when dropped
/// - Token is guaranteed non-zero (valid)
pub struct ShmBufferRef {
    token: NonZeroU32,
    ptr: NonNull<u8>,
    size: usize,
}

impl ShmBufferRef {
    /// Map an existing shared memory buffer with read-only access.
    ///
    /// # Arguments
    /// * `token` - Token of the buffer to map (from another process)
    /// * `size` - Expected size of the buffer
    ///
    /// # Errors
    /// - `ShmError::InvalidToken` if token is 0 or invalid
    /// - `ShmError::MappingFailed` if kernel cannot map the buffer
    #[unsafe(link_section = ".user_text")]
    pub fn map_readonly(token: u32, size: usize) -> Result<Self, ShmError> {
        let token_nz = NonZeroU32::new(token).ok_or(ShmError::InvalidToken)?;

        if size == 0 {
            return Err(ShmError::InvalidSize);
        }

        let ptr_raw = sys_shm_map(token, SHM_ACCESS_RO);
        if ptr_raw == 0 {
            return Err(ShmError::MappingFailed);
        }

        let ptr = NonNull::new(ptr_raw as *mut u8).ok_or(ShmError::MappingFailed)?;

        Ok(Self {
            token: token_nz,
            ptr,
            size,
        })
    }

    /// Get the buffer token.
    #[inline]
    pub fn token(&self) -> u32 {
        self.token.get()
    }

    /// Get the size of the buffer in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get a read-only slice view of the entire buffer.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is valid and size is correct (both set at construction)
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.size) }
    }

    /// Get a read-only slice of a subregion.
    ///
    /// Returns None if the range is out of bounds.
    #[inline]
    pub fn slice(&self, start: usize, len: usize) -> Option<&[u8]> {
        if start.saturating_add(len) <= self.size {
            Some(&self.as_slice()[start..start + len])
        } else {
            None
        }
    }
}

impl Drop for ShmBufferRef {
    #[unsafe(link_section = ".user_text")]
    fn drop(&mut self) {
        // Unmap from our address space (we don't destroy - we're not the owner)
        unsafe {
            sys_shm_unmap(self.ptr.as_ptr() as u64);
        }
    }
}

// =============================================================================
// Cached Buffer Access (for compositor surface cache)
// =============================================================================

/// A cached shared memory mapping that does NOT unmap on drop.
///
/// Used by the compositor to cache client surface mappings across frames.
/// The mapping is kept alive by not calling sys_shm_unmap.
///
/// # Safety
/// The mapping is valid as long as:
/// - The client process that created the buffer is alive
/// - The buffer has not been destroyed
/// The compositor must clean up stale entries when windows are closed.
pub struct CachedShmMapping {
    vaddr: u64,
    size: usize,
}

impl CachedShmMapping {
    /// Map a buffer with read-only access (for compositor).
    /// Unlike ShmBufferRef, this does NOT unmap on drop.
    #[unsafe(link_section = ".user_text")]
    pub fn map_readonly(token: u32, size: usize) -> Option<Self> {
        if token == 0 || size == 0 {
            return None;
        }

        let vaddr = sys_shm_map(token, SHM_ACCESS_RO);
        if vaddr == 0 {
            return None;
        }

        Some(Self { vaddr, size })
    }

    /// Get the virtual address (for cache tracking).
    #[inline]
    pub fn vaddr(&self) -> u64 {
        self.vaddr
    }

    /// Get the buffer size.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get a read-only slice view of the buffer.
    ///
    /// # Safety
    /// This is safe because:
    /// - The vaddr was validated by the kernel during mapping
    /// - The size was specified at creation time
    /// - The mapping persists (we don't unmap)
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: vaddr is a valid kernel-provided address for a mapped buffer
        unsafe { core::slice::from_raw_parts(self.vaddr as *const u8, self.size) }
    }

    /// Get a subslice, returning None if out of bounds.
    #[inline]
    pub fn slice(&self, start: usize, len: usize) -> Option<&[u8]> {
        if start.saturating_add(len) <= self.size {
            Some(&self.as_slice()[start..start + len])
        } else {
            None
        }
    }
}

// Note: CachedShmMapping intentionally does NOT implement Drop.
// The kernel will clean up when the process exits.
