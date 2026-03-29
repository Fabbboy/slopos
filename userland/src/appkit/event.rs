//! High-level event types for windowed applications.
//!
//! Converts raw `InputEvent` values from the kernel into a clean enum
//! that applications can match on without knowing ABI details.

use crate::syscall::{InputEvent, InputEventType};

#[derive(Clone, Copy, Debug)]
pub enum Event {
    PointerMotion { x: i32, y: i32 },
    PointerPress { button: u8 },
    PointerRelease { button: u8 },
    KeyPress { scancode: u8, ascii: u8 },
    KeyRelease { scancode: u8, ascii: u8 },
    CloseRequest,
    Configure { width: u32, height: u32 },
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
            _ => Event::Other,
        }
    }
}
