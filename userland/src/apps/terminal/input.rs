//! Userland glue over the host-testable `slopos-terminal-core` input model.
//!
//! `classify` is the sole protocol-coupled function; everything else lives in
//! the core crate.

pub use slopos_terminal_core::input::*;

use slopos_abi::input::POINTER_AXIS_VERTICAL;
use slopos_protocol::types::Event as ProtocolEvent;

pub fn classify(evt: &ProtocolEvent) -> CompositorEvent {
    match evt {
        ProtocolEvent::Key {
            scancode,
            ascii,
            codepoint,
            pressed,
            ..
        } => {
            if *pressed {
                CompositorEvent::Key(*ascii as u8, *scancode as u8, *codepoint)
            } else {
                CompositorEvent::Ignored
            }
        }
        ProtocolEvent::Modifiers { mods } => CompositorEvent::Modifiers(*mods as u8),
        ProtocolEvent::PointerMotion { x, y, .. } => CompositorEvent::PointerMotion(*x, *y),
        ProtocolEvent::PointerEnter { x, y, .. } => CompositorEvent::PointerEnter(*x, *y),
        ProtocolEvent::PointerLeave { .. } => CompositorEvent::PointerLeave,
        ProtocolEvent::PointerButton {
            button, pressed, ..
        } => CompositorEvent::PointerButton {
            pressed: *pressed,
            code: *button as u8,
        },
        ProtocolEvent::Configure { width, height, .. } => {
            CompositorEvent::Resize(*width as i32, *height as i32)
        }
        ProtocolEvent::PointerAxis { axis, value, .. } => {
            // Horizontal scroll has no meaning for the terminal grid.
            if *axis == POINTER_AXIS_VERTICAL {
                CompositorEvent::Scroll(*value)
            } else {
                CompositorEvent::Ignored
            }
        }
        ProtocolEvent::Close { .. } => CompositorEvent::Close,
        ProtocolEvent::PasteReady { len } => CompositorEvent::PasteReady(*len),
        ProtocolEvent::PasteResult { len } => CompositorEvent::PasteResult(*len),
        _ => CompositorEvent::Ignored,
    }
}
