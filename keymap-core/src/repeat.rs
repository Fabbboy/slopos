//! Auto-repeat (typematic) state machine, keyed on canonical usages, for
//! consumers that get no hardware typematic and drive it from a periodic tick.

use slopos_abi::input::keycode::*;

/// Delay before the first auto-repeat fires.
pub const REPEAT_DELAY_MS: u64 = 500;
/// Interval between auto-repeats once repeating (~30 Hz).
pub const REPEAT_INTERVAL_MS: u64 = 33;

#[derive(Debug, Clone, Copy)]
pub struct KeyRepeat {
    key: Option<u16>,
    press_time_ms: u64,
    last_repeat_ms: u64,
    repeating: bool,
}

impl KeyRepeat {
    pub const fn new() -> Self {
        Self {
            key: None,
            press_time_ms: 0,
            last_repeat_ms: 0,
            repeating: false,
        }
    }

    /// Call on key-down. Returns `true` if this is a fresh press (not the same
    /// key already held).
    pub fn on_key_down(&mut self, key: u16, now_ms: u64) -> bool {
        if self.key == Some(key) {
            return false;
        }
        self.key = Some(key);
        self.press_time_ms = now_ms;
        self.last_repeat_ms = now_ms;
        self.repeating = false;
        true
    }

    pub fn on_key_up(&mut self, key: u16) {
        if self.key == Some(key) {
            self.key = None;
            self.repeating = false;
        }
    }

    /// Drive from a periodic tick. Returns `Some(key)` when a repeat should fire.
    pub fn tick(&mut self, now_ms: u64) -> Option<u16> {
        let key = self.key?;
        if !key_repeats(key) {
            return None;
        }
        if !self.repeating {
            if now_ms.saturating_sub(self.press_time_ms) >= REPEAT_DELAY_MS {
                self.repeating = true;
                self.last_repeat_ms = now_ms;
                return Some(key);
            }
        } else if now_ms.saturating_sub(self.last_repeat_ms) >= REPEAT_INTERVAL_MS {
            self.last_repeat_ms = now_ms;
            return Some(key);
        }
        None
    }
}

impl Default for KeyRepeat {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a key auto-repeats. Text and navigation keys repeat; modifiers,
/// locks, function, and one-shot system keys do not.
fn key_repeats(usage: u16) -> bool {
    if is_modifier(usage) {
        return false;
    }
    !matches!(
        usage,
        KEY_CAPSLOCK
            | KEY_NUMLOCK
            | KEY_SCROLLLOCK
            | KEY_PRINTSCREEN
            | KEY_SYSRQ
            | KEY_PAUSE
            | KEY_MENU
            | KEY_APPLICATION
            | KEY_ESC
            | KEY_ENTER
            | KEY_KP_ENTER
            | KEY_INSERT
            | KEY_F1
            | KEY_F2
            | KEY_F3
            | KEY_F4
            | KEY_F5
            | KEY_F6
            | KEY_F7
            | KEY_F8
            | KEY_F9
            | KEY_F10
            | KEY_F11
            | KEY_F12
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_down_is_fresh_then_repeats() {
        let mut r = KeyRepeat::new();
        assert!(r.on_key_down(KEY_A, 0));
        assert!(!r.on_key_down(KEY_A, 10)); // same key held
        assert_eq!(r.tick(100), None);
        assert_eq!(r.tick(REPEAT_DELAY_MS), Some(KEY_A));
        assert_eq!(r.tick(REPEAT_DELAY_MS + 1), None);
        assert_eq!(r.tick(REPEAT_DELAY_MS + REPEAT_INTERVAL_MS), Some(KEY_A));
    }

    #[test]
    fn release_stops_repeat() {
        let mut r = KeyRepeat::new();
        r.on_key_down(KEY_A, 0);
        r.on_key_up(KEY_A);
        assert_eq!(r.tick(10_000), None);
    }

    #[test]
    fn non_repeating_keys() {
        let mut r = KeyRepeat::new();
        r.on_key_down(KEY_LEFTSHIFT, 0);
        assert_eq!(r.tick(10_000), None);
        r.on_key_up(KEY_LEFTSHIFT);
        r.on_key_down(KEY_F5, 0);
        assert_eq!(r.tick(10_000), None);
        r.on_key_up(KEY_F5);
        // Arrows DO repeat.
        r.on_key_down(KEY_LEFT, 0);
        assert_eq!(r.tick(REPEAT_DELAY_MS), Some(KEY_LEFT));
    }
}
