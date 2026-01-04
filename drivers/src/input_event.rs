//! Input Event Protocol (Wayland-like per-task input queues)
//!
//! This module implements a Wayland-inspired input event system:
//! - Per-task event queues
//! - Keyboard and pointer focus tracking
//! - Structured input events with timestamps
//!
//! Events are routed to the focused task for each input type.

use spin::Mutex;

// =============================================================================
// Input Event Types
// =============================================================================

/// Maximum number of tasks that can have input queues
const MAX_INPUT_TASKS: usize = 32;

/// Maximum events per task queue
const MAX_EVENTS_PER_TASK: usize = 64;

/// Type of input event
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
}

/// Input event data (union-like structure)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEventData {
    /// For key events: scancode in low 16 bits, ASCII in high 16 bits
    /// For pointer motion: x in low 32 bits, y in high 32 bits (packed as i16)
    /// For pointer button: button code
    pub data0: u32,
    pub data1: u32,
}

impl Default for InputEventData {
    fn default() -> Self {
        Self { data0: 0, data1: 0 }
    }
}

/// A complete input event
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
    /// Create a key event
    pub fn key(event_type: InputEventType, scancode: u8, ascii: u8, timestamp_ms: u64) -> Self {
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

    /// Create a pointer motion event
    pub fn pointer_motion(x: i32, y: i32, timestamp_ms: u64) -> Self {
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

    /// Create a pointer button event
    pub fn pointer_button(pressed: bool, button: u8, timestamp_ms: u64) -> Self {
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

    /// Create a pointer enter/leave event
    pub fn pointer_enter_leave(enter: bool, x: i32, y: i32, timestamp_ms: u64) -> Self {
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

    /// Extract scancode from key event
    pub fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// Extract ASCII from key event
    pub fn key_ascii(&self) -> u8 {
        ((self.data.data0 >> 16) & 0xFF) as u8
    }

    /// Extract X coordinate from pointer event
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    /// Extract Y coordinate from pointer event
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    /// Extract button from pointer button event
    pub fn pointer_button_code(&self) -> u8 {
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
    /// Current pointer position
    pointer_x: i32,
    pointer_y: i32,
    /// Current pointer button state
    pointer_buttons: u8,
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
    let mut mgr = INPUT_MANAGER.lock();
    mgr.keyboard_focus = task_id;
}

/// Set pointer focus to a task (called by compositor)
/// Also sends enter/leave events
pub fn input_set_pointer_focus(task_id: u32, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();
    let old_focus = mgr.pointer_focus;
    let x = mgr.pointer_x;
    let y = mgr.pointer_y;

    if old_focus == task_id {
        return;
    }

    // Send leave event to old focus
    if old_focus != 0 {
        if let Some(idx) = mgr.find_queue(old_focus) {
            mgr.queues[idx].push(InputEvent::pointer_enter_leave(false, x, y, timestamp_ms));
        }
    }

    mgr.pointer_focus = task_id;

    // Send enter event to new focus
    if task_id != 0 {
        if let Some(idx) = mgr.find_or_create_queue(task_id) {
            mgr.queues[idx].push(InputEvent::pointer_enter_leave(true, x, y, timestamp_ms));
        }
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
pub fn input_route_pointer_motion(x: i32, y: i32, timestamp_ms: u64) {
    let mut mgr = INPUT_MANAGER.lock();
    mgr.pointer_x = x;
    mgr.pointer_y = y;

    let focus = mgr.pointer_focus;
    if focus == 0 {
        return;
    }

    if let Some(idx) = mgr.find_or_create_queue(focus) {
        mgr.queues[idx].push(InputEvent::pointer_motion(x, y, timestamp_ms));
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
    let mut mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        return mgr.queues[idx].pop();
    }
    None
}

/// Peek at the next input event without removing it
pub fn input_peek(task_id: u32) -> Option<InputEvent> {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        return mgr.queues[idx].peek().copied();
    }
    None
}

/// Check if a task has pending input events
pub fn input_has_events(task_id: u32) -> bool {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        return mgr.queues[idx].count > 0;
    }
    false
}

/// Get the number of pending events for a task
pub fn input_event_count(task_id: u32) -> u32 {
    let mgr = INPUT_MANAGER.lock();
    if let Some(idx) = mgr.find_queue(task_id) {
        return mgr.queues[idx].count as u32;
    }
    0
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
}
