//! Public API for Compositor
//!
//! This module provides the public interface to the compositor, maintaining
//! compatibility with the existing video_bridge callback signatures while
//! internally using the event-driven architecture.
//!
//! All operations go through the global compositor instance which processes
//! events synchronously (since this is a kernel with no async runtime).

use core::ffi::c_int;
use spin::Mutex;

use slopos_drivers::video_bridge::VideoResult;
use slopos_lib::FramebufferInfo;

use super::events::CompositorEvent;
use super::queue::EventQueue;
use super::{Compositor, WindowInfo};

/// Global compositor instance
///
/// Protected by a single mutex. In the event-driven model, this lock is held
/// only during event processing - much briefer than the old per-surface locks.
static COMPOSITOR: Mutex<Compositor> = Mutex::new(Compositor::new());

/// Global event queue for deferred processing (if needed)
static EVENT_QUEUE: EventQueue = EventQueue::new();

// =============================================================================
// Initialization
// =============================================================================

/// Initialize the compositor with framebuffer info
pub fn init(info: FramebufferInfo) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    compositor.init_framebuffer(info)
}

/// Process any pending events in the queue
///
/// This is called periodically (e.g., from timer interrupt or main loop)
/// to process deferred events.
pub fn tick() {
    if EVENT_QUEUE.has_pending() {
        let mut compositor = COMPOSITOR.lock();
        compositor.process_events(&EVENT_QUEUE);
    }
}

// =============================================================================
// Surface Lifecycle
// =============================================================================

/// Register a surface for a task
///
/// Creates a new surface with the given dimensions. The surface is immediately
/// visible and positioned with a cascading offset.
pub fn register_surface_for_task(task_id: u32, width: u32, height: u32, bpp: u8) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::CreateSurface {
        task_id,
        width,
        height,
        bpp,
    };
    match compositor.handle_event(event) {
        Ok(()) => 0,
        Err(e) => e.to_code(),
    }
}

/// Unregister a surface for a task (called on task cleanup)
pub fn unregister_surface_for_task(task_id: u32) {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::DestroySurface { task_id };
    let _ = compositor.handle_event(event);
}

// =============================================================================
// Surface Operations
// =============================================================================

/// Commit back buffer to front (Wayland-style double buffering)
pub fn surface_commit(task_id: u32) -> VideoResult {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::Commit { task_id };
    compositor
        .handle_event(event)
        .map_err(|_| slopos_drivers::video_bridge::VideoError::Invalid)
}

/// Set window position
pub fn surface_set_window_position(task_id: u32, x: i32, y: i32) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::SetPosition { task_id, x, y };
    match compositor.handle_event(event) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Set window state (normal, minimized, maximized)
pub fn surface_set_window_state(task_id: u32, state: u8) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::SetWindowState { task_id, state };
    match compositor.handle_event(event) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Raise window (bring to front)
pub fn surface_raise_window(task_id: u32) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::RaiseWindow { task_id };
    match compositor.handle_event(event) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

// =============================================================================
// Window Enumeration
// =============================================================================

/// Enumerate all visible windows
///
/// Fills the output buffer with WindowInfo structs for all visible surfaces,
/// sorted by z-order. Returns the number of windows written.
pub fn surface_enumerate_windows(out_buffer: *mut WindowInfo, max_count: u32) -> u32 {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let compositor = COMPOSITOR.lock();
    let windows = compositor.enumerate_windows();

    let count = windows.len().min(max_count as usize) as u32;
    for (i, window) in windows.into_iter().take(count as usize).enumerate() {
        unsafe {
            *out_buffer.add(i) = window;
        }
    }

    count
}

// =============================================================================
// Framebuffer Operations
// =============================================================================

/// Page flip from shared memory to MMIO framebuffer
pub fn fb_flip_from_shm(shm_phys: u64, size: usize) -> c_int {
    let mut compositor = COMPOSITOR.lock();
    let event = CompositorEvent::PageFlip { shm_phys, size };
    match compositor.handle_event(event) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Get framebuffer snapshot for legacy code
pub fn framebuffer_snapshot() -> Option<super::FramebufferState> {
    let compositor = COMPOSITOR.lock();
    compositor.framebuffer().copied()
}

// =============================================================================
// Legacy Compatibility - Access to compositor state
// =============================================================================

/// Execute a function with the compositor locked
///
/// This is for legacy code that needs direct access to compositor state.
/// Prefer using the event-based API for new code.
pub fn with_compositor<F, R>(f: F) -> R
where
    F: FnOnce(&Compositor) -> R,
{
    let compositor = COMPOSITOR.lock();
    f(&compositor)
}

/// Execute a function with the compositor locked mutably
///
/// This is for legacy code that needs direct mutable access.
/// Prefer using the event-based API for new code.
pub fn with_compositor_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut Compositor) -> R,
{
    let mut compositor = COMPOSITOR.lock();
    f(&mut compositor)
}

/// Send an event to the queue for deferred processing
///
/// Returns true if the event was queued, false if the queue is full.
pub fn send_event(event: CompositorEvent) -> bool {
    EVENT_QUEUE.enqueue(event)
}

/// Check if there are pending events
pub fn has_pending_events() -> bool {
    EVENT_QUEUE.has_pending()
}
