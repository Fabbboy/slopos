//! Userland glue over the host-testable `slopos-terminal-core` input model.
//!
//! The pure logic (key encoding, selection, paste sanitizing, the
//! `CompositorEvent` taxonomy) lives in the core crate. The only piece that
//! cannot move there is `classify`, which translates the compositor's wire
//! `Event` into a `CompositorEvent` — it is the sole protocol-coupled function.

pub use slopos_terminal_core::input::*;

use slopos_protocol::types::Event as ProtocolEvent;

/// Translate a raw protocol event into a terminal-facing `CompositorEvent`.
pub fn classify(evt: &ProtocolEvent) -> CompositorEvent {
    match evt {
        ProtocolEvent::Key {
            scancode,
            ascii,
            pressed,
            ..
        } => {
            if *pressed {
                CompositorEvent::Key(*ascii as u8, *scancode as u8)
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
        ProtocolEvent::Close { .. } => CompositorEvent::Close,
        ProtocolEvent::PasteReady { len } => CompositorEvent::PasteReady(*len),
        ProtocolEvent::PasteResult { len } => CompositorEvent::PasteResult(*len),
        _ => CompositorEvent::Ignored,
    }
}
