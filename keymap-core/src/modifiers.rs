//! Modifier + lock state tracking.
//!
//! [`ModTracker`] folds the eight HID modifier usages (0xE0..=0xE7) and the three
//! lock keys into a snapshot whose `mods` byte uses the ABI `MODIFIER_*` bit
//! layout verbatim, so it drops straight into an `InputEvent`.

use slopos_abi::input::keycode::*;
use slopos_abi::input::{
    MODIFIER_ALT, MODIFIER_ALTGR, MODIFIER_CAPS_LOCK, MODIFIER_CTRL, MODIFIER_NUM_LOCK,
    MODIFIER_SCROLL_LOCK, MODIFIER_SHIFT, MODIFIER_SUPER,
};

use crate::keymap::{Locks, Mods};

pub const LOCK_CAPS: u8 = 1 << 0;
pub const LOCK_NUM: u8 = 1 << 1;
pub const LOCK_SCROLL: u8 = 1 << 2;

/// A point-in-time snapshot of modifier + lock state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModSnapshot {
    /// `MODIFIER_*` bitset (Shift/Ctrl/Alt/Super/CapsLock).
    pub mods: u8,
    /// `LOCK_*` bitset (Caps/Num/Scroll).
    pub locks: u8,
}

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
    /// Num Lock starts **on**, matching typical firmware state.
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

    /// Apply a decoded key transition. Returns `true` if `usage` was a modifier or
    /// lock key. Lock keys toggle on press and ignore release.
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
            altgr: self.ralt,
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
        if self.ralt {
            // AltGr also keeps MODIFIER_ALT set above.
            mods |= MODIFIER_ALTGR;
        }
        if self.meta() {
            mods |= MODIFIER_SUPER;
        }
        if self.caps {
            mods |= MODIFIER_CAPS_LOCK;
        }
        if self.num {
            mods |= MODIFIER_NUM_LOCK;
        }
        if self.scroll {
            mods |= MODIFIER_SCROLL_LOCK;
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

/// Reconstruct [`Mods`] + [`Locks`] from an ABI `MODIFIER_*` snapshot byte, so a
/// downstream consumer can re-run the shared [`Layout`] off a forwarded event.
///
/// [`Layout`]: crate::keymap::Layout
pub fn mods_locks_from_raw(raw: u8) -> (Mods, Locks) {
    let mods = Mods {
        shift: raw & MODIFIER_SHIFT != 0,
        ctrl: raw & MODIFIER_CTRL != 0,
        alt: raw & MODIFIER_ALT != 0,
        meta: raw & MODIFIER_SUPER != 0,
        altgr: raw & MODIFIER_ALTGR != 0,
    };
    let locks = Locks {
        caps: raw & MODIFIER_CAPS_LOCK != 0,
        num: raw & MODIFIER_NUM_LOCK != 0,
        scroll: raw & MODIFIER_SCROLL_LOCK != 0,
    };
    (mods, locks)
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
        m.update(KEY_CAPSLOCK, false);
        assert!(m.caps_lock());
        m.update(KEY_CAPSLOCK, true);
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
