//! Canonical keyboard keycodes — USB HID Keyboard/Keypad usage IDs (page 0x07).
//!
//! This is SlopOS's single, layout-independent keycode space. Every keyboard
//! backend (i8042 PS/2 today; USB-HID / I²C-HID later) decodes its raw protocol
//! into these usages, and a single keymap layer turns `(usage, modifiers, locks)`
//! into text codepoints or [`NamedKey`]s. The kernel already speaks HID
//! (`drivers::touchpad`), so this unifies all input under one vocabulary.
//!
//! Values are the standard USB HID Usage Table v1.12, "Keyboard/Keypad Page
//! (0x07)" usage IDs. They are carried on the wire as the low 16 bits of an
//! [`super::InputEvent`]'s `data1`.
//!
//! These constants are intentionally **not** glob-re-exported at the crate root
//! (`slopos_abi`) — reach them as `slopos_abi::input::keycode::KEY_A` so the ~100
//! short names never collide with other ABI symbols.

#![allow(dead_code)]

// --- Letters (0x04..=0x1D) ---------------------------------------------------
pub const KEY_A: u16 = 0x04;
pub const KEY_B: u16 = 0x05;
pub const KEY_C: u16 = 0x06;
pub const KEY_D: u16 = 0x07;
pub const KEY_E: u16 = 0x08;
pub const KEY_F: u16 = 0x09;
pub const KEY_G: u16 = 0x0A;
pub const KEY_H: u16 = 0x0B;
pub const KEY_I: u16 = 0x0C;
pub const KEY_J: u16 = 0x0D;
pub const KEY_K: u16 = 0x0E;
pub const KEY_L: u16 = 0x0F;
pub const KEY_M: u16 = 0x10;
pub const KEY_N: u16 = 0x11;
pub const KEY_O: u16 = 0x12;
pub const KEY_P: u16 = 0x13;
pub const KEY_Q: u16 = 0x14;
pub const KEY_R: u16 = 0x15;
pub const KEY_S: u16 = 0x16;
pub const KEY_T: u16 = 0x17;
pub const KEY_U: u16 = 0x18;
pub const KEY_V: u16 = 0x19;
pub const KEY_W: u16 = 0x1A;
pub const KEY_X: u16 = 0x1B;
pub const KEY_Y: u16 = 0x1C;
pub const KEY_Z: u16 = 0x1D;

// --- Number row (0x1E..=0x27): 1,2,3,4,5,6,7,8,9,0 ---------------------------
pub const KEY_1: u16 = 0x1E;
pub const KEY_2: u16 = 0x1F;
pub const KEY_3: u16 = 0x20;
pub const KEY_4: u16 = 0x21;
pub const KEY_5: u16 = 0x22;
pub const KEY_6: u16 = 0x23;
pub const KEY_7: u16 = 0x24;
pub const KEY_8: u16 = 0x25;
pub const KEY_9: u16 = 0x26;
pub const KEY_0: u16 = 0x27;

// --- Main control / punctuation ---------------------------------------------
pub const KEY_ENTER: u16 = 0x28;
pub const KEY_ESC: u16 = 0x29;
pub const KEY_BACKSPACE: u16 = 0x2A;
pub const KEY_TAB: u16 = 0x2B;
pub const KEY_SPACE: u16 = 0x2C;
pub const KEY_MINUS: u16 = 0x2D;
pub const KEY_EQUAL: u16 = 0x2E;
pub const KEY_LEFTBRACE: u16 = 0x2F;
pub const KEY_RIGHTBRACE: u16 = 0x30;
pub const KEY_BACKSLASH: u16 = 0x31;
pub const KEY_NONUS_HASH: u16 = 0x32;
pub const KEY_SEMICOLON: u16 = 0x33;
pub const KEY_APOSTROPHE: u16 = 0x34;
pub const KEY_GRAVE: u16 = 0x35;
pub const KEY_COMMA: u16 = 0x36;
pub const KEY_DOT: u16 = 0x37;
pub const KEY_SLASH: u16 = 0x38;
pub const KEY_CAPSLOCK: u16 = 0x39;

// --- Function row (0x3A..=0x45) ---------------------------------------------
pub const KEY_F1: u16 = 0x3A;
pub const KEY_F2: u16 = 0x3B;
pub const KEY_F3: u16 = 0x3C;
pub const KEY_F4: u16 = 0x3D;
pub const KEY_F5: u16 = 0x3E;
pub const KEY_F6: u16 = 0x3F;
pub const KEY_F7: u16 = 0x40;
pub const KEY_F8: u16 = 0x41;
pub const KEY_F9: u16 = 0x42;
pub const KEY_F10: u16 = 0x43;
pub const KEY_F11: u16 = 0x44;
pub const KEY_F12: u16 = 0x45;

// --- Navigation / editing cluster -------------------------------------------
pub const KEY_PRINTSCREEN: u16 = 0x46;
pub const KEY_SCROLLLOCK: u16 = 0x47;
pub const KEY_PAUSE: u16 = 0x48;
pub const KEY_INSERT: u16 = 0x49;
pub const KEY_HOME: u16 = 0x4A;
pub const KEY_PAGEUP: u16 = 0x4B;
pub const KEY_DELETE: u16 = 0x4C;
pub const KEY_END: u16 = 0x4D;
pub const KEY_PAGEDOWN: u16 = 0x4E;
pub const KEY_RIGHT: u16 = 0x4F;
pub const KEY_LEFT: u16 = 0x50;
pub const KEY_DOWN: u16 = 0x51;
pub const KEY_UP: u16 = 0x52;

// --- Keypad (0x53..=0x63) — distinct from the navigation cluster above -------
pub const KEY_NUMLOCK: u16 = 0x53;
pub const KEY_KP_SLASH: u16 = 0x54;
pub const KEY_KP_ASTERISK: u16 = 0x55;
pub const KEY_KP_MINUS: u16 = 0x56;
pub const KEY_KP_PLUS: u16 = 0x57;
pub const KEY_KP_ENTER: u16 = 0x58;
pub const KEY_KP_1: u16 = 0x59;
pub const KEY_KP_2: u16 = 0x5A;
pub const KEY_KP_3: u16 = 0x5B;
pub const KEY_KP_4: u16 = 0x5C;
pub const KEY_KP_5: u16 = 0x5D;
pub const KEY_KP_6: u16 = 0x5E;
pub const KEY_KP_7: u16 = 0x5F;
pub const KEY_KP_8: u16 = 0x60;
pub const KEY_KP_9: u16 = 0x61;
pub const KEY_KP_0: u16 = 0x62;
pub const KEY_KP_DOT: u16 = 0x63;

// --- Misc -------------------------------------------------------------------
pub const KEY_NONUS_BACKSLASH: u16 = 0x64;
pub const KEY_APPLICATION: u16 = 0x65; // "menu" / context key
pub const KEY_MENU: u16 = 0x76;

// --- Modifier keys (0xE0..=0xE7) --------------------------------------------
pub const KEY_LEFTCTRL: u16 = 0xE0;
pub const KEY_LEFTSHIFT: u16 = 0xE1;
pub const KEY_LEFTALT: u16 = 0xE2;
pub const KEY_LEFTMETA: u16 = 0xE3; // Left GUI / Super / Logo
pub const KEY_RIGHTCTRL: u16 = 0xE4;
pub const KEY_RIGHTSHIFT: u16 = 0xE5;
pub const KEY_RIGHTALT: u16 = 0xE6;
pub const KEY_RIGHTMETA: u16 = 0xE7; // Right GUI / Super / Logo

/// A keyboard key that does not directly produce a text codepoint (or whose
/// non-text meaning the consumer wants explicitly). Emitted by the keymap layer
/// alongside text codepoints.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedKey {
    Escape,
    Tab,
    Enter,
    Backspace,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    CapsLock,
    NumLock,
    ScrollLock,
    PrintScreen,
    Pause,
    Menu,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl NamedKey {
    /// Map a function-row index (1..=12) to the matching `Fn` variant.
    pub fn function(n: u8) -> Option<Self> {
        Some(match n {
            1 => Self::F1,
            2 => Self::F2,
            3 => Self::F3,
            4 => Self::F4,
            5 => Self::F5,
            6 => Self::F6,
            7 => Self::F7,
            8 => Self::F8,
            9 => Self::F9,
            10 => Self::F10,
            11 => Self::F11,
            12 => Self::F12,
            _ => return None,
        })
    }
}

/// Returns `true` for the eight HID modifier usages (0xE0..=0xE7).
#[inline]
pub fn is_modifier(usage: u16) -> bool {
    (KEY_LEFTCTRL..=KEY_RIGHTMETA).contains(&usage)
}

/// Returns `true` for the keypad block (0x54..=0x63), excluding Num Lock itself.
#[inline]
pub fn is_keypad(usage: u16) -> bool {
    (KEY_KP_SLASH..=KEY_KP_DOT).contains(&usage)
}
