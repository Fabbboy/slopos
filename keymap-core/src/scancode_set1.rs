//! PS/2 scan-code-set-1 → canonical HID usage decoder.
//!
//! The i8042 controller, with its hardware translation bit enabled (the SlopOS
//! default), delivers the keyboard's set-2 codes already translated to set 1.
//! [`Set1Decoder`] owns the `0xE0` extended-key latch, swallows the multi-byte
//! `0xE1` Pause sequence, and filters the PS/2 "fake shift" wrappers
//! (`E0 2A` / `E0 AA`) around extended keys.

use slopos_abi::input::keycode::*;

/// One decoded key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeStep {
    /// Canonical HID Keyboard/Keypad usage.
    pub usage: u16,
    /// `true` = key pressed (make), `false` = released (break).
    pub pressed: bool,
}

/// Byte-at-a-time set-1 decoder.
#[derive(Debug, Clone, Copy)]
pub struct Set1Decoder {
    /// `0xE0` extended-key latch: the next code is from the extended table.
    extended: bool,
    /// Remaining bytes to swallow for an in-progress `0xE1` (Pause) sequence.
    e1_remaining: u8,
}

impl Set1Decoder {
    pub const fn new() -> Self {
        Self {
            extended: false,
            e1_remaining: 0,
        }
    }

    /// Feed one raw scancode byte. Returns `Some(step)` once a full key
    /// transition is decoded, or `None` for prefix/filtered/unknown bytes.
    pub fn feed(&mut self, byte: u8) -> Option<DecodeStep> {
        // Pause is `E1 1D 45` / `E1 9D C5` — two trailing bytes either way, which
        // must be consumed rather than mis-decoded as 1D/45.
        if self.e1_remaining > 0 {
            self.e1_remaining -= 1;
            return None;
        }

        match byte {
            0xE0 => {
                self.extended = true;
                None
            }
            0xE1 => {
                self.e1_remaining = 2;
                None
            }
            _ => {
                let extended = self.extended;
                self.extended = false;
                let pressed = byte & 0x80 == 0;
                let make = byte & 0x7F;

                // PS/2 "fake shift" wrappers around extended keys (E0 2A / E0 AA,
                // E0 36 / E0 B6) must not touch real shift state and produce no key.
                if extended && (make == 0x2A || make == 0x36) {
                    return None;
                }

                let usage = if extended {
                    ext_usage(make)
                } else {
                    base_usage(make)
                };
                usage.map(|usage| DecodeStep { usage, pressed })
            }
        }
    }
}

impl Default for Set1Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Non-extended set-1 make code → canonical usage.
fn base_usage(make: u8) -> Option<u16> {
    Some(match make {
        0x01 => KEY_ESC,
        0x02 => KEY_1,
        0x03 => KEY_2,
        0x04 => KEY_3,
        0x05 => KEY_4,
        0x06 => KEY_5,
        0x07 => KEY_6,
        0x08 => KEY_7,
        0x09 => KEY_8,
        0x0A => KEY_9,
        0x0B => KEY_0,
        0x0C => KEY_MINUS,
        0x0D => KEY_EQUAL,
        0x0E => KEY_BACKSPACE,
        0x0F => KEY_TAB,
        0x10 => KEY_Q,
        0x11 => KEY_W,
        0x12 => KEY_E,
        0x13 => KEY_R,
        0x14 => KEY_T,
        0x15 => KEY_Y,
        0x16 => KEY_U,
        0x17 => KEY_I,
        0x18 => KEY_O,
        0x19 => KEY_P,
        0x1A => KEY_LEFTBRACE,
        0x1B => KEY_RIGHTBRACE,
        0x1C => KEY_ENTER,
        0x1D => KEY_LEFTCTRL,
        0x1E => KEY_A,
        0x1F => KEY_S,
        0x20 => KEY_D,
        0x21 => KEY_F,
        0x22 => KEY_G,
        0x23 => KEY_H,
        0x24 => KEY_J,
        0x25 => KEY_K,
        0x26 => KEY_L,
        0x27 => KEY_SEMICOLON,
        0x28 => KEY_APOSTROPHE,
        0x29 => KEY_GRAVE,
        0x2A => KEY_LEFTSHIFT,
        0x2B => KEY_BACKSLASH,
        0x2C => KEY_Z,
        0x2D => KEY_X,
        0x2E => KEY_C,
        0x2F => KEY_V,
        0x30 => KEY_B,
        0x31 => KEY_N,
        0x32 => KEY_M,
        0x33 => KEY_COMMA,
        0x34 => KEY_DOT,
        0x35 => KEY_SLASH,
        0x36 => KEY_RIGHTSHIFT,
        0x37 => KEY_KP_ASTERISK,
        0x38 => KEY_LEFTALT,
        0x39 => KEY_SPACE,
        0x3A => KEY_CAPSLOCK,
        0x3B => KEY_F1,
        0x3C => KEY_F2,
        0x3D => KEY_F3,
        0x3E => KEY_F4,
        0x3F => KEY_F5,
        0x40 => KEY_F6,
        0x41 => KEY_F7,
        0x42 => KEY_F8,
        0x43 => KEY_F9,
        0x44 => KEY_F10,
        0x45 => KEY_NUMLOCK,
        0x46 => KEY_SCROLLLOCK,
        0x47 => KEY_KP_7,
        0x48 => KEY_KP_8,
        0x49 => KEY_KP_9,
        0x4A => KEY_KP_MINUS,
        0x4B => KEY_KP_4,
        0x4C => KEY_KP_5,
        0x4D => KEY_KP_6,
        0x4E => KEY_KP_PLUS,
        0x4F => KEY_KP_1,
        0x50 => KEY_KP_2,
        0x51 => KEY_KP_3,
        0x52 => KEY_KP_0,
        0x53 => KEY_KP_DOT,
        // Alt+PrintScreen is reported as its own bare make code, not as
        // PrintScreen's `E0 2A E0 37`, so it decodes here and not in `ext_usage`.
        0x54 => KEY_SYSRQ,
        0x56 => KEY_NONUS_BACKSLASH,
        0x57 => KEY_F11,
        0x58 => KEY_F12,
        _ => return None,
    })
}

/// `0xE0`-prefixed set-1 make code → canonical usage.
fn ext_usage(make: u8) -> Option<u16> {
    Some(match make {
        0x1C => KEY_KP_ENTER,
        0x1D => KEY_RIGHTCTRL,
        0x35 => KEY_KP_SLASH,
        // PrintScreen press is `E0 2A E0 37`; the fake shift is filtered in `feed`.
        0x37 => KEY_PRINTSCREEN,
        0x38 => KEY_RIGHTALT,
        0x46 => KEY_PAUSE, // Ctrl+Break (E0 46) — best-effort Pause.
        0x47 => KEY_HOME,
        0x48 => KEY_UP,
        0x49 => KEY_PAGEUP,
        0x4B => KEY_LEFT,
        0x4D => KEY_RIGHT,
        0x4F => KEY_END,
        0x50 => KEY_DOWN,
        0x51 => KEY_PAGEDOWN,
        0x52 => KEY_INSERT,
        0x53 => KEY_DELETE,
        0x5B => KEY_LEFTMETA,
        0x5C => KEY_RIGHTMETA,
        0x5D => KEY_APPLICATION,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(byte: u8) -> Option<DecodeStep> {
        Set1Decoder::new().feed(byte)
    }

    #[test]
    fn base_letters_and_press_release() {
        // 'a' make = 0x1E, break = 0x9E.
        assert_eq!(
            one(0x1E),
            Some(DecodeStep {
                usage: KEY_A,
                pressed: true
            })
        );
        assert_eq!(
            one(0x9E),
            Some(DecodeStep {
                usage: KEY_A,
                pressed: false
            })
        );
    }

    #[test]
    fn keypad_block_decodes_distinctly_from_nav() {
        assert_eq!(one(0x47).unwrap().usage, KEY_KP_7);
        assert_eq!(one(0x48).unwrap().usage, KEY_KP_8);
        assert_eq!(one(0x49).unwrap().usage, KEY_KP_9);
        assert_eq!(one(0x4A).unwrap().usage, KEY_KP_MINUS);
        assert_eq!(one(0x4B).unwrap().usage, KEY_KP_4);
        assert_eq!(one(0x4C).unwrap().usage, KEY_KP_5);
        assert_eq!(one(0x4D).unwrap().usage, KEY_KP_6);
        assert_eq!(one(0x4E).unwrap().usage, KEY_KP_PLUS);
        assert_eq!(one(0x4F).unwrap().usage, KEY_KP_1);
        assert_eq!(one(0x50).unwrap().usage, KEY_KP_2);
        assert_eq!(one(0x51).unwrap().usage, KEY_KP_3);
        assert_eq!(one(0x52).unwrap().usage, KEY_KP_0);
        assert_eq!(one(0x53).unwrap().usage, KEY_KP_DOT);
        assert_eq!(one(0x37).unwrap().usage, KEY_KP_ASTERISK);
        assert_eq!(one(0x45).unwrap().usage, KEY_NUMLOCK);
    }

    #[test]
    fn extended_nav_keys() {
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x48).unwrap().usage, KEY_UP);
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x4B).unwrap().usage, KEY_LEFT);
        // E0 53 = Delete (distinct from KP_DOT's 0x53).
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x53).unwrap().usage, KEY_DELETE);
    }

    #[test]
    fn extended_keypad_enter_and_slash() {
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x1C).unwrap().usage, KEY_KP_ENTER);
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x35).unwrap().usage, KEY_KP_SLASH);
    }

    #[test]
    fn extended_right_modifiers_distinct_from_left() {
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0x1D).unwrap().usage, KEY_LEFTCTRL);
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x1D).unwrap().usage, KEY_RIGHTCTRL);
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x38).unwrap().usage, KEY_RIGHTALT);
    }

    #[test]
    fn fake_shift_wrappers_filtered() {
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x2A), None);
        // The very next real key decodes from the BASE table, not extended.
        assert_eq!(d.feed(0x1E).unwrap().usage, KEY_A);
    }

    #[test]
    fn printscreen_make_sequence() {
        // Real PrintScreen press: E0 2A E0 37.
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(d.feed(0x2A), None);
        assert_eq!(d.feed(0xE0), None);
        assert_eq!(
            d.feed(0x37),
            Some(DecodeStep {
                usage: KEY_PRINTSCREEN,
                pressed: true
            })
        );
    }

    #[test]
    fn sysrq_is_a_bare_make_code() {
        // Alt+PrintScreen: SysRq is bare 0x54 / 0xD4, no E0 prefix, no fake shift.
        let mut d = Set1Decoder::new();
        assert_eq!(
            d.feed(0x54),
            Some(DecodeStep {
                usage: KEY_SYSRQ,
                pressed: true
            })
        );
        assert_eq!(
            d.feed(0xD4),
            Some(DecodeStep {
                usage: KEY_SYSRQ,
                pressed: false
            })
        );
    }

    #[test]
    fn sysrq_does_not_shadow_the_keypad_asterisk() {
        // Bare 0x37 is the keypad `*`; only the E0-prefixed form is PrintScreen.
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0x37).unwrap().usage, KEY_KP_ASTERISK);
    }

    #[test]
    fn pause_sequence_swallowed() {
        let mut d = Set1Decoder::new();
        assert_eq!(d.feed(0xE1), None);
        assert_eq!(d.feed(0x1D), None);
        assert_eq!(d.feed(0x45), None);
        assert_eq!(d.feed(0x1E).unwrap().usage, KEY_A);
    }

    #[test]
    fn unknown_codes_return_none() {
        assert_eq!(one(0x00), None);
        assert_eq!(one(0x7F), None);
    }
}
