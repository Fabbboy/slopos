//! Video services trait interface - breaks circular dependencies between crates.
//!
//! These traits are defined in `abi` (no dependencies) so that:
//! - `drivers` can depend on `abi` and call through trait objects
//! - `video` can depend on `abi` and implement the traits
//! - `boot` can depend on both and wire them together
//!
//! This replaces the 349-line VideoCallbacks function pointer system in video_bridge.rs

use core::ffi::c_int;

use crate::window::WindowInfo;

/// Framebuffer information structure (C ABI compatible).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct FramebufferInfoC {
    pub initialized: u8,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub bpp: u32,
    pub pixel_format: u32,
}

impl FramebufferInfoC {
    /// Create a zeroed FramebufferInfoC for static initialization.
    pub const fn new() -> Self {
        Self {
            initialized: 0,
            width: 0,
            height: 0,
            pitch: 0,
            bpp: 0,
            pixel_format: 0,
        }
    }
}

/// Video subsystem error types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoError {
    NoFramebuffer,
    OutOfBounds,
    Invalid,
}

/// Result type for video operations.
pub type VideoResult = Result<(), VideoError>;

/// Convert c_int result code to VideoResult.
#[inline]
pub fn video_result_from_code(rc: c_int) -> VideoResult {
    if rc == 0 {
        Ok(())
    } else {
        Err(VideoError::Invalid)
    }
}

/// Core video services trait.
/// Provides compositor, surface management, and framebuffer operations.
pub trait VideoServices: Send + Sync {
    /// Get framebuffer information.
    fn framebuffer_get_info(&self) -> *mut FramebufferInfoC;

    /// Draw roulette wheel animation.
    fn roulette_draw(&self, fate: u32) -> c_int;

    /// Enumerate windows for compositor.
    fn surface_enumerate_windows(&self, out: *mut WindowInfo, max: u32) -> u32;

    /// Set window position (compositor API).
    fn surface_set_window_position(&self, task_id: u32, x: i32, y: i32) -> c_int;

    /// Set window state (compositor API).
    fn surface_set_window_state(&self, task_id: u32, state: u8) -> c_int;

    /// Raise window to top (compositor API).
    fn surface_raise_window(&self, task_id: u32) -> c_int;

    /// Commit surface back buffer to front (client API).
    fn surface_commit(&self, task_id: u32) -> c_int;

    /// Register a surface for a task (called on surface_attach).
    fn register_surface(&self, task_id: u32, width: u32, height: u32, shm_token: u32) -> c_int;

    /// Drain compositor queue (called at start of each frame).
    fn drain_queue(&self);

    /// Copy shared memory buffer to framebuffer MMIO (page flip).
    fn fb_flip(&self, shm_phys: u64, size: usize) -> c_int;

    /// Request a frame callback (Wayland wl_surface.frame).
    fn surface_request_frame_callback(&self, task_id: u32) -> c_int;

    /// Mark frames as done (compositor API, after present).
    fn surface_mark_frames_done(&self, present_time_ms: u64);

    /// Poll for frame completion.
    fn surface_poll_frame_done(&self, task_id: u32) -> u64;

    /// Add damage region to surface (Wayland wl_surface.damage).
    fn surface_add_damage(&self, task_id: u32, x: i32, y: i32, width: i32, height: i32) -> c_int;

    /// Get back buffer age for damage accumulation.
    fn surface_get_buffer_age(&self, task_id: u32) -> u8;

    /// Set surface role (toplevel, popup, subsurface).
    fn surface_set_role(&self, task_id: u32, role: u8) -> c_int;

    /// Set parent surface for subsurfaces.
    fn surface_set_parent(&self, task_id: u32, parent_task_id: u32) -> c_int;

    /// Set relative position for subsurfaces.
    fn surface_set_relative_position(&self, task_id: u32, rel_x: i32, rel_y: i32) -> c_int;

    /// Set window title.
    fn surface_set_title(&self, task_id: u32, title_ptr: *const u8, title_len: usize) -> c_int;
}
