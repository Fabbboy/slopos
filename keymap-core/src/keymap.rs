//! Keymap / layout layer: canonical usage + modifier/lock state → produced
//! text codepoint or named key. The single home of keyboard *layout*.
//!
//! Keypad usages resolve to digits/operators or to navigation named keys
//! depending on Num Lock (Shift inverts it, as on real keyboards).

use slopos_abi::input::keycode::*;

/// Active modifier keys (left/right collapsed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Active lock states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Locks {
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

/// What a key produces under the active modifiers/locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A text codepoint. Includes control characters (`'\n'`, `'\t'`, `0x08`,
    /// `0x1B`) so the legacy ASCII path can be reconstructed exactly.
    Text(u32),
    /// A non-text action key (navigation, function, lock).
    Named(NamedKey),
    /// No output (modifier keys; KP5 with Num Lock off).
    None,
}

/// A keyboard layout: maps a canonical usage + state to an outcome.
pub trait Layout {
    fn map(&self, usage: u16, mods: Mods, locks: Locks) -> KeyOutcome;
}

/// The US-QWERTY layout.
pub struct UsQwerty;

impl Layout for UsQwerty {
    fn map(&self, usage: u16, mods: Mods, locks: Locks) -> KeyOutcome {
        // Letters: case from shift ^ caps; Ctrl+letter → control code.
        if (KEY_A..=KEY_Z).contains(&usage) {
            let base = b'a' + (usage - KEY_A) as u8;
            if mods.ctrl {
                return KeyOutcome::Text((base - b'a' + 1) as u32);
            }
            let upper = mods.shift ^ locks.caps;
            let ch = if upper { base - 0x20 } else { base };
            return KeyOutcome::Text(ch as u32);
        }

        // Number row.
        if let Some((normal, shifted)) = number_row(usage) {
            return KeyOutcome::Text(if mods.shift { shifted } else { normal } as u32);
        }

        // Punctuation.
        if let Some((normal, shifted)) = punctuation(usage) {
            return KeyOutcome::Text(if mods.shift { shifted } else { normal } as u32);
        }

        // Control / whitespace text keys.
        match usage {
            KEY_ENTER => return KeyOutcome::Text('\n' as u32),
            KEY_TAB => return KeyOutcome::Text('\t' as u32),
            KEY_SPACE => return KeyOutcome::Text(' ' as u32),
            KEY_BACKSPACE => return KeyOutcome::Text(0x08),
            KEY_ESC => return KeyOutcome::Text(0x1B),
            _ => {}
        }

        // Keypad block (Num Lock / Shift aware).
        if let Some(outcome) = keypad(usage, mods, locks) {
            return outcome;
        }

        // Navigation / function / lock named keys.
        if let Some(named) = named_key(usage) {
            return KeyOutcome::Named(named);
        }

        // Modifiers and anything unmapped produce no output.
        KeyOutcome::None
    }
}

/// Number-row `(unshifted, shifted)` for US-QWERTY.
fn number_row(usage: u16) -> Option<(u8, u8)> {
    Some(match usage {
        KEY_1 => (b'1', b'!'),
        KEY_2 => (b'2', b'@'),
        KEY_3 => (b'3', b'#'),
        KEY_4 => (b'4', b'$'),
        KEY_5 => (b'5', b'%'),
        KEY_6 => (b'6', b'^'),
        KEY_7 => (b'7', b'&'),
        KEY_8 => (b'8', b'*'),
        KEY_9 => (b'9', b'('),
        KEY_0 => (b'0', b')'),
        _ => return None,
    })
}

/// Punctuation `(unshifted, shifted)` for US-QWERTY.
fn punctuation(usage: u16) -> Option<(u8, u8)> {
    Some(match usage {
        KEY_MINUS => (b'-', b'_'),
        KEY_EQUAL => (b'=', b'+'),
        KEY_LEFTBRACE => (b'[', b'{'),
        KEY_RIGHTBRACE => (b']', b'}'),
        KEY_BACKSLASH | KEY_NONUS_HASH | KEY_NONUS_BACKSLASH => (b'\\', b'|'),
        KEY_SEMICOLON => (b';', b':'),
        KEY_APOSTROPHE => (b'\'', b'"'),
        KEY_GRAVE => (b'`', b'~'),
        KEY_COMMA => (b',', b'<'),
        KEY_DOT => (b'.', b'>'),
        KEY_SLASH => (b'/', b'?'),
        _ => return None,
    })
}

/// Keypad resolution. Operators and KP-Enter are unconditional; digit keys
/// produce a digit when `num XOR shift`, otherwise their navigation function.
fn keypad(usage: u16, mods: Mods, locks: Locks) -> Option<KeyOutcome> {
    match usage {
        KEY_KP_SLASH => return Some(KeyOutcome::Text(b'/' as u32)),
        KEY_KP_ASTERISK => return Some(KeyOutcome::Text(b'*' as u32)),
        KEY_KP_MINUS => return Some(KeyOutcome::Text(b'-' as u32)),
        KEY_KP_PLUS => return Some(KeyOutcome::Text(b'+' as u32)),
        KEY_KP_ENTER => return Some(KeyOutcome::Text('\n' as u32)),
        _ => {}
    }

    let digit_mode = locks.num ^ mods.shift;
    let outcome = match usage {
        KEY_KP_0 => digit_or(digit_mode, b'0', NamedKey::Insert),
        KEY_KP_1 => digit_or(digit_mode, b'1', NamedKey::End),
        KEY_KP_2 => digit_or(digit_mode, b'2', NamedKey::Down),
        KEY_KP_3 => digit_or(digit_mode, b'3', NamedKey::PageDown),
        KEY_KP_4 => digit_or(digit_mode, b'4', NamedKey::Left),
        KEY_KP_5 => {
            if digit_mode {
                KeyOutcome::Text(b'5' as u32)
            } else {
                KeyOutcome::None // KP5 has no navigation function
            }
        }
        KEY_KP_6 => digit_or(digit_mode, b'6', NamedKey::Right),
        KEY_KP_7 => digit_or(digit_mode, b'7', NamedKey::Home),
        KEY_KP_8 => digit_or(digit_mode, b'8', NamedKey::Up),
        KEY_KP_9 => digit_or(digit_mode, b'9', NamedKey::PageUp),
        KEY_KP_DOT => digit_or(digit_mode, b'.', NamedKey::Delete),
        _ => return None,
    };
    Some(outcome)
}

#[inline]
fn digit_or(digit_mode: bool, digit: u8, nav: NamedKey) -> KeyOutcome {
    if digit_mode {
        KeyOutcome::Text(digit as u32)
    } else {
        KeyOutcome::Named(nav)
    }
}

/// Navigation / function / lock usages → named keys. (Text-producing keys are
/// handled before this is reached.)
fn named_key(usage: u16) -> Option<NamedKey> {
    Some(match usage {
        KEY_LEFT => NamedKey::Left,
        KEY_RIGHT => NamedKey::Right,
        KEY_UP => NamedKey::Up,
        KEY_DOWN => NamedKey::Down,
        KEY_HOME => NamedKey::Home,
        KEY_END => NamedKey::End,
        KEY_PAGEUP => NamedKey::PageUp,
        KEY_PAGEDOWN => NamedKey::PageDown,
        KEY_INSERT => NamedKey::Insert,
        KEY_DELETE => NamedKey::Delete,
        KEY_CAPSLOCK => NamedKey::CapsLock,
        KEY_NUMLOCK => NamedKey::NumLock,
        KEY_SCROLLLOCK => NamedKey::ScrollLock,
        KEY_PRINTSCREEN => NamedKey::PrintScreen,
        KEY_PAUSE => NamedKey::Pause,
        KEY_APPLICATION | KEY_MENU => NamedKey::Menu,
        KEY_F1 => NamedKey::F1,
        KEY_F2 => NamedKey::F2,
        KEY_F3 => NamedKey::F3,
        KEY_F4 => NamedKey::F4,
        KEY_F5 => NamedKey::F5,
        KEY_F6 => NamedKey::F6,
        KEY_F7 => NamedKey::F7,
        KEY_F8 => NamedKey::F8,
        KEY_F9 => NamedKey::F9,
        KEY_F10 => NamedKey::F10,
        KEY_F11 => NamedKey::F11,
        KEY_F12 => NamedKey::F12,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
        meta: false,
    };
    const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
        meta: false,
    };
    const NUM_ON: Locks = Locks {
        caps: false,
        num: true,
        scroll: false,
    };
    const NUM_OFF: Locks = Locks {
        caps: false,
        num: false,
        scroll: false,
    };
    const CAPS_ON: Locks = Locks {
        caps: true,
        num: true,
        scroll: false,
    };

    fn text(o: KeyOutcome) -> u32 {
        match o {
            KeyOutcome::Text(c) => c,
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn letters_shift_and_caps() {
        let km = UsQwerty;
        assert_eq!(text(km.map(KEY_A, NONE, NUM_ON)), b'a' as u32);
        assert_eq!(text(km.map(KEY_A, SHIFT, NUM_ON)), b'A' as u32);
        assert_eq!(text(km.map(KEY_A, NONE, CAPS_ON)), b'A' as u32);
        // Shift + CapsLock cancels back to lowercase.
        assert_eq!(text(km.map(KEY_A, SHIFT, CAPS_ON)), b'a' as u32);
    }

    #[test]
    fn ctrl_letter_is_control_code() {
        let km = UsQwerty;
        assert_eq!(text(km.map(KEY_C, CTRL, NUM_ON)), 0x03); // Ctrl+C
        assert_eq!(text(km.map(KEY_A, CTRL, NUM_ON)), 0x01); // Ctrl+A
        // Ctrl on a number is unaffected (only letters transform).
        assert_eq!(text(km.map(KEY_1, CTRL, NUM_ON)), b'1' as u32);
    }

    #[test]
    fn number_row_symbols() {
        let km = UsQwerty;
        assert_eq!(text(km.map(KEY_1, NONE, NUM_ON)), b'1' as u32);
        assert_eq!(text(km.map(KEY_1, SHIFT, NUM_ON)), b'!' as u32);
        assert_eq!(text(km.map(KEY_2, SHIFT, NUM_ON)), b'@' as u32);
        assert_eq!(text(km.map(KEY_LEFTBRACE, SHIFT, NUM_ON)), b'{' as u32);
    }

    #[test]
    fn whitespace_and_control_keys() {
        let km = UsQwerty;
        assert_eq!(text(km.map(KEY_ENTER, NONE, NUM_ON)), '\n' as u32);
        assert_eq!(text(km.map(KEY_TAB, NONE, NUM_ON)), '\t' as u32);
        assert_eq!(text(km.map(KEY_SPACE, NONE, NUM_ON)), ' ' as u32);
        assert_eq!(text(km.map(KEY_BACKSPACE, NONE, NUM_ON)), 0x08);
        assert_eq!(text(km.map(KEY_ESC, NONE, NUM_ON)), 0x1B);
    }

    #[test]
    fn keypad_numlock_matrix() {
        let km = UsQwerty;
        // Num Lock ON → digits.
        assert_eq!(text(km.map(KEY_KP_7, NONE, NUM_ON)), b'7' as u32);
        assert_eq!(text(km.map(KEY_KP_0, NONE, NUM_ON)), b'0' as u32);
        assert_eq!(text(km.map(KEY_KP_DOT, NONE, NUM_ON)), b'.' as u32);
        // Num Lock OFF → navigation.
        assert_eq!(
            km.map(KEY_KP_7, NONE, NUM_OFF),
            KeyOutcome::Named(NamedKey::Home)
        );
        assert_eq!(
            km.map(KEY_KP_8, NONE, NUM_OFF),
            KeyOutcome::Named(NamedKey::Up)
        );
        assert_eq!(
            km.map(KEY_KP_2, NONE, NUM_OFF),
            KeyOutcome::Named(NamedKey::Down)
        );
        assert_eq!(
            km.map(KEY_KP_0, NONE, NUM_OFF),
            KeyOutcome::Named(NamedKey::Insert)
        );
        assert_eq!(
            km.map(KEY_KP_DOT, NONE, NUM_OFF),
            KeyOutcome::Named(NamedKey::Delete)
        );
        assert_eq!(km.map(KEY_KP_5, NONE, NUM_OFF), KeyOutcome::None);
        // Shift inverts Num Lock for digit keys.
        assert_eq!(
            km.map(KEY_KP_7, SHIFT, NUM_ON),
            KeyOutcome::Named(NamedKey::Home)
        );
        assert_eq!(text(km.map(KEY_KP_7, SHIFT, NUM_OFF)), b'7' as u32);
    }

    #[test]
    fn keypad_operators_unconditional() {
        let km = UsQwerty;
        assert_eq!(text(km.map(KEY_KP_PLUS, NONE, NUM_OFF)), b'+' as u32);
        assert_eq!(text(km.map(KEY_KP_MINUS, NONE, NUM_OFF)), b'-' as u32);
        assert_eq!(text(km.map(KEY_KP_ASTERISK, NONE, NUM_OFF)), b'*' as u32);
        assert_eq!(text(km.map(KEY_KP_SLASH, NONE, NUM_OFF)), b'/' as u32);
        assert_eq!(text(km.map(KEY_KP_ENTER, NONE, NUM_OFF)), '\n' as u32);
    }

    #[test]
    fn named_and_modifier_outcomes() {
        let km = UsQwerty;
        assert_eq!(
            km.map(KEY_LEFT, NONE, NUM_ON),
            KeyOutcome::Named(NamedKey::Left)
        );
        assert_eq!(
            km.map(KEY_F5, NONE, NUM_ON),
            KeyOutcome::Named(NamedKey::F5)
        );
        assert_eq!(
            km.map(KEY_DELETE, NONE, NUM_ON),
            KeyOutcome::Named(NamedKey::Delete)
        );
        // Modifiers produce nothing.
        assert_eq!(km.map(KEY_LEFTCTRL, NONE, NUM_ON), KeyOutcome::None);
        assert_eq!(km.map(KEY_RIGHTALT, NONE, NUM_ON), KeyOutcome::None);
    }
}
