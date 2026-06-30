//! Input translation: windowing events → widget events.
//!
//! Text characters come from the **kernel-resolved codepoint** the event already
//! carries: the kernel is the single keyboard-layout authority (it applies the
//! active runtime layout, AltGr levels, and dead-key composition before the
//! event crosses to userland), so switching the layout with `keymap <name>`
//! flows into every GUI app for free — no per-app layout engine. Named action
//! keys and Ctrl-shortcut letters are layout-independent and come from the
//! shared `keymap-core` classifier driven by the canonical keycode.

use super::event::{Key, Modifiers, WidgetEvent};
use slopos_keymap_core::{UiKey, mods_locks_from_raw, ui_classify};
use slopos_windowing::Event;

/// Classify a windowing key event into an appkit [`Key`].
///
/// - Named action keys (arrows, F-keys, keypad navigation, Enter/Tab/Esc/Space/
///   Backspace) are layout-independent → from the keycode classifier.
/// - Printable text uses the kernel-resolved `codepoint` (honors the active
///   layout incl. AltGr + dead keys). A freshly-pressed dead key carries
///   `codepoint == 0` and classifies as [`Key::Unknown`] (no text yet).
/// - When Ctrl makes the codepoint a control code, the Ctrl-independent letter
///   is recovered from the classifier so shortcuts (Ctrl+C, …) still match.
fn classify_key(keycode: u16, codepoint: u32, modifiers: u8) -> Key {
    let (mods, locks) = mods_locks_from_raw(modifiers);

    if let UiKey::Named(nk) = ui_classify(keycode, mods, locks) {
        return Key::Named(nk);
    }
    if let Some(c) = char::from_u32(codepoint) {
        if !c.is_control() {
            return Key::Char(c);
        }
    }
    if mods.ctrl {
        if let UiKey::Char(c) = ui_classify(keycode, mods, locks) {
            return Key::Char(c);
        }
    }
    Key::Unknown
}

/// Convert a windowing [`Event`] into a [`WidgetEvent`].
pub fn translate_event(event: &Event) -> Option<WidgetEvent> {
    match event {
        Event::PointerMotion { x, y } => Some(WidgetEvent::PointerMove { x: *x, y: *y }),
        Event::PointerPress { button } => {
            let btn = pointer_button(*button);
            // Position will be filled in by the framework from tracked pointer state.
            Some(WidgetEvent::PointerDown {
                x: 0,
                y: 0,
                button: btn,
            })
        }
        Event::PointerRelease { button } => {
            let btn = pointer_button(*button);
            Some(WidgetEvent::PointerUp {
                x: 0,
                y: 0,
                button: btn,
            })
        }
        Event::PointerAxis { value_v120, .. } => {
            let delta_lines = *value_v120 / 120;
            let delta_px = delta_lines * 20; // ~line_height
            Some(WidgetEvent::Scroll {
                delta_x: 0,
                delta_y: delta_px,
            })
        }
        Event::KeyPress {
            keycode,
            codepoint,
            modifiers,
            ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *codepoint, *modifiers);
            match key {
                // Printable text with no Ctrl and no *plain* Alt is text input
                // (AltGr-composed characters are text); everything else (named
                // keys, shortcuts, modifier keys) is a key-down.
                Key::Char(c) if !mods.ctrl && !mods.plain_alt() => {
                    Some(WidgetEvent::TextInput { character: c })
                }
                _ => Some(WidgetEvent::KeyDown {
                    key,
                    modifiers: mods,
                    repeat: false,
                }),
            }
        }
        Event::KeyRelease {
            keycode,
            codepoint,
            modifiers,
            ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *codepoint, *modifiers);
            Some(WidgetEvent::KeyUp {
                key,
                modifiers: mods,
            })
        }
        Event::Configure { width, height } => Some(WidgetEvent::Configure {
            width: *width,
            height: *height,
        }),
        Event::CloseRequest => None, // Handled at the appkit level.
        Event::Other => None,
    }
}

fn pointer_button(button: u8) -> super::event::PointerButton {
    match button {
        0 | 1 => super::event::PointerButton::Left,
        2 => super::event::PointerButton::Right,
        3 => super::event::PointerButton::Middle,
        _ => super::event::PointerButton::Left,
    }
}
