use core::ffi::c_int;
use core::sync::atomic::{AtomicPtr, Ordering};
use spin::Once;

// Re-export ABI types for consumers
pub use slopos_abi::{WindowDamageRect, WindowInfo, MAX_WINDOW_DAMAGE_REGIONS};

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FramebufferInfoC {
    pub initialized: u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub pixel_format: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoError {
    NoFramebuffer,
    OutOfBounds,
    Invalid,
}

pub type VideoResult = Result<(), VideoError>;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoCallbacks {
    pub framebuffer_get_info: Option<fn() -> *mut FramebufferInfoC>,
    pub roulette_draw: Option<fn(u32) -> c_int>,
    pub surface_enumerate_windows: Option<fn(*mut WindowInfo, u32) -> u32>,
    pub surface_set_window_position: Option<fn(u32, i32, i32) -> c_int>,
    pub surface_set_window_state: Option<fn(u32, u8) -> c_int>,
    pub surface_raise_window: Option<fn(u32) -> c_int>,
    pub surface_commit: Option<fn(u32) -> c_int>,
    /// Copy shared memory buffer to framebuffer MMIO (page flip)
    /// Args: (shm_phys_addr, size) -> c_int
    pub fb_flip: Option<fn(u64, usize) -> c_int>,
    /// Register a surface for a task (called on surface_attach)
    /// Args: (task_id, width, height, bpp, shm_token) -> c_int
    pub register_surface: Option<fn(u32, u32, u32, u8, u32) -> c_int>,
    /// Drain the compositor queue (called by compositor at start of each frame)
    pub drain_queue: Option<fn()>,
    /// Request a frame callback (Wayland wl_surface.frame)
    /// Args: (task_id) -> c_int
    pub surface_request_frame_callback: Option<fn(u32) -> c_int>,
    /// Mark frames as done (called by compositor after present)
    /// Args: (present_time_ms)
    pub surface_mark_frames_done: Option<fn(u64)>,
    /// Poll for frame completion
    /// Args: (task_id) -> timestamp (0 if not done)
    pub surface_poll_frame_done: Option<fn(u32) -> u64>,
    /// Add damage region to surface (Wayland wl_surface.damage)
    /// Args: (task_id, x, y, width, height) -> c_int
    pub surface_add_damage: Option<fn(u32, i32, i32, i32, i32) -> c_int>,
    /// Get back buffer age for damage accumulation
    /// Args: (task_id) -> age (0 = undefined, N = N frames old)
    pub surface_get_buffer_age: Option<fn(u32) -> u8>,
    /// Set surface role (toplevel, popup, subsurface)
    /// Args: (task_id, role) -> c_int
    pub surface_set_role: Option<fn(u32, u8) -> c_int>,
    /// Set parent surface for subsurfaces
    /// Args: (task_id, parent_task_id) -> c_int
    pub surface_set_parent: Option<fn(u32, u32) -> c_int>,
    /// Set relative position for subsurfaces
    /// Args: (task_id, rel_x, rel_y) -> c_int
    pub surface_set_relative_position: Option<fn(u32, i32, i32) -> c_int>,
}

static VIDEO_CALLBACKS: Once<VideoCallbacks> = Once::new();

static FRAMEBUFFER_INFO: AtomicPtr<FramebufferInfoC> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_video_callbacks(callbacks: VideoCallbacks) {
    let _ = VIDEO_CALLBACKS.call_once(|| callbacks);
}

pub fn framebuffer_get_info() -> *mut FramebufferInfoC {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.framebuffer_get_info {
            let ptr = cb();
            FRAMEBUFFER_INFO.store(ptr, Ordering::Relaxed);
            return ptr;
        }
    }
    FRAMEBUFFER_INFO.load(Ordering::Relaxed)
}

pub fn roulette_draw(fate: u32) -> VideoResult {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.roulette_draw {
            return video_result_from_code(cb(fate));
        }
    }
    Err(VideoError::NoFramebuffer)
}

pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_enumerate_windows {
            return cb(out_buffer, max_count);
        }
    }
    0
}

pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_window_position {
            return cb(task_id, x, y);
        }
    }
    -1
}

pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_window_state {
            return cb(task_id, state);
        }
    }
    -1
}

pub fn surface_raise_window(task_id: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_raise_window {
            return cb(task_id);
        }
    }
    -1
}

/// Commit a surface's back buffer to front buffer (Wayland-style double buffering)
pub fn surface_commit(task_id: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_commit {
            return cb(task_id);
        }
    }
    -1
}

/// Register a surface for a task (called when surface_attach is invoked)
/// This creates the surface entry so it appears in enumerate_windows
pub fn register_surface(task_id: u32, width: u32, height: u32, bpp: u8, shm_token: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.register_surface {
            return cb(task_id, width, height, bpp, shm_token);
        }
    }
    -1
}

/// Drain the compositor queue (called by compositor at start of each frame)
/// Processes all pending client operations (commits, registers, unregisters)
pub fn drain_queue() {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.drain_queue {
            cb();
        }
    }
}

fn video_result_from_code(rc: c_int) -> VideoResult {
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}

/// Copy from a shared memory buffer to the MMIO framebuffer.
/// This is the "page flip" operation for the userland compositor.
///
/// # Arguments
/// * `shm_phys` - Physical address of the source shared memory buffer
/// * `size` - Size of the buffer in bytes
///
/// # Returns
/// 0 on success, -1 on failure
pub fn fb_flip_from_shm(shm_phys: u64, size: usize) -> c_int {
    // Get framebuffer info
    let fb_ptr = framebuffer_get_info();
    if fb_ptr.is_null() {
        return -1;
    }

    let fb_info = unsafe { &*fb_ptr };
    if fb_info.initialized == 0 {
        return -1;
    }

    // Calculate framebuffer size
    let fb_size = (fb_info.pitch * fb_info.height) as usize;

    // Ensure we don't copy more than framebuffer size
    let copy_size = size.min(fb_size);
    if copy_size == 0 {
        return -1;
    }

    // Convert physical address to virtual using HHDM
    let shm_virt = slopos_mm::phys_to_virt(shm_phys);
    if shm_virt == 0 {
        return -1;
    }

    // Get framebuffer base address from the video subsystem
    // We need to call into the video crate to get the actual FB base
    // For now, use a callback-based approach
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        // Use the existing framebuffer infrastructure
        // The framebuffer base is stored in the video crate
        // We'll add a new callback for the flip operation
        if let Some(flip_cb) = cbs.fb_flip {
            return flip_cb(shm_phys, copy_size);
        }
    }

    // Fallback: no flip callback registered
    -1
}

// Add fb_flip to VideoCallbacks
impl VideoCallbacks {
    /// Check if fb_flip is available
    pub fn has_fb_flip(&self) -> bool {
        self.fb_flip.is_some()
    }
}

// =============================================================================
// Frame Callback Protocol (Wayland wl_surface.frame)
// =============================================================================

/// Request a frame callback (client API).
/// Called by clients via syscall to request notification when frame is presented.
pub fn surface_request_frame_callback(task_id: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_request_frame_callback {
            return cb(task_id);
        }
    }
    -1
}

/// Mark frames as done (compositor API).
/// Called by compositor after presenting a frame to notify clients.
pub fn surface_mark_frames_done(present_time_ms: u64) {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_mark_frames_done {
            cb(present_time_ms);
        }
    }
}

/// Poll for frame completion (client API).
/// Returns presentation timestamp if done, 0 if still pending.
pub fn surface_poll_frame_done(task_id: u32) -> u64 {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_poll_frame_done {
            return cb(task_id);
        }
    }
    0
}

// =============================================================================
// Damage Tracking Protocol (Wayland wl_surface.damage)
// =============================================================================

/// Add damage region to surface's back buffer (client API).
/// Called by clients via syscall to mark regions that need redrawing.
pub fn surface_add_damage(task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_add_damage {
            return cb(task_id, x, y, width, height);
        }
    }
    -1
}

/// Get back buffer age for damage accumulation (client API).
/// Returns 0 if buffer content is undefined (must redraw everything).
/// Returns N if buffer contains content from N frames ago.
pub fn surface_get_buffer_age(task_id: u32) -> u8 {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_get_buffer_age {
            return cb(task_id);
        }
    }
    0
}

// =============================================================================
// Surface Role Protocol (Wayland xdg_toplevel, xdg_popup, wl_subsurface)
// =============================================================================

/// Set surface role (client API).
/// Role can only be set once per surface.
/// Values: 0 = None, 1 = Toplevel, 2 = Popup, 3 = Subsurface
pub fn surface_set_role(task_id: u32, role: u8) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_role {
            return cb(task_id, role);
        }
    }
    -1
}

/// Set parent surface for subsurfaces (client API).
/// Only valid for surfaces with role Subsurface.
pub fn surface_set_parent(task_id: u32, parent_task_id: u32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_parent {
            return cb(task_id, parent_task_id);
        }
    }
    -1
}

/// Set relative position for subsurfaces (client API).
/// Position is relative to parent's top-left corner.
pub fn surface_set_relative_position(task_id: u32, rel_x: i32, rel_y: i32) -> c_int {
    if let Some(cbs) = VIDEO_CALLBACKS.get() {
        if let Some(cb) = cbs.surface_set_relative_position {
            return cb(task_id, rel_x, rel_y);
        }
    }
    -1
}
