//! Modifier + lock state tracking.
//!
//! [`ModTracker`] folds the eight HID modifier usages (0xE0..=0xE7) and the
//! three lock keys (Caps/Num/Scroll) into a snapshot. The `mods` byte uses the
//! ABI `MODIFIER_*` bit layout verbatim, so it drops straight into an
//! `InputEvent` and matches what the compositor already expects.

use slopos_abi::input::keycode::*;
use slopos_abi::input::{
    MODIFIER_ALT, MODIFIER_CAPS_LOCK, MODIFIER_CTRL, MODIFIER_SHIFT, MODIFIER_SUPER,
};

use crate::keymap::{Locks, Mods};

/// Caps Lock active (in `ModSnapshot::locks`).
pub const LOCK_CAPS: u8 = 1 << 0;
/// Num Lock active.
pub const LOCK_NUM: u8 = 1 << 1;
/// Scroll Lock active.
pub const LOCK_SCROLL: u8 = 1 << 2;

/// A point-in-time snapshot of modifier + lock state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModSnapshot {
    /// `MODIFIER_*` bitset (Shift/Ctrl/Alt/Super/CapsLock).
    pub mods: u8,
    /// `LOCK_*` bitset (Caps/Num/Scroll).
    pub locks: u8,
}

/// Tracks the live state of all modifier and lock keys.
#[derive(Debug, Clone, Copy)]
pub struct ModTracker {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lmeta: bool,
    rmeta: bool,
    caps: bool,
    num: bool,
    scroll: bool,
}

impl ModTracker {
    /// New tracker. Num Lock starts **on** to match typical firmware state, so a
    /// laptop's numeric keypad produces digits out of the box.
    pub const fn new() -> Self {
        Self {
            lshift: false,
            rshift: false,
            lctrl: false,
            rctrl: false,
            lalt: false,
            ralt: false,
            lmeta: false,
            rmeta: false,
            caps: false,
            num: true,
            scroll: false,
        }
    }

    /// Apply a decoded key transition. Returns `true` if `usage` was a modifier
    /// or lock key (its only effect is state — it produces no text). Lock keys
    /// toggle on press and ignore release.
    pub fn update(&mut self, usage: u16, pressed: bool) -> bool {
        match usage {
            KEY_LEFTSHIFT => self.lshift = pressed,
            KEY_RIGHTSHIFT => self.rshift = pressed,
            KEY_LEFTCTRL => self.lctrl = pressed,
            KEY_RIGHTCTRL => self.rctrl = pressed,
            KEY_LEFTALT => self.lalt = pressed,
            KEY_RIGHTALT => self.ralt = pressed,
            KEY_LEFTMETA => self.lmeta = pressed,
            KEY_RIGHTMETA => self.rmeta = pressed,
            KEY_CAPSLOCK => {
                if pressed {
                    self.caps = !self.caps;
                }
            }
            KEY_NUMLOCK => {
                if pressed {
                    self.num = !self.num;
                }
            }
            KEY_SCROLLLOCK => {
                if pressed {
                    self.scroll = !self.scroll;
                }
            }
            _ => return false,
        }
        true
    }

    #[inline]
    pub fn shift(&self) -> bool {
        self.lshift || self.rshift
    }
    #[inline]
    pub fn ctrl(&self) -> bool {
        self.lctrl || self.rctrl
    }
    #[inline]
    pub fn alt(&self) -> bool {
        self.lalt || self.ralt
    }
    #[inline]
    pub fn meta(&self) -> bool {
        self.lmeta || self.rmeta
    }
    #[inline]
    pub fn caps_lock(&self) -> bool {
        self.caps
    }
    #[inline]
    pub fn num_lock(&self) -> bool {
        self.num
    }
    #[inline]
    pub fn scroll_lock(&self) -> bool {
        self.scroll
    }

    /// Modifier view for the keymap layer.
    pub fn mods(&self) -> Mods {
        Mods {
            shift: self.shift(),
            ctrl: self.ctrl(),
            alt: self.alt(),
            meta: self.meta(),
        }
    }

    /// Lock view for the keymap layer.
    pub fn locks(&self) -> Locks {
        Locks {
            caps: self.caps,
            num: self.num,
            scroll: self.scroll,
        }
    }

    /// ABI-shaped snapshot for embedding in an `InputEvent`.
    pub fn snapshot(&self) -> ModSnapshot {
        let mut mods = 0u8;
        if self.shift() {
            mods |= MODIFIER_SHIFT;
        }
        if self.ctrl() {
            mods |= MODIFIER_CTRL;
        }
        if self.alt() {
            mods |= MODIFIER_ALT;
        }
        if self.meta() {
            mods |= MODIFIER_SUPER;
        }
        if self.caps {
            mods |= MODIFIER_CAPS_LOCK;
        }
        let mut locks = 0u8;
        if self.caps {
            locks |= LOCK_CAPS;
        }
        if self.num {
            locks |= LOCK_NUM;
        }
        if self.scroll {
            locks |= LOCK_SCROLL;
        }
        ModSnapshot { mods, locks }
    }
}

impl Default for ModTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_press_release_tracks_and_reports_modifier() {
        let mut m = ModTracker::new();
        assert!(m.update(KEY_LEFTSHIFT, true));
        assert!(m.shift());
        assert_eq!(m.snapshot().mods & MODIFIER_SHIFT, MODIFIER_SHIFT);
        assert!(m.update(KEY_LEFTSHIFT, false));
        assert!(!m.shift());
    }

    #[test]
    fn left_right_collapse() {
        let mut m = ModTracker::new();
        m.update(KEY_RIGHTCTRL, true);
        assert!(m.ctrl());
        m.update(KEY_RIGHTCTRL, false);
        assert!(!m.ctrl());
        m.update(KEY_LEFTALT, true);
        m.update(KEY_RIGHTALT, true);
        assert!(m.alt());
        m.update(KEY_LEFTALT, false);
        assert!(m.alt()); // right alt still held
    }

    #[test]
    fn caps_lock_toggles_on_press_only() {
        let mut m = ModTracker::new();
        assert!(!m.caps_lock());
        m.update(KEY_CAPSLOCK, true);
        assert!(m.caps_lock());
        m.update(KEY_CAPSLOCK, false); // release: no change
        assert!(m.caps_lock());
        m.update(KEY_CAPSLOCK, true); // press again: toggle off
        assert!(!m.caps_lock());
    }

    #[test]
    fn numlock_defaults_on_and_toggles() {
        let mut m = ModTracker::new();
        assert!(m.num_lock());
        assert_eq!(m.snapshot().locks & LOCK_NUM, LOCK_NUM);
        m.update(KEY_NUMLOCK, true);
        assert!(!m.num_lock());
    }

    #[test]
    fn non_modifier_returns_false() {
        let mut m = ModTracker::new();
        assert!(!m.update(KEY_A, true));
        assert!(!m.update(KEY_KP_7, true));
    }
}
