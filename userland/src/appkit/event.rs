//! High-level event types for windowed applications.
//!
//! Converts raw `InputEvent` values from the kernel into a clean enum
//! that applications can match on without knowing ABI details.

use crate::syscall::{InputEvent, InputEventType};

#[derive(Clone, Copy, Debug)]
pub enum Event {
    PointerMotion {
        x: i32,
        y: i32,
    },
    PointerPress {
        button: u8,
    },
    PointerRelease {
        button: u8,
    },
    /// Pointer axis (scroll) event.
    /// `axis`: 0 = vertical, 1 = horizontal (see `POINTER_AXIS_*` in ABI).
    /// `value_v120`: scroll amount in value120 units (+-120 = one wheel click).
    PointerAxis {
        axis: u32,
        value_v120: i32,
    },
    KeyPress {
        scancode: u8,
        ascii: u8,
    },
    KeyRelease {
        scancode: u8,
        ascii: u8,
    },
    CloseRequest,
    Configure {
        width: u32,
        height: u32,
    },
    Other,
}

impl Event {
    pub fn from_raw(raw: &InputEvent) -> Self {
        match raw.event_type {
            InputEventType::PointerMotion | InputEventType::PointerEnter => Event::PointerMotion {
                x: raw.pointer_x(),
                y: raw.pointer_y(),
            },
            InputEventType::PointerButtonPress => Event::PointerPress {
                button: raw.pointer_button_code(),
            },
            InputEventType::PointerButtonRelease => Event::PointerRelease {
                button: raw.pointer_button_code(),
            },
            InputEventType::KeyPress => Event::KeyPress {
                scancode: raw.key_scancode(),
                ascii: raw.key_ascii(),
            },
            InputEventType::KeyRelease => Event::KeyRelease {
                scancode: raw.key_scancode(),
                ascii: raw.key_ascii(),
            },
            InputEventType::CloseRequest => Event::CloseRequest,
            InputEventType::Configure => Event::Configure {
                width: raw.configure_width(),
                height: raw.configure_height(),
            },
            InputEventType::PointerAxis => Event::PointerAxis {
                axis: raw.axis_id(),
                value_v120: raw.axis_value_v120(),
            },
            _ => Event::Other,
        }
    }
}
