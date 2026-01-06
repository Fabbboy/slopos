//! Input Event Protocol (Wayland-like per-task input queues)
//!
//! This module implements a Wayland-inspired input event system:
//! - Per-task event queues
//! - Keyboard and pointer focus tracking
//! - Structured input events with timestamps
//!
//! Events are routed to the focused task for each input type.

use spin::Mutex;
use slopos_lib::cpu;

// Re-export ABI types and constants for consumers
pub use slopos_abi::{InputEvent, InputEventData, InputEventType, MAX_EVENTS_PER_TASK, MAX_INPUT_TASKS};

/// Extension trait for InputEvent construction methods
pub trait InputEventExt {
    /// Create a key event
    fn key(event_type: InputEventType, scancode: u8, ascii: u8, timestamp_ms: u64) -> Self;
    /// Create a pointer motion event
    fn pointer_motion(x: i32, y: i32, timestamp_ms: u64) -> Self;
    /// Create a pointer button event
    fn pointer_button(pressed: bool, button: u8, timestamp_ms: u64) -> Self;
    /// Create a pointer enter/leave event
    fn pointer_enter_leave(enter: bool, x: i32, y: i32, timestamp_ms: u64) -> Self;
    /// Extract scancode from key event
    fn key_scancode(&self) -> u8;
    /// Extract ASCII from key event
    fn key_ascii(&self) -> u8;
    /// Extract X coordinate from pointer event
    fn pointer_x(&self) -> i32;
    /// Extract Y coordinate from pointer event
    fn pointer_y(&self) -> i32;
    /// Extract button from pointer button event
    fn pointer_button_code(&self) -> u8;
}

impl InputEventExt for InputEvent {
    fn key(event_type: InputEventType, scancode: u8, ascii: u8, timestamp_ms: u64) -> Self {
        Self {
            event_type,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: (scancode as u32) | ((ascii as u32) << 16),
                data1: 0,
            },
        }
    }

    fn pointer_motion(x: i32, y: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: InputEventType::PointerMotion,
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: x as u32,
                data1: y as u32,
            },
        }
    }

    fn pointer_button(pressed: bool, button: u8, timestamp_ms: u64) -> Self {
        Self {
            event_type: if pressed {
                InputEventType::PointerButtonPress
            } else {
                InputEventType::PointerButtonRelease
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: button as u32,
                data1: 0,
            },
        }
    }

    fn pointer_enter_leave(enter: bool, x: i32, y: i32, timestamp_ms: u64) -> Self {
        Self {
            event_type: if enter {
                InputEventType::PointerEnter
            } else {
                InputEventType::PointerLeave
            },
            _padding: [0; 3],
            timestamp_ms,
            data: InputEventData {
                data0: x as u32,
                data1: y as u32,
            },
        }
    }

    fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    fn key_ascii(&self) -> u8 {
        ((self.data.data0 >> 16) & 0xFF) as u8
    }

    fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }
}

// =============================================================================
// Per-Task Event Queue
// =============================================================================

/// Event queue for a single task
struct TaskEventQueue {
    /// Task ID this queue belongs to
    task_id: u32,
    /// Whether this slot is active
    active: bool,
    /// Circular buffer of events
    events: [InputEvent; MAX_EVENTS_PER_TASK],
    /// Head index (next write position)
    head: usize,
    /// Tail index (next read position)
    tail: usize,
    /// Number of events in queue
    count: usize,
}

impl TaskEventQueue {
    const fn new() -> Self {
        Self {
            task_id: 0,
            active: false,
            events: [InputEvent {
                event_type: InputEventType::KeyPress,
                _padding: [0; 3],
                timestamp_ms: 0,
                data: InputEventData { data0: 0, data1: 0 },
            }; MAX_EVENTS_PER_TASK],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    fn push(&mut self, event: InputEvent) {
        if self.count >= MAX_EVENTS_PER_TASK {
            // Drop oldest event
            self.tail = (self.tail + 1) % MAX_EVENTS_PER_TASK;
            self.count -= 1;
        }
        self.events[self.head] = event;
        self.head = (self.head + 1) % MAX_EVENTS_PER_TASK;
        self.count += 1;
    }

    fn pop(&mut self) -> Option<InputEvent> {
        if self.count == 0 {
            return None;
        }
        let event = self.events[self.tail];
        self.tail = (self.tail + 1) % MAX_EVENTS_PER_TASK;
        self.count -= 1;
        Some(event)
    }

    fn peek(&self) -> Option<&InputEvent> {
        if self.count == 0 {
            return None;
        }
        Some(&self.events[self.tail])
    }

    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
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
        // First, try to find existing queue
        if let Some(idx) = self.find_queue(task_id) {
            return Some(idx);
        }

        // Find a free slot
        for (i, queue) in self.queues.iter_mut().enumerate() {
            if !queue.active {
                queue.task_id = task_id;
                queue.active = true;
                queue.clear();
                return Some(i);
            }
        }

        None
    }
}

static INPUT_MANAGER: Mutex<InputManager> = Mutex::new(InputManager::new());

// =============================================================================
// Public API - Focus Management (Compositor Operations)
// =============================================================================

/// Set keyboard focus to a task (called by compositor)
pub fn input_set_keyboard_focus(task_id: u32) {
    // Use irqsave/irqrestore to prevent deadlock if keyboard IRQ fires while holding lock
    let flags = cpu::save_flags_cli();
    let mut mgr = INPUT_MANAGER.lock();
    mgr.keyboard_focus = task_id;
    drop(mgr);
    cpu::restore_flags(flags);
}

/// Set pointer focus to a task (called by compositor)
/// Also sends enter/leave events. Uses offset (0, 0) for backwards compatibility.
pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    input_set_pointer_focus_with_offset(task_id, 0, 0, timestamp_ms);
}

/// Set pointer focus to a task with window offset for coordinate translation
/// The offset is subtracted from screen coordinates to get window-local coordinates.
/// For a window at screen position (100, 50), pass offset_x=100, offset_y=50.
pub fn input_set_pointer_focus_with_offset(task_id: u32, offset_x: i32, offset_y: i32, timestamp_ms: u64) {
    // Use irqsave/irqrestore to prevent deadlock if mouse IRQ fires while holding lock
    let flags = cpu::save_flags_cli();
    let mut mgr = INPUT_MANAGER.lock();
    let old_focus = mgr.pointer_focus;
    let x = mgr.pointer_x;
    let y = mgr.pointer_y;

    // Update window offset for coordinate translation
    mgr.window_offset_x = offset_x;
    mgr.window_offset_y = offset_y;

    if old_focus == task_id {
        drop(mgr);
        cpu::restore_flags(flags);
        return;
    }

    // Send leave event to old focus (with old offset)
    if old_focus != 0 {
        if let Some(idx) = mgr.find_queue(old_focus) {
            mgr.queues[idx].push(InputEvent::pointer_enter_leave(false, x, y, timestamp_ms));
        }
    }

    mgr.pointer_focus = task_id;

    // Send enter event to new focus (with new offset - translated coords)
    if task_id != 0 {
        if let Some(idx) = mgr.find_or_create_queue(task_id) {
            let local_x = x - offset_x;
            let local_y = y - offset_y;
            mgr.queues[idx].push(InputEvent::pointer_enter_leave(true, local_x, local_y, timestamp_ms));
        }
    }
    drop(mgr);
    cpu::restore_flags(flags);
}

/// Get current keyboard focus task ID
pub fn input_get_keyboard_focus() -> u32 {
    let flags = cpu::save_flags_cli();
    let result = INPUT_MANAGER.lock().keyboard_focus;
    cpu::restore_flags(flags);
    result
}

/// Get current pointer focus task ID
pub fn input_get_pointer_focus() -> u32 {
    let flags = cpu::save_flags_cli();
    let result = INPUT_MANAGER.lock().pointer_focus;
    cpu::restore_flags(flags);
    result
}

/// Get current global pointer position (screen coordinates)
/// Used by compositor to track cursor even when pointer focus is on another task
pub fn input_get_pointer_position() -> (i32, i32) {
    let flags = cpu::save_flags_cli();
    let mgr = INPUT_MANAGER.lock();
    let result = (mgr.pointer_x, mgr.pointer_y);
    drop(mgr);
    cpu::restore_flags(flags);
    result
}

/// Get current global pointer button state
/// Used by compositor to track buttons even when pointer focus is on another task
pub fn input_get_button_state() -> u8 {
    let flags = cpu::save_flags_cli();
    let result = INPUT_MANAGER.lock().pointer_buttons;
    cpu::restore_flags(flags);
    result
}

// =============================================================================
// Public API - Event Routing (Called from IRQ handlers)
// =============================================================================

/// Route a keyboard event to the focused task
pub fn input_route_key_event(scancode: u8, ascii: u8, pressed: bool, timestamp_ms: u64) {
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
        mgr.queues[idx].push(InputEvent::key(event_type, scancode, ascii, timestamp_ms));
    }
}

/// Route a pointer motion event to the focused task
/// Coordinates are translated from screen coords to window-local coords
pub fn input_route_pointer_motion(x: i32, y: i32, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();
    mgr.pointer_x = x;
    mgr.pointer_y = y;

    let focus = mgr.pointer_focus;
    if focus == 0 {
        return;
    }

    // Translate to window-local coordinates
    let local_x = x - mgr.window_offset_x;
    let local_y = y - mgr.window_offset_y;

    if let Some(idx) = mgr.find_or_create_queue(focus) {
        mgr.queues[idx].push(InputEvent::pointer_motion(local_x, local_y, timestamp_ms));
    }
}

/// Route a pointer button event to the focused task
pub fn input_route_pointer_button(button: u8, pressed: bool, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();

    // Update button state
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
        mgr.queues[idx].push(InputEvent::pointer_button(pressed, button, timestamp_ms));
    }
}

// =============================================================================
// Public API - Client Operations (Syscalls)
// =============================================================================

/// Poll for an input event (non-blocking)
/// Returns the event if available, None if queue is empty
pub fn input_poll(task_id: u32) -> Option<InputEvent> {
    // Use irqsave/irqrestore to prevent deadlock if keyboard/mouse IRQ fires while holding lock
    let flags = cpu::save_flags_cli();
    let mut mgr = INPUT_MANAGER.lock();
    let result = if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].pop()
    } else {
        None
    };
    drop(mgr);
    cpu::restore_flags(flags);
    result
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

    // Use irqsave/irqrestore to prevent deadlock if keyboard/mouse IRQ fires while holding lock
    let flags = cpu::save_flags_cli();
    let mut mgr = INPUT_MANAGER.lock();
    let idx = match mgr.find_queue(task_id) {
        Some(i) => i,
        None => {
            drop(mgr);
            cpu::restore_flags(flags);
            return 0;
        }
    };

    let mut count = 0;
    while count < max_count {
        if let Some(event) = mgr.queues[idx].pop() {
            unsafe { out_buffer.add(count).write(event); }
            count += 1;
        } else {
            break;
        }
    }
    drop(mgr);
    cpu::restore_flags(flags);
    count
}

/// Peek at the next input event without removing it
pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    let flags = cpu::save_flags_cli();
    let mgr = INPUT_MANAGER.lock();
    let result = if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].peek().copied()
    } else {
        None
    };
    drop(mgr);
    cpu::restore_flags(flags);
    result
}

/// Check if a task has pending input events
pub fn input_has_events(task_id: u32) -> bool {
    let flags = cpu::save_flags_cli();
    let mgr = INPUT_MANAGER.lock();
    let result = if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].count > 0
    } else {
        false
    };
    drop(mgr);
    cpu::restore_flags(flags);
    result
}

/// Get the number of pending events for a task
pub fn input_event_count(task_id: u32) -> u32 {
    let flags = cpu::save_flags_cli();
    let mgr = INPUT_MANAGER.lock();
    let result = if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].count as u32
    } else {
        0
    };
    drop(mgr);
    cpu::restore_flags(flags);
    result
}

// =============================================================================
// Task Cleanup
// =============================================================================

/// Clean up input queue for a terminated task
pub fn input_cleanup_task(task_id: u32) {
    let flags = cpu::save_flags_cli();
    let mut mgr = INPUT_MANAGER.lock();

    // Clear focus if this task had it
    if mgr.keyboard_focus == task_id {
        mgr.keyboard_focus = 0;
    }
    if mgr.pointer_focus == task_id {
        mgr.pointer_focus = 0;
    }

    // Deactivate the queue
    if let Some(idx) = mgr.find_queue(task_id) {
        mgr.queues[idx].active = false;
        mgr.queues[idx].task_id = 0;
        mgr.queues[idx].clear();
    }
    drop(mgr);
    cpu::restore_flags(flags);
}
