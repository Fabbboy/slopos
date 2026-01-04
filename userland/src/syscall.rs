use core::arch::asm;
use core::ffi::{c_char, c_void};
use core::num::NonZeroU32;
use core::ptr::NonNull;

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
// Input event protocol (Wayland-like per-task queues)
pub const SYSCALL_INPUT_POLL: u64 = 60;
pub const SYSCALL_INPUT_HAS_EVENTS: u64 = 61;
pub const SYSCALL_INPUT_SET_FOCUS: u64 = 62;

/// Shared memory access flags
pub const SHM_ACCESS_RO: u32 = 0;
pub const SHM_ACCESS_RW: u32 = 1;

// =============================================================================
// Pixel Formats (Wayland wl_shm compatible)
// =============================================================================

/// Pixel format for shared memory buffers (matches Wayland wl_shm formats).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit ARGB (alpha in high byte, red in bits 16-23)
    Argb8888 = 0,
    /// 32-bit XRGB (alpha ignored, red in bits 16-23)
    Xrgb8888 = 1,
    /// 24-bit RGB (no alpha)
    Rgb888 = 2,
    /// 24-bit BGR (no alpha)
    Bgr888 = 3,
    /// 32-bit RGBA (red in high byte, alpha in bits 0-7)
    Rgba8888 = 4,
    /// 32-bit BGRA (blue in high byte, alpha in bits 0-7)
    Bgra8888 = 5,
}

impl PixelFormat {
    /// Convert from u32 representation.
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(Self::Argb8888),
            1 => Some(Self::Xrgb8888),
            2 => Some(Self::Rgb888),
            3 => Some(Self::Bgr888),
            4 => Some(Self::Rgba8888),
            5 => Some(Self::Bgra8888),
            _ => None,
        }
    }

    /// Get bytes per pixel for this format.
    pub fn bytes_per_pixel(&self) -> u8 {
        match self {
            Self::Argb8888 | Self::Xrgb8888 | Self::Rgba8888 | Self::Bgra8888 => 4,
            Self::Rgb888 | Self::Bgr888 => 3,
        }
    }

    /// Check if format has an alpha channel.
    pub fn has_alpha(&self) -> bool {
        matches!(self, Self::Argb8888 | Self::Rgba8888 | Self::Bgra8888)
    }
}

// =============================================================================
// Surface Roles (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Role of a surface in the compositor hierarchy.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SurfaceRole {
    /// No role assigned yet
    #[default]
    None = 0,
    /// Top-level window (regular application window)
    Toplevel = 1,
    /// Popup surface (menus, tooltips, dropdowns)
    Popup = 2,
    /// Subsurface (child surface positioned relative to parent)
    Subsurface = 3,
}

impl SurfaceRole {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::None),
            1 => Some(Self::Toplevel),
            2 => Some(Self::Popup),
            3 => Some(Self::Subsurface),
            _ => None,
        }
    }
}

// =============================================================================
// Input Event Types (Wayland-like per-task input queues)
// =============================================================================

/// Type of input event.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEventType {
    /// Key pressed
    KeyPress = 0,
    /// Key released
    KeyRelease = 1,
    /// Pointer (mouse) motion
    PointerMotion = 2,
    /// Pointer button pressed
    PointerButtonPress = 3,
    /// Pointer button released
    PointerButtonRelease = 4,
    /// Pointer entered surface
    PointerEnter = 5,
    /// Pointer left surface
    PointerLeave = 6,
}

impl InputEventType {
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(Self::KeyPress),
            1 => Some(Self::KeyRelease),
            2 => Some(Self::PointerMotion),
            3 => Some(Self::PointerButtonPress),
            4 => Some(Self::PointerButtonRelease),
            5 => Some(Self::PointerEnter),
            6 => Some(Self::PointerLeave),
            _ => None,
        }
    }

    /// Returns true if this is a key event (press or release).
    pub fn is_key_event(&self) -> bool {
        matches!(self, Self::KeyPress | Self::KeyRelease)
    }

    /// Returns true if this is a pointer event.
    pub fn is_pointer_event(&self) -> bool {
        !self.is_key_event()
    }
}

/// Input event data (union-like structure).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEventData {
    /// For key events: scancode in low 16 bits, ASCII in high 16 bits
    /// For pointer motion: x in low 32 bits, y in high 32 bits (packed as i16)
    /// For pointer button: button code
    pub data0: u32,
    pub data1: u32,
}

/// A complete input event.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    /// Type of event
    pub event_type: InputEventType,
    /// Padding for alignment
    pub _padding: [u8; 3],
    /// Timestamp in milliseconds
    pub timestamp_ms: u64,
    /// Event-specific data
    pub data: InputEventData,
}

impl Default for InputEvent {
    fn default() -> Self {
        Self {
            event_type: InputEventType::KeyPress,
            _padding: [0; 3],
            timestamp_ms: 0,
            data: InputEventData::default(),
        }
    }
}

impl InputEvent {
    /// Extract scancode from key event.
    pub fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// Extract ASCII from key event.
    pub fn key_ascii(&self) -> u8 {
        ((self.data.data0 >> 16) & 0xFF) as u8
    }

    /// Extract X coordinate from pointer event.
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    /// Extract Y coordinate from pointer event.
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    /// Extract button from pointer button event.
    pub fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }
}

/// Focus type for input_set_focus syscall.
pub const INPUT_FOCUS_KEYBOARD: u32 = 0;
pub const INPUT_FOCUS_POINTER: u32 = 1;

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
unsafe fn syscall4(num: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let mut ret = num;
    unsafe {
        asm!(
            "int 0x80",
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("rcx") arg3,
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

/// Drain the compositor queue (compositor only).
/// Processes all pending client operations (commits, registers, unregisters).
/// Must be called at the start of each compositor frame, before enumerate_windows.
#[inline(always)]
#[unsafe(link_section = ".user_text")]
pub fn sys_drain_queue() {
    unsafe { syscall(SYSCALL_DRAIN_QUEUE, 0, 0, 0); }
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
    unsafe { syscall(SYSCALL_SHM_ACQUIRE, token as u64, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_SHM_RELEASE, token as u64, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_SHM_POLL_RELEASED, token as u64, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_SURFACE_FRAME, 0, 0, 0) as i64 }
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
    unsafe { syscall(SYSCALL_POLL_FRAME_DONE, 0, 0, 0) }
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
    unsafe { syscall(SYSCALL_MARK_FRAMES_DONE, present_time_ms, 0, 0); }
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
    unsafe { syscall(SYSCALL_SHM_GET_FORMATS, 0, 0, 0) as u32 }
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
    unsafe { syscall(SYSCALL_SHM_CREATE_WITH_FORMAT, size, format as u64, 0) as u32 }
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
        syscall4(
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
    unsafe { syscall(SYSCALL_BUFFER_AGE, 0, 0, 0) as u8 }
}

// =============================================================================
// Surface Role Syscalls (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Set the role of a surface.
/// Role can only be set once per surface (Wayland semantics).
/// Returns 0 on success, -1 if role already set or invalid.
pub fn sys_surface_set_role(role: SurfaceRole) -> i64 {
    unsafe { syscall(SYSCALL_SURFACE_SET_ROLE, role as u64, 0, 0) as i64 }
}

/// Set the parent surface for a subsurface.
/// Only valid for surfaces with role Subsurface.
/// Returns 0 on success, -1 on failure.
pub fn sys_surface_set_parent(parent_task_id: u32) -> i64 {
    unsafe { syscall(SYSCALL_SURFACE_SET_PARENT, parent_task_id as u64, 0, 0) as i64 }
}

/// Set the relative position of a subsurface.
/// Position is relative to the parent surface's top-left corner.
/// Only valid for surfaces with role Subsurface.
pub fn sys_surface_set_relative_position(rel_x: i32, rel_y: i32) -> i64 {
    unsafe { syscall(SYSCALL_SURFACE_SET_REL_POS, rel_x as u64, rel_y as u64, 0) as i64 }
}

// =============================================================================
// Input Event Protocol (Wayland-like per-task input queues)
// =============================================================================

/// Poll for an input event (non-blocking).
/// Returns Some(event) if an event was available, None if queue is empty.
pub fn sys_input_poll(event_out: &mut InputEvent) -> Option<InputEvent> {
    let result = unsafe {
        syscall(
            SYSCALL_INPUT_POLL,
            event_out as *mut InputEvent as u64,
            0,
            0,
        )
    };
    if result == 1 {
        Some(*event_out)
    } else {
        None
    }
}

/// Check if the current task has pending input events.
/// Returns the number of pending events.
pub fn sys_input_has_events() -> u32 {
    unsafe { syscall(SYSCALL_INPUT_HAS_EVENTS, 0, 0, 0) as u32 }
}

/// Set keyboard or pointer focus to a task (compositor only).
/// focus_type: 0 = keyboard, 1 = pointer
/// Returns 0 on success, -1 on failure.
pub fn sys_input_set_focus(target_task_id: u32, focus_type: u32) -> i64 {
    unsafe {
        syscall(
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

// =============================================================================
// Safe Shared Memory Abstractions
// =============================================================================

/// Error type for shared memory operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShmError {
    /// Failed to allocate physical memory for buffer
    AllocationFailed,
    /// Failed to map buffer into address space
    MappingFailed,
    /// Invalid or expired buffer token
    InvalidToken,
    /// Operation not permitted (e.g., non-owner trying to destroy)
    PermissionDenied,
    /// Maximum number of shared buffers reached
    BufferLimitReached,
    /// Maximum number of mappings per buffer reached
    MappingLimitReached,
    /// Invalid size (zero or too large)
    InvalidSize,
}

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
        sys_shm_unmap(self.ptr.as_ptr() as u64);
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
        sys_shm_unmap(self.ptr.as_ptr() as u64);
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
