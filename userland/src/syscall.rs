use core::arch::asm;
use core::ffi::{c_char, c_void};

pub const SYSCALL_YIELD: u64 = 0;
pub const SYSCALL_EXIT: u64 = 1;
pub const SYSCALL_WRITE: u64 = 2;
pub const SYSCALL_READ: u64 = 3;
pub const SYSCALL_READ_CHAR: u64 = 25;
pub const SYSCALL_TTY_SET_FOCUS: u64 = 28;
pub const SYSCALL_ROULETTE: u64 = 4;
pub const SYSCALL_SLEEP_MS: u64 = 5;
pub const SYSCALL_GET_TIME_MS: u64 = 39;
pub const SYSCALL_FB_INFO: u64 = 6;
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
pub const SYSCALL_SURFACE_COMMIT: u64 = 38;

// Shared memory syscalls for Wayland-like compositor
pub const SYSCALL_SHM_CREATE: u64 = 40;
pub const SYSCALL_SHM_MAP: u64 = 41;
pub const SYSCALL_SHM_UNMAP: u64 = 42;
pub const SYSCALL_SHM_DESTROY: u64 = 43;
pub const SYSCALL_SURFACE_ATTACH: u64 = 44;
pub const SYSCALL_FB_FLIP: u64 = 45;

/// Shared memory access flags
pub const SHM_ACCESS_RO: u32 = 0;
pub const SHM_ACCESS_RW: u32 = 1;

pub const USER_FS_OPEN_READ: u32 = 0x1;
pub const USER_FS_OPEN_WRITE: u32 = 0x2;
pub const USER_FS_OPEN_CREAT: u32 = 0x4;
pub const USER_FS_OPEN_APPEND: u32 = 0x8;

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFbInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u8,
    pub pixel_format: u8,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsEntry {
    pub name: [u8; 64],
    pub r#type: u8,
    pub size: u32,
}

impl UserFsEntry {
    pub const fn new() -> Self {
        Self {
            name: [0; 64],
            r#type: 0,
            size: 0,
        }
    }
}

impl Default for UserFsEntry {
    fn default() -> Self {
        Self::new()
    }
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsStat {
    pub r#type: u8,
    pub size: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsList {
    pub entries: *mut UserFsEntry,
    pub max_entries: u32,
    pub count: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
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

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserMouseEvent {
    pub x: i32,
    pub y: i32,
    pub buttons: u8,
}

/// Per-window damage region (surface-local coordinates)
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserWindowDamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// Maximum damage regions per window
pub const MAX_WINDOW_DAMAGE_REGIONS: usize = 8;

#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserWindowInfo {
    pub task_id: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub state: u8,
    pub damage_count: u8,
    pub _padding: [u8; 2],
    /// Shared memory token for this surface (0 if not using shared memory)
    pub shm_token: u32,
    // Individual damage regions
    pub damage_regions: [UserWindowDamageRect; MAX_WINDOW_DAMAGE_REGIONS],
    pub title: [c_char; 32],
}

impl UserWindowInfo {
    /// Returns true if the window has any pending damage
    pub fn is_dirty(&self) -> bool {
        self.damage_count > 0
    }
}

impl Default for UserWindowInfo {
    fn default() -> Self {
        Self {
            task_id: 0,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            state: 0,
            damage_count: 0,
            _padding: [0; 2],
            shm_token: 0,
            damage_regions: [UserWindowDamageRect::default(); MAX_WINDOW_DAMAGE_REGIONS],
            title: [0; 32],
        }
    }
}

/// Damage region for efficient damage tracking (Wayland-style)
#[repr(C)]
#[derive(Default, Copy, Clone, Debug)]
pub struct UserDamageRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
unsafe fn syscall(num: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let mut ret = num;
    unsafe {
        asm!(
            "int 0x80",
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            inout("rax") ret,
            clobber_abi("sysv64"),
            options(nostack),
        );
    }
    ret
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_yield() {
    unsafe {
        syscall(SYSCALL_YIELD, 0, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_write(buf: &[u8]) -> i64 {
    unsafe { syscall(SYSCALL_WRITE, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}


#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read(buf: &mut [u8]) -> i64 {
    unsafe { syscall(SYSCALL_READ, buf.as_ptr() as u64, buf.len() as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_read_char() -> i64 {
    unsafe { syscall(SYSCALL_READ_CHAR, 0, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sleep_ms(ms: u32) {
    unsafe {
        syscall(SYSCALL_SLEEP_MS, ms as u64, 0, 0);
    }
}

/// Returns the current time in milliseconds since boot.
/// Used for frame pacing in the compositor (60Hz target).
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_get_time_ms() -> u64 {
    unsafe { syscall(SYSCALL_GET_TIME_MS, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette() -> u64 {
    unsafe { syscall(SYSCALL_ROULETTE, 0, 0, 0) }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_result(fate_packed: u64) {
    unsafe {
        syscall(SYSCALL_ROULETTE_RESULT, fate_packed, 0, 0);
    }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_roulette_draw(fate: u32) -> i64 {
    unsafe { syscall(SYSCALL_ROULETTE_DRAW, fate as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_exit() -> ! {
    unsafe {
        syscall(SYSCALL_EXIT, 0, 0, 0);
    }
    loop {}
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fb_info(out: &mut UserFbInfo) -> i64 {
    unsafe { syscall(SYSCALL_FB_INFO, out as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_tty_set_focus(task_id: u32) -> i64 {
    unsafe { syscall(SYSCALL_TTY_SET_FOCUS, task_id as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_random_next() -> u32 {
    unsafe { syscall(SYSCALL_RANDOM_NEXT, 0, 0, 0) as u32 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_open(path: *const c_char, flags: u32) -> i64 {
    unsafe { syscall(SYSCALL_FS_OPEN, path as u64, flags as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_close(fd: i32) -> i64 {
    unsafe { syscall(SYSCALL_FS_CLOSE, fd as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_read(fd: i32, buf: *mut c_void, len: usize) -> i64 {
    unsafe { syscall(SYSCALL_FS_READ, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_write(fd: i32, buf: *const c_void, len: usize) -> i64 {
    unsafe { syscall(SYSCALL_FS_WRITE, fd as u64, buf as u64, len as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_stat(path: *const c_char, out_stat: &mut UserFsStat) -> i64 {
    unsafe { syscall(SYSCALL_FS_STAT, path as u64, out_stat as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_mkdir(path: *const c_char) -> i64 {
    unsafe { syscall(SYSCALL_FS_MKDIR, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_unlink(path: *const c_char) -> i64 {
    unsafe { syscall(SYSCALL_FS_UNLINK, path as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_fs_list(path: *const c_char, list: &mut UserFsList) -> i64 {
    unsafe { syscall(SYSCALL_FS_LIST, path as u64, list as *mut _ as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_sys_info(info: &mut UserSysInfo) -> i64 {
    unsafe { syscall(SYSCALL_SYS_INFO, info as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_mouse_read(event: &mut UserMouseEvent) -> i64 {
    unsafe { syscall(SYSCALL_MOUSE_READ, event as *mut _ as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_enumerate_windows(windows: &mut [UserWindowInfo]) -> u64 {
    unsafe {
        syscall(
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
    unsafe { syscall(SYSCALL_SET_WINDOW_POSITION, task_id as u64, x as u64, y as u64) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_set_window_state(task_id: u32, state: u8) -> i64 {
    unsafe { syscall(SYSCALL_SET_WINDOW_STATE, task_id as u64, state as u64, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_raise_window(task_id: u32) -> i64 {
    unsafe { syscall(SYSCALL_RAISE_WINDOW, task_id as u64, 0, 0) as i64 }
}

#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_halt() -> ! {
    unsafe {
        syscall(SYSCALL_HALT, 0, 0, 0);
    }
    loop {}
}

/// Commit a surface's back buffer to front buffer (Wayland-style double buffering)
/// Swaps buffer pointers atomically and transfers damage tracking
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_surface_commit() -> i64 {
    unsafe { syscall(SYSCALL_SURFACE_COMMIT, 0, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_SHM_CREATE, size, flags as u64, 0) as u32 }
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
    unsafe { syscall(SYSCALL_SHM_MAP, token as u64, access as u64, 0) }
}

/// Unmap a shared memory buffer from the caller's address space.
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `virt_addr` - Virtual address from sys_shm_map
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_unmap(virt_addr: u64) -> i64 {
    unsafe { syscall(SYSCALL_SHM_UNMAP, virt_addr, 0, 0) as i64 }
}

/// Destroy a shared memory buffer (owner only).
/// Returns 0 on success, -1 on failure.
///
/// # Arguments
/// * `token` - Token from sys_shm_create
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_shm_destroy(token: u32) -> i64 {
    unsafe { syscall(SYSCALL_SHM_DESTROY, token as u64, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_SURFACE_ATTACH, token as u64, width as u64, height as u64) as i64 }
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
    unsafe { syscall(SYSCALL_FB_FLIP, token as u64, 0, 0) as i64 }
}
