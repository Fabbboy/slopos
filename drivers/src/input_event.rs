//! Input Event Protocol (Wayland-like per-task input queues)
//!
//! This module implements a Wayland-inspired input event system:
//! - Per-task event queues
//! - Keyboard and pointer focus tracking
//! - Structured input events with timestamps
//!
//! Events are routed to the focused task for each input type.

use core::sync::atomic::{AtomicU32, Ordering};
use slopos_sync::IrqMutex;
use slopos_utils::RingBuffer;

/// Monotonic millisecond timestamp for input events.
///
/// Uses the HPET hardware counter for
/// nanosecond-precision wall time, converted to milliseconds.
pub fn get_timestamp_ms() -> u64 {
    crate::hpet::nanoseconds(crate::hpet::read_counter()) / 1_000_000
}

// Re-export ABI types and constants for consumers.
// All construction and accessor methods live on `InputEvent` in `slopos_abi::input`.
pub use slopos_abi::{
    InputEvent, InputEventData, InputEventType, MAX_EVENTS_PER_TASK, MAX_INPUT_TASKS,
};

// =============================================================================
// Per-Task Event Queue
// =============================================================================

struct TaskEventQueue {
    task_id: u32,
    active: bool,
    events: RingBuffer<InputEvent, MAX_EVENTS_PER_TASK>,
}

impl TaskEventQueue {
    const fn new() -> Self {
        Self {
            task_id: 0,
            active: false,
            events: RingBuffer::new_with(InputEvent {
                event_type: InputEventType::KeyPress,
                _padding: [0; 3],
                timestamp_ms: 0,
                data: InputEventData { data0: 0, data1: 0 },
            }),
        }
    }
}

// =============================================================================
// Global Input Manager
// =============================================================================

struct InputManager {
    /// Per-task event queues
    queues: [TaskEventQueue; MAX_INPUT_TASKS],
    /// Task ID with keyboard focus (0 = no focus)
    keyboard_focus: u32,
    /// Task ID with pointer focus (0 = no focus)
    pointer_focus: u32,
    /// Current pointer position (screen coordinates)
    pointer_x: i32,
    pointer_y: i32,
    /// Current pointer button state
    pointer_buttons: u8,
    /// Window offset for coordinate translation (set by compositor)
    /// Pointer events will be translated from screen coords to window-local coords
    window_offset_x: i32,
    window_offset_y: i32,
}

impl InputManager {
    const fn new() -> Self {
        Self {
            queues: [const { TaskEventQueue::new() }; MAX_INPUT_TASKS],
            keyboard_focus: 0,
            pointer_focus: 0,
            pointer_x: 0,
            pointer_y: 0,
            pointer_buttons: 0,
            window_offset_x: 0,
            window_offset_y: 0,
        }
    }

    fn find_queue(&self, task_id: u32) -> Option<usize> {
        for (i, queue) in self.queues.iter().enumerate() {
            if queue.active && queue.task_id == task_id {
                return Some(i);
            }
        }
        None
    }

    fn find_or_create_queue(&mut self, task_id: u32) -> Option<usize> {
        if let Some(idx) = self.find_queue(task_id) {
            return Some(idx);
        }

        for (i, queue) in self.queues.iter_mut().enumerate() {
            if !queue.active {
                queue.task_id = task_id;
                queue.active = true;
                queue.events.reset();
                return Some(i);
            }
        }

        None
    }
}

static INPUT_MANAGER: IrqMutex<InputManager> = IrqMutex::new(InputManager::new());
static KEYBOARD_FOCUS_FAST: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn has_keyboard_focus() -> bool {
    KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) != 0
}

// =============================================================================
// Public API - Focus Management (Compositor Operations)
// =============================================================================

pub fn input_set_keyboard_focus(task_id: u32) {
    KEYBOARD_FOCUS_FAST.store(task_id, Ordering::Release);
    INPUT_MANAGER.lock().keyboard_focus = task_id;
}

/// Set pointer focus to a task (called by compositor)
/// Also sends enter/leave events. Uses offset (0, 0) for backwards compatibility.
pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    input_set_pointer_focus_with_offset(task_id, 0, 0, timestamp_ms);
}

/// Set pointer focus to a task with window offset for coordinate translation
/// The offset is subtracted from screen coordinates to get window-local coordinates.
/// For a window at screen position (100, 50), pass offset_x=100, offset_y=50.
pub fn input_set_pointer_focus_with_offset(
    task_id: u32,
    offset_x: i32,
    offset_y: i32,
    timestamp_ms: u64,
) {
    let mut mgr = INPUT_MANAGER.lock();
    let old_focus = mgr.pointer_focus;
    let x = mgr.pointer_x;
    let y = mgr.pointer_y;

    // Update window offset for coordinate translation
    mgr.window_offset_x = offset_x;
    mgr.window_offset_y = offset_y;

    if old_focus == task_id {
        return;
    }

    // Send leave event to old focus (with old offset)
    if old_focus != 0 {
        if let Some(idx) = mgr.find_queue(old_focus) {
            mgr.queues[idx]
                .events
                .push_overwrite(InputEvent::pointer_enter_leave(false, x, y, timestamp_ms));
        }
    }

    mgr.pointer_focus = task_id;

    // Send enter event to new focus (with new offset - translated coords)
    if task_id != 0 {
        if let Some(idx) = mgr.find_or_create_queue(task_id) {
            let local_x = x - offset_x;
            let local_y = y - offset_y;
            mgr.queues[idx]
                .events
                .push_overwrite(InputEvent::pointer_enter_leave(
                    true,
                    local_x,
                    local_y,
                    timestamp_ms,
                ));
        }
    }
}

/// Enqueue a window-close request event for a task.
/// Called by compositor syscall path when user clicks a close button.
pub fn input_request_close(task_id: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }

    let mut mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_or_create_queue(task_id) {
        mgr.queues[idx]
            .events
            .push_overwrite(InputEvent::close_request(timestamp_ms));
        true
    } else {
        false
    }
}

/// Enqueue a configure (resize) event for a task.
/// Called by compositor when a window resize operation completes.
pub fn input_send_configure(task_id: u32, width: u32, height: u32, timestamp_ms: u64) -> bool {
    if task_id == 0 {
        return false;
    }

    let mut mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_or_create_queue(task_id) {
        mgr.queues[idx]
            .events
            .push_overwrite(InputEvent::configure(width, height, timestamp_ms));
        true
    } else {
        false
    }
}

/// Get current keyboard focus task ID
pub fn input_get_keyboard_focus() -> u32 {
    INPUT_MANAGER.lock().keyboard_focus
}

/// Get current pointer focus task ID
pub fn input_get_pointer_focus() -> u32 {
    INPUT_MANAGER.lock().pointer_focus
}

/// Get current global pointer position (screen coordinates)
/// Used by compositor to track cursor even when pointer focus is on another task
pub fn input_get_pointer_position() -> (i32, i32) {
    let mgr = INPUT_MANAGER.lock();
    (mgr.pointer_x, mgr.pointer_y)
}

/// Get current global pointer button state
/// Used by compositor to track buttons even when pointer focus is on another task
pub fn input_get_button_state() -> u8 {
    INPUT_MANAGER.lock().pointer_buttons
}

// =============================================================================
// Public API - Event Routing (Called from IRQ handlers)
// =============================================================================

/// Route a keyboard event to the focused task
///
/// Called from IRQ context (keyboard interrupt handler). IrqMutex handles
/// interrupt safety automatically.
pub fn input_route_key_event(scancode: u8, ascii: u8, pressed: bool, timestamp_ms: u64) {
    if KEYBOARD_FOCUS_FAST.load(Ordering::Acquire) == 0 {
        return;
    }

    let mut mgr = INPUT_MANAGER.lock();
    let focus = mgr.keyboard_focus;

    if focus == 0 {
        return;
    }

    if let Some(idx) = mgr.find_or_create_queue(focus) {
        let event_type = if pressed {
            InputEventType::KeyPress
        } else {
            InputEventType::KeyRelease
        };
        mgr.queues[idx].events.push_overwrite(InputEvent::key(
            event_type,
            scancode,
            ascii,
            timestamp_ms,
        ));
    }
}

/// Route a pointer motion event to the focused task (called from mouse IRQ).
/// Coordinates are translated from screen coords to window-local coords.
pub fn input_route_pointer_motion(x: i32, y: i32, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();
    mgr.pointer_x = x;
    mgr.pointer_y = y;

    let focus = mgr.pointer_focus;
    if focus == 0 {
        return;
    }

    let local_x = x - mgr.window_offset_x;
    let local_y = y - mgr.window_offset_y;

    if let Some(idx) = mgr.find_or_create_queue(focus) {
        mgr.queues[idx]
            .events
            .push_overwrite(InputEvent::pointer_motion(local_x, local_y, timestamp_ms));
    }
}

/// Route a pointer button event to the focused task (called from mouse IRQ).
pub fn input_route_pointer_button(button: u8, pressed: bool, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();

    if pressed {
        mgr.pointer_buttons |= button;
    } else {
        mgr.pointer_buttons &= !button;
    }

    let focus = mgr.pointer_focus;
    if focus == 0 {
        return;
    }

    if let Some(idx) = mgr.find_or_create_queue(focus) {
        mgr.queues[idx]
            .events
            .push_overwrite(InputEvent::pointer_button(pressed, button, timestamp_ms));
    }
}

// =============================================================================
// Public API - Client Operations (Syscalls)
// =============================================================================

/// Poll for an input event (non-blocking)
/// Returns the event if available, None if queue is empty
pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    let mut mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].events.try_pop()
    } else {
        None
    }
}

/// Drain up to max_count events from a task's queue in a single lock acquisition.
/// This is much more efficient than calling input_poll() in a loop, as it avoids
/// lock ping-pong with IRQ handlers that enqueue events.
///
/// # Arguments
/// * `task_id` - The task whose queue to drain
/// * `out_buffer` - Pointer to buffer to receive events
/// * `max_count` - Maximum number of events to drain
///
/// # Returns
/// Number of events written to buffer (0 to max_count)
///
/// # Safety
/// Caller must ensure out_buffer points to valid memory for max_count InputEvents.
pub fn input_drain_batch(task_id: u32, out_buffer: *mut InputEvent, max_count: usize) -> usize {
    if out_buffer.is_null() || max_count == 0 {
        return 0;
    }

    let mut mgr = INPUT_MANAGER.lock();
    let idx = match mgr.find_or_create_queue(task_id) {
        Some(i) => i,
        None => return 0,
    };

    let mut count = 0;
    while count < max_count {
        if let Some(event) = mgr.queues[idx].events.try_pop() {
            unsafe {
                out_buffer.add(count).write(event);
            }
            count += 1;
        } else {
            break;
        }
    }
    count
}

/// Peek at the next input event without removing it
pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].events.peek().copied()
    } else {
        None
    }
}

/// Check if a task has pending input events
pub fn input_has_events(task_id: u32) -> bool {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        !mgr.queues[idx].events.is_empty()
    } else {
        false
    }
}

/// Get the number of pending events for a task
pub fn input_event_count(task_id: u32) -> u32 {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].events.len() as u32
    } else {
        0
    }
}

struct ClipboardState {
    data: [u8; slopos_abi::CLIPBOARD_MAX_SIZE],
    len: usize,
}

impl ClipboardState {
    const fn new() -> Self {
        Self {
            data: [0u8; slopos_abi::CLIPBOARD_MAX_SIZE],
            len: 0,
        }
    }
}

static CLIPBOARD: IrqMutex<ClipboardState> = IrqMutex::new(ClipboardState::new());

pub fn clipboard_copy(src: &[u8]) -> usize {
    let mut clip = CLIPBOARD.lock();
    let copy_len = src.len().min(slopos_abi::CLIPBOARD_MAX_SIZE);
    clip.data[..copy_len].copy_from_slice(&src[..copy_len]);
    clip.len = copy_len;
    copy_len
}

pub fn clipboard_paste(dst: &mut [u8]) -> usize {
    let clip = CLIPBOARD.lock();
    if clip.len == 0 {
        return 0;
    }

    let copy_len = clip.len.min(dst.len());
    dst[..copy_len].copy_from_slice(&clip.data[..copy_len]);
    copy_len
}

// =============================================================================
// Task Cleanup
// =============================================================================

/// Clean up input queue for a terminated task
pub fn input_cleanup_task(task_id: u32) {
    let mut mgr = INPUT_MANAGER.lock();

    // Clear focus if this task had it
    if mgr.keyboard_focus == task_id {
        mgr.keyboard_focus = 0;
        KEYBOARD_FOCUS_FAST.store(0, Ordering::Release);
    }
    if mgr.pointer_focus == task_id {
        mgr.pointer_focus = 0;
    }

    if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].active = false;
        mgr.queues[idx].task_id = 0;
        mgr.queues[idx].events.reset();
    }
}
