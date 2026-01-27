use core::ffi::{c_char, c_void};
use core::num::NonZeroU32;
use core::ptr::NonNull;

use crate::syscall_raw::{syscall0, syscall1, syscall2, syscall3, syscall4};

pub use slopos_abi::{
    DisplayInfo, INPUT_FOCUS_KEYBOARD, INPUT_FOCUS_POINTER, InputEvent, InputEventData,
    InputEventType, MAX_WINDOW_DAMAGE_REGIONS, PixelFormat, SHM_ACCESS_RO, SHM_ACCESS_RW,
    SurfaceRole, USER_FS_OPEN_APPEND, USER_FS_OPEN_CREAT, USER_FS_OPEN_READ, USER_FS_OPEN_WRITE,
    UserFsEntry, UserFsList, UserFsStat, WindowDamageRect, WindowInfo,
};

pub use slopos_abi::syscall::*;

pub type UserWindowDamageRect = WindowDamageRect;
pub type UserWindowInfo = WindowInfo;

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_yield() {
    unsafe {
        syscall0(SYSCALL_YIELD);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_write(buf: &[u8]) -> i64 {
    unsafe { syscall2(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read(buf: &mut [u8]) -> i64 {
    unsafe { syscall2(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read_char() -> i64 {
    unsafe { syscall0(SYSCALL_READ_CHAR) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sleep_ms(ms: u32) {
    unsafe {
        syscall1(SYSCALL_SLEEP_MS, ms as u64);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_get_time_ms() -> u64 {
    unsafe { syscall0(SYSCALL_GET_TIME_MS) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_get_cpu_count() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CPU_COUNT) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_get_current_cpu() -> u32 {
    unsafe { syscall0(SYSCALL_GET_CURRENT_CPU) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette() -> u64 {
    unsafe { syscall0(SYSCALL_ROULETTE) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_result(fate_packed: u64) {
    unsafe {
        syscall1(SYSCALL_ROULETTE_RESULT, fate_packed);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_draw(fate: u32) -> i64 {
    unsafe { syscall1(SYSCALL_ROULETTE_DRAW, fate as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exit() -> ! {
    unsafe {
        syscall0(SYSCALL_EXIT);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_info(out: &mut DisplayInfo) -> i64 {
    unsafe { syscall1(SYSCALL_FB_INFO, out as *mut _ as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_tty_set_focus(task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_TTY_SET_FOCUS, task_id as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_random_next() -> u32 {
    unsafe { syscall0(SYSCALL_RANDOM_NEXT) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_open(path: *const c_char, flags: u32) -> i64 {
    unsafe { syscall2(SYSCALL_FS_OPEN, path as u64, flags as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_close(fd: i32) -> i64 {
    unsafe { syscall1(SYSCALL_FS_CLOSE, fd as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_read(fd: i32, buf: *mut c_void, len: usize) -> i64 {
    unsafe { syscall3(SYSCALL_FS_READ, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_write(fd: i32, buf: *const c_void, len: usize) -> i64 {
    unsafe { syscall3(SYSCALL_FS_WRITE, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_stat(path: *const c_char, out_stat: &mut UserFsStat) -> i64 {
    unsafe { syscall2(SYSCALL_FS_STAT, path as u64, out_stat as *mut _ as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_mkdir(path: *const c_char) -> i64 {
    unsafe { syscall1(SYSCALL_FS_MKDIR, path as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_unlink(path: *const c_char) -> i64 {
    unsafe { syscall1(SYSCALL_FS_UNLINK, path as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_fs_list(path: *const c_char, list: &mut UserFsList) -> i64 {
    unsafe { syscall2(SYSCALL_FS_LIST, path as u64, list as *mut _ as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall1(SYSCALL_SYS_INFO, info as *mut _ as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_enumerate_windows(windows: &mut [UserWindowInfo]) -> u64 {
    unsafe {
        syscall2(
            SYSCALL_ENUMERATE_WINDOWS,
            windows.as_mut_ptr() as u64,
            windows.len() as u64,
        )
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_set_window_position(task_id: u32, x: i32, y: i32) -> i64 {
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
#[unsafe(link_section = ".user_text")]
pub fn sys_set_window_state(task_id: u32, state: u8) -> i64 {
    unsafe { syscall2(SYSCALL_SET_WINDOW_STATE, task_id as u64, state as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_raise_window(task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_RAISE_WINDOW, task_id as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_halt() -> ! {
    unsafe {
        syscall0(SYSCALL_HALT);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_spawn_task(name: &[u8]) -> i32 {
    unsafe { syscall2(SYSCALL_SPAWN_TASK, name.as_ptr() as u64, name.len() as u64) as i32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exec(path: &[u8]) -> i64 {
    unsafe { syscall1(SYSCALL_EXEC, path.as_ptr() as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fork() -> i32 {
    unsafe { syscall0(SYSCALL_FORK) as i32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_commit() -> i64 {
    unsafe { syscall0(SYSCALL_SURFACE_COMMIT) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_create(size: u64, flags: u32) -> u32 {
    unsafe { syscall2(SYSCALL_SHM_CREATE, size, flags as u64) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_map(token: u32, access: u32) -> u64 {
    unsafe { syscall2(SYSCALL_SHM_MAP, token as u64, access as u64) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub unsafe fn sys_shm_unmap(virt_addr: u64) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_UNMAP, virt_addr) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_destroy(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_DESTROY, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_attach(token: u32, width: u32, height: u32) -> i64 {
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
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_flip(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_FB_FLIP, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_drain_queue() {
    unsafe {
        syscall0(SYSCALL_DRAIN_QUEUE);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_acquire(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_ACQUIRE, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_release(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_RELEASE, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_poll_released(token: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SHM_POLL_RELEASED, token as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_frame() -> i64 {
    unsafe { syscall0(SYSCALL_SURFACE_FRAME) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_poll_frame_done() -> u64 {
    unsafe { syscall0(SYSCALL_POLL_FRAME_DONE) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_mark_frames_done(present_time_ms: u64) {
    unsafe {
        syscall1(SYSCALL_MARK_FRAMES_DONE, present_time_ms);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_get_formats() -> u32 {
    unsafe { syscall0(SYSCALL_SHM_GET_FORMATS) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_create_with_format(size: u64, format: PixelFormat) -> u32 {
    unsafe { syscall2(SYSCALL_SHM_CREATE_WITH_FORMAT, size, format as u64) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_damage(x: i32, y: i32, width: i32, height: i32) -> i64 {
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
#[unsafe(link_section = ".user_text")]
pub fn sys_buffer_age() -> u8 {
    unsafe { syscall0(SYSCALL_BUFFER_AGE) as u8 }
}

pub fn sys_surface_set_role(role: SurfaceRole) -> i64 {
    unsafe { syscall1(SYSCALL_SURFACE_SET_ROLE, role as u64) as i64 }
}

pub fn sys_surface_set_parent(parent_task_id: u32) -> i64 {
    unsafe { syscall1(SYSCALL_SURFACE_SET_PARENT, parent_task_id as u64) as i64 }
}

pub fn sys_surface_set_relative_position(rel_x: i32, rel_y: i32) -> i64 {
    unsafe { syscall2(SYSCALL_SURFACE_SET_REL_POS, rel_x as u64, rel_y as u64) as i64 }
}

pub fn sys_surface_set_title(title: &str) -> i64 {
    let bytes = title.as_bytes();
    unsafe {
        syscall2(
            SYSCALL_SURFACE_SET_TITLE,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        ) as i64
    }
}

pub fn sys_input_poll(event_out: &mut InputEvent) -> Option<InputEvent> {
    let result = unsafe { syscall1(SYSCALL_INPUT_POLL, event_out as *mut InputEvent as u64) };
    if result == 1 { Some(*event_out) } else { None }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_input_poll_batch(events: &mut [InputEvent]) -> u64 {
    unsafe {
        syscall2(
            SYSCALL_INPUT_POLL_BATCH,
            events.as_mut_ptr() as u64,
            events.len() as u64,
        )
    }
}

pub fn sys_input_has_events() -> u32 {
    unsafe { syscall0(SYSCALL_INPUT_HAS_EVENTS) as u32 }
}

pub fn sys_input_set_focus(target_task_id: u32, focus_type: u32) -> i64 {
    unsafe {
        syscall2(
            SYSCALL_INPUT_SET_FOCUS,
            target_task_id as u64,
            focus_type as u64,
        ) as i64
    }
}

pub fn sys_input_set_keyboard_focus(target_task_id: u32) -> i64 {
    sys_input_set_focus(target_task_id, INPUT_FOCUS_KEYBOARD)
}

pub fn sys_input_set_pointer_focus(target_task_id: u32) -> i64 {
    sys_input_set_focus(target_task_id, INPUT_FOCUS_POINTER)
}

pub fn sys_input_set_pointer_focus_with_offset(
    target_task_id: u32,
    offset_x: i32,
    offset_y: i32,
) -> i64 {
    unsafe {
        syscall3(
            SYSCALL_INPUT_SET_FOCUS_WITH_OFFSET,
            target_task_id as u64,
            offset_x as u64,
            offset_y as u64,
        ) as i64
    }
}

pub fn sys_input_get_pointer_pos() -> (i32, i32) {
    let result = unsafe { syscall0(SYSCALL_INPUT_GET_POINTER_POS) };
    let x = (result >> 32) as i32;
    let y = result as i32;
    (x, y)
}

pub fn sys_input_get_button_state() -> u8 {
    unsafe { syscall0(SYSCALL_INPUT_GET_BUTTON_STATE) as u8 }
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
