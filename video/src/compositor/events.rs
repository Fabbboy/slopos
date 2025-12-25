//! Compositor Event Types
//!
//! All mutations to the compositor and surfaces flow through this event system.
//! This enables a single-threaded, lock-free, Wayland-inspired architecture.

use core::ffi::c_int;

/// Window state: normal (default)
pub const WINDOW_STATE_NORMAL: u8 = 0;
/// Window state: minimized
pub const WINDOW_STATE_MINIMIZED: u8 = 1;
/// Window state: maximized
pub const WINDOW_STATE_MAXIMIZED: u8 = 2;

/// Compositor event types - all mutations flow through this enum.
///
/// This follows the Wayland model where clients send requests (events)
/// to the compositor, which processes them sequentially in a single thread.
#[derive(Debug)]
pub enum CompositorEvent {
    // === Surface Lifecycle ===
    /// Create a new surface for a task
    CreateSurface {
        task_id: u32,
        width: u32,
        height: u32,
        bpp: u8,
    },

    /// Destroy a surface (task cleanup)
    DestroySurface { task_id: u32 },

    // === Surface State Mutations ===
    /// Commit back buffer to front (Wayland wl_surface.commit)
    Commit { task_id: u32 },

    /// Set window position
    SetPosition { task_id: u32, x: i32, y: i32 },

    /// Set window state (normal, minimized, maximized)
    SetWindowState { task_id: u32, state: u8 },

    /// Raise window (bring to front)
    RaiseWindow { task_id: u32 },

    /// Set window visibility
    SetVisible { task_id: u32, visible: bool },

    /// Add damage region to back buffer
    AddDamage {
        task_id: u32,
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
    },

    // === Framebuffer Operations ===
    /// Page flip from shared memory to MMIO framebuffer
    PageFlip { shm_phys: u64, size: usize },
}

/// Result type for compositor operations
pub type CompositorResult = Result<(), CompositorError>;

/// Compositor error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorError {
    /// Surface not found for the given task ID
    SurfaceNotFound,
    /// Failed to allocate memory for surface buffers
    AllocationFailed,
    /// Surface already exists for this task
    SurfaceExists,
    /// Event queue is full
    QueueFull,
    /// Framebuffer not initialized
    NoFramebuffer,
    /// Invalid parameter
    Invalid,
}

impl CompositorError {
    /// Convert error to C-style return code
    pub fn to_code(self) -> c_int {
        match self {
            CompositorError::SurfaceNotFound => -1,
            CompositorError::AllocationFailed => -2,
            CompositorError::SurfaceExists => 0, // Not really an error
            CompositorError::QueueFull => -3,
            CompositorError::NoFramebuffer => -4,
            CompositorError::Invalid => -5,
        }
    }
}
