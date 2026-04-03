//! High-level event types for windowed applications.
//!
//! Converts `slopos_protocol::Event` values from the compositor socket into a
//! clean enum that applications can match on without knowing protocol details.

use slopos_protocol::types::Event as ProtocolEvent;

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
    /// Convert a protocol event into a windowing Event.
    ///
    /// Returns `None` for protocol events that have no windowing equivalent
    /// (e.g. `FrameDone`, `OutputInfo`).
    pub fn from_protocol(evt: &ProtocolEvent) -> Option<Self> {
        match evt {
            ProtocolEvent::PointerEnter { x, y, .. }
            | ProtocolEvent::PointerMotion { x, y, .. } => {
                Some(Event::PointerMotion { x: *x, y: *y })
            }
            ProtocolEvent::PointerButton {
                button, pressed, ..
            } => {
                if *pressed {
                    Some(Event::PointerPress {
                        button: *button as u8,
                    })
                } else {
                    Some(Event::PointerRelease {
                        button: *button as u8,
                    })
                }
            }
            ProtocolEvent::PointerAxis { axis, value, .. } => Some(Event::PointerAxis {
                axis: *axis,
                value_v120: *value,
            }),
            ProtocolEvent::Key {
                scancode,
                ascii,
                pressed,
                ..
            } => {
                let sc = u8::try_from(*scancode).ok()?;
                let a = u8::try_from(*ascii).ok()?;
                if *pressed {
                    Some(Event::KeyPress {
                        scancode: sc,
                        ascii: a,
                    })
                } else {
                    Some(Event::KeyRelease {
                        scancode: sc,
                        ascii: a,
                    })
                }
            }
            ProtocolEvent::Close { .. } => Some(Event::CloseRequest),
            ProtocolEvent::Configure { width, height, .. } => Some(Event::Configure {
                width: *width,
                height: *height,
            }),
            _ => None,
        }
    }
}
