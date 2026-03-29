use super::event::{Key, Modifiers, NamedKey, WidgetEvent};
use crate::appkit::event::Event;

/// US-QWERTY keymap: maps scancodes to Keys.
pub struct Keymap {
    base: [Key; 128],
    shift: [Key; 128],
}

impl Keymap {
    pub fn us_qwerty() -> Self {
        let mut base = [Key::Unknown; 128];
        let mut shift = [Key::Unknown; 128];

        // Row 1: number row
        base[0x02] = Key::Char('1');
        shift[0x02] = Key::Char('!');
        base[0x03] = Key::Char('2');
        shift[0x03] = Key::Char('@');
        base[0x04] = Key::Char('3');
        shift[0x04] = Key::Char('#');
        base[0x05] = Key::Char('4');
        shift[0x05] = Key::Char('$');
        base[0x06] = Key::Char('5');
        shift[0x06] = Key::Char('%');
        base[0x07] = Key::Char('6');
        shift[0x07] = Key::Char('^');
        base[0x08] = Key::Char('7');
        shift[0x08] = Key::Char('&');
        base[0x09] = Key::Char('8');
        shift[0x09] = Key::Char('*');
        base[0x0A] = Key::Char('9');
        shift[0x0A] = Key::Char('(');
        base[0x0B] = Key::Char('0');
        shift[0x0B] = Key::Char(')');
        base[0x0C] = Key::Char('-');
        shift[0x0C] = Key::Char('_');
        base[0x0D] = Key::Char('=');
        shift[0x0D] = Key::Char('+');
        base[0x29] = Key::Char('`');
        shift[0x29] = Key::Char('~');

        // Row 2: QWERTY
        base[0x10] = Key::Char('q');
        shift[0x10] = Key::Char('Q');
        base[0x11] = Key::Char('w');
        shift[0x11] = Key::Char('W');
        base[0x12] = Key::Char('e');
        shift[0x12] = Key::Char('E');
        base[0x13] = Key::Char('r');
        shift[0x13] = Key::Char('R');
        base[0x14] = Key::Char('t');
        shift[0x14] = Key::Char('T');
        base[0x15] = Key::Char('y');
        shift[0x15] = Key::Char('Y');
        base[0x16] = Key::Char('u');
        shift[0x16] = Key::Char('U');
        base[0x17] = Key::Char('i');
        shift[0x17] = Key::Char('I');
        base[0x18] = Key::Char('o');
        shift[0x18] = Key::Char('O');
        base[0x19] = Key::Char('p');
        shift[0x19] = Key::Char('P');
        base[0x1A] = Key::Char('[');
        shift[0x1A] = Key::Char('{');
        base[0x1B] = Key::Char(']');
        shift[0x1B] = Key::Char('}');
        base[0x2B] = Key::Char('\\');
        shift[0x2B] = Key::Char('|');

        // Row 3: ASDF
        base[0x1E] = Key::Char('a');
        shift[0x1E] = Key::Char('A');
        base[0x1F] = Key::Char('s');
        shift[0x1F] = Key::Char('S');
        base[0x20] = Key::Char('d');
        shift[0x20] = Key::Char('D');
        base[0x21] = Key::Char('f');
        shift[0x21] = Key::Char('F');
        base[0x22] = Key::Char('g');
        shift[0x22] = Key::Char('G');
        base[0x23] = Key::Char('h');
        shift[0x23] = Key::Char('H');
        base[0x24] = Key::Char('j');
        shift[0x24] = Key::Char('J');
        base[0x25] = Key::Char('k');
        shift[0x25] = Key::Char('K');
        base[0x26] = Key::Char('l');
        shift[0x26] = Key::Char('L');
        base[0x27] = Key::Char(';');
        shift[0x27] = Key::Char(':');
        base[0x28] = Key::Char('\'');
        shift[0x28] = Key::Char('"');

        // Row 4: ZXCV
        base[0x2C] = Key::Char('z');
        shift[0x2C] = Key::Char('Z');
        base[0x2D] = Key::Char('x');
        shift[0x2D] = Key::Char('X');
        base[0x2E] = Key::Char('c');
        shift[0x2E] = Key::Char('C');
        base[0x2F] = Key::Char('v');
        shift[0x2F] = Key::Char('V');
        base[0x30] = Key::Char('b');
        shift[0x30] = Key::Char('B');
        base[0x31] = Key::Char('n');
        shift[0x31] = Key::Char('N');
        base[0x32] = Key::Char('m');
        shift[0x32] = Key::Char('M');
        base[0x33] = Key::Char(',');
        shift[0x33] = Key::Char('<');
        base[0x34] = Key::Char('.');
        shift[0x34] = Key::Char('>');
        base[0x35] = Key::Char('/');
        shift[0x35] = Key::Char('?');

        // Special keys
        base[0x01] = Key::Named(NamedKey::Escape);
        shift[0x01] = Key::Named(NamedKey::Escape);
        base[0x0E] = Key::Named(NamedKey::Backspace);
        shift[0x0E] = Key::Named(NamedKey::Backspace);
        base[0x0F] = Key::Named(NamedKey::Tab);
        shift[0x0F] = Key::Named(NamedKey::Tab);
        base[0x1C] = Key::Named(NamedKey::Enter);
        shift[0x1C] = Key::Named(NamedKey::Enter);
        base[0x39] = Key::Named(NamedKey::Space);
        shift[0x39] = Key::Named(NamedKey::Space);

        // Arrow keys (extended scancodes — typically 0x48/0x4B/0x4D/0x50)
        base[0x48] = Key::Named(NamedKey::Up);
        shift[0x48] = Key::Named(NamedKey::Up);
        base[0x4B] = Key::Named(NamedKey::Left);
        shift[0x4B] = Key::Named(NamedKey::Left);
        base[0x4D] = Key::Named(NamedKey::Right);
        shift[0x4D] = Key::Named(NamedKey::Right);
        base[0x50] = Key::Named(NamedKey::Down);
        shift[0x50] = Key::Named(NamedKey::Down);

        // Home/End/PgUp/PgDn/Delete
        base[0x47] = Key::Named(NamedKey::Home);
        shift[0x47] = Key::Named(NamedKey::Home);
        base[0x4F] = Key::Named(NamedKey::End);
        shift[0x4F] = Key::Named(NamedKey::End);
        base[0x49] = Key::Named(NamedKey::PageUp);
        shift[0x49] = Key::Named(NamedKey::PageUp);
        base[0x51] = Key::Named(NamedKey::PageDown);
        shift[0x51] = Key::Named(NamedKey::PageDown);
        base[0x53] = Key::Named(NamedKey::Delete);
        shift[0x53] = Key::Named(NamedKey::Delete);

        // Function keys
        base[0x3B] = Key::Named(NamedKey::F1);
        base[0x3C] = Key::Named(NamedKey::F2);
        base[0x3D] = Key::Named(NamedKey::F3);
        base[0x3E] = Key::Named(NamedKey::F4);
        base[0x3F] = Key::Named(NamedKey::F5);
        base[0x40] = Key::Named(NamedKey::F6);
        base[0x41] = Key::Named(NamedKey::F7);
        base[0x42] = Key::Named(NamedKey::F8);
        base[0x43] = Key::Named(NamedKey::F9);
        base[0x44] = Key::Named(NamedKey::F10);
        base[0x57] = Key::Named(NamedKey::F11);
        base[0x58] = Key::Named(NamedKey::F12);

        Self { base, shift }
    }

    /// Translate a scancode + modifiers to a Key.
    pub fn translate(&self, scancode: u8, modifiers: &Modifiers) -> Key {
        let idx = scancode as usize;
        if idx >= 128 {
            return Key::Unknown;
        }
        let mut key = if modifiers.shift {
            self.shift[idx]
        } else {
            self.base[idx]
        };
        // Apply caps lock to letter characters.
        if modifiers.caps_lock {
            if let Key::Char(c) = key {
                if c.is_ascii_alphabetic() {
                    key = if modifiers.shift {
                        Key::Char(c.to_ascii_lowercase())
                    } else {
                        Key::Char(c.to_ascii_uppercase())
                    };
                }
            }
        }
        key
    }
}

/// Key repeat state machine.
pub struct KeyRepeatState {
    key: Option<Key>,
    press_time_ms: u64,
    last_repeat_ms: u64,
    repeating: bool,
}

const REPEAT_DELAY_MS: u64 = 500;
const REPEAT_INTERVAL_MS: u64 = 33;

impl KeyRepeatState {
    pub fn new() -> Self {
        Self {
            key: None,
            press_time_ms: 0,
            last_repeat_ms: 0,
            repeating: false,
        }
    }

    /// Call on key down. Returns true if this is a new key press (not a repeat).
    pub fn on_key_down(&mut self, key: Key, now_ms: u64) -> bool {
        if self.key == Some(key) {
            return false; // Already tracking this key.
        }
        self.key = Some(key);
        self.press_time_ms = now_ms;
        self.last_repeat_ms = now_ms;
        self.repeating = false;
        true
    }

    /// Call on key up.
    pub fn on_key_up(&mut self, key: Key) {
        if self.key == Some(key) {
            self.key = None;
            self.repeating = false;
        }
    }

    /// Tick each frame. Returns Some(key) if a repeat event should fire.
    pub fn tick(&mut self, now_ms: u64) -> Option<Key> {
        let key = self.key?;
        if !Self::key_repeats(&key) {
            return None;
        }
        if !self.repeating {
            if now_ms - self.press_time_ms >= REPEAT_DELAY_MS {
                self.repeating = true;
                self.last_repeat_ms = now_ms;
                return Some(key);
            }
        } else if now_ms - self.last_repeat_ms >= REPEAT_INTERVAL_MS {
            self.last_repeat_ms = now_ms;
            return Some(key);
        }
        None
    }

    fn key_repeats(key: &Key) -> bool {
        match key {
            Key::Char(_) => true,
            Key::Named(n) => matches!(
                n,
                NamedKey::Backspace
                    | NamedKey::Delete
                    | NamedKey::Left
                    | NamedKey::Right
                    | NamedKey::Up
                    | NamedKey::Down
                    | NamedKey::Tab
                    | NamedKey::Space
                    | NamedKey::PageUp
                    | NamedKey::PageDown
                    | NamedKey::Home
                    | NamedKey::End
            ),
            Key::Unknown => false,
        }
    }
}

impl Default for KeyRepeatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert an appkit Event into a WidgetEvent, using the keymap for key events.
pub fn translate_event(
    event: &Event,
    keymap: &Keymap,
    modifiers: &Modifiers,
) -> Option<WidgetEvent> {
    match event {
        Event::PointerMotion { x, y } => Some(WidgetEvent::PointerMove { x: *x, y: *y }),
        Event::PointerPress { button } => {
            let btn = match button {
                0 | 1 => super::event::PointerButton::Left,
                2 => super::event::PointerButton::Right,
                3 => super::event::PointerButton::Middle,
                _ => super::event::PointerButton::Left,
            };
            // Position will be filled in by the framework from tracked pointer state.
            Some(WidgetEvent::PointerDown {
                x: 0,
                y: 0,
                button: btn,
            })
        }
        Event::PointerRelease { button } => {
            let btn = match button {
                0 | 1 => super::event::PointerButton::Left,
                2 => super::event::PointerButton::Right,
                3 => super::event::PointerButton::Middle,
                _ => super::event::PointerButton::Left,
            };
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
        Event::KeyPress { scancode, .. } => {
            let key = keymap.translate(*scancode, modifiers);
            match key {
                Key::Char(c) if !modifiers.ctrl && !modifiers.alt => {
                    // Emit both KeyDown and TextInput.
                    // The framework will handle dispatching both.
                    Some(WidgetEvent::TextInput { character: c })
                }
                _ => Some(WidgetEvent::KeyDown {
                    key,
                    modifiers: *modifiers,
                    repeat: false,
                }),
            }
        }
        Event::KeyRelease { scancode, .. } => {
            let key = keymap.translate(*scancode, modifiers);
            Some(WidgetEvent::KeyUp {
                key,
                modifiers: *modifiers,
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
