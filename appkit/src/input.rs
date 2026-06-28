//! Input translation: windowing events → widget events.
//!
//! Key classification is delegated to the shared `keymap-core` layout, driven by
//! the kernel-provided canonical keycode + modifier snapshot that each key event
//! carries. The keyboard layout lives in one place (`keymap-core`, also used by
//! the kernel), so a layout change flows everywhere automatically.

use super::event::{Key, Modifiers, WidgetEvent};
use slopos_keymap_core::{UiKey, mods_locks_from_raw, ui_classify};
use slopos_windowing::Event;

/// Map a shared `keymap-core` classification to an appkit [`Key`]. Modifier and
/// otherwise-unmapped keys become [`Key::Unknown`] (so a press/release of e.g.
/// Shift still drives focus modality, as before).
fn ui_key_to_key(u: UiKey) -> Key {
    match u {
        UiKey::Char(c) => Key::Char(c),
        UiKey::Named(nk) => Key::Named(nk),
        UiKey::None => Key::Unknown,
    }
}

/// Classify a windowing key event's keycode under its modifier snapshot.
fn classify_key(keycode: u16, modifiers: u8) -> Key {
    let (mods, locks) = mods_locks_from_raw(modifiers);
    ui_key_to_key(ui_classify(keycode, mods, locks))
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
            keycode, modifiers, ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *modifiers);
            match key {
                // Printable text with no Ctrl/Alt is text input; everything else
                // (named keys, shortcuts, modifier keys) is a key-down.
                Key::Char(c) if !mods.ctrl && !mods.alt => {
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
            keycode, modifiers, ..
        } => {
            let mods = Modifiers::from_raw(*modifiers);
            let key = classify_key(*keycode, *modifiers);
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
