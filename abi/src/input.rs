//! Input event types (Wayland-style per-task input queues)

/// Maximum number of tasks that can have input queues
pub const MAX_INPUT_TASKS: usize = 32;

/// Maximum events per task queue
pub const MAX_EVENTS_PER_TASK: usize = 64;

/// Focus type for input_set_focus syscall
pub const INPUT_FOCUS_KEYBOARD: u32 = 0;
pub const INPUT_FOCUS_POINTER: u32 = 1;

/// Type of input event
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputEventType {
    /// Key pressed
    #[default]
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
    /// Convert from raw u8 value
    #[inline]
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

    /// Returns true if this is a key event (press or release)
    #[inline]
    pub fn is_key_event(self) -> bool {
        matches!(self, Self::KeyPress | Self::KeyRelease)
    }

    /// Returns true if this is a pointer event
    #[inline]
    pub fn is_pointer_event(self) -> bool {
        !self.is_key_event()
    }
}

/// Input event data (union-like structure)
///
/// For key events: data0 contains scancode in low 16 bits, ASCII in high 16 bits
/// For pointer motion: data0 is x coordinate, data1 is y coordinate
/// For pointer button: data0 contains button code
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct InputEventData {
    pub data0: u32,
    pub data1: u32,
}

/// A complete input event
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    /// Type of event
    pub event_type: InputEventType,
    /// Padding for alignment
    pub _padding: [u8; 3],
    /// Timestamp in milliseconds since boot
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
    #[inline]
    pub fn key_scancode(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }

    /// Extract ASCII from key event
    #[inline]
    pub fn key_ascii(&self) -> u8 {
        ((self.data.data0 >> 16) & 0xFF) as u8
    }

    /// Extract X coordinate from pointer event
    #[inline]
    pub fn pointer_x(&self) -> i32 {
        self.data.data0 as i32
    }

    /// Extract Y coordinate from pointer event
    #[inline]
    pub fn pointer_y(&self) -> i32 {
        self.data.data1 as i32
    }

    /// Extract button from pointer button event
    #[inline]
    pub fn pointer_button_code(&self) -> u8 {
        (self.data.data0 & 0xFF) as u8
    }
}
