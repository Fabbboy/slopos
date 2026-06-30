//! Keymap / layout layer: canonical usage + modifier/lock state → produced text
//! codepoint or named key. The single home of keyboard *layout*.
//!
//! Resolution is **data-driven**: a [`LayoutTable`] holds the per-key, per-level
//! glyphs (base / shift / AltGr / shift+AltGr), a per-key caps-affected bit, and
//! a dead-key compose table. [`resolve`] is the full engine — it picks the level
//! from the active modifiers/locks, runs the dead-key state machine, and applies
//! the kernel/TTY Ctrl→control-code transform. Layout-*independent* keys (named
//! keys, the numeric keypad, NumLock, whitespace control keys) are resolved in
//! code here and never appear in the table.
//!
//! The built-in US-QWERTY layout ([`crate::layout_table::US_QWERTY`]) backs the
//! historical [`UsQwerty`]/[`char_for`]/[`ui_classify`] entry points so existing
//! kernel and GUI call sites keep working unchanged while the kernel gains a
//! runtime-swappable active layout.

use slopos_abi::input::keycode::*;

use crate::layout_table::{CellKind, LayoutTable, US_QWERTY};

/// Active modifier keys (left/right collapsed, except AltGr).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
    /// AltGr (right Alt) — selects the level-3 / level-4 glyph columns. Distinct
    /// from `alt` so a layout can put characters on AltGr without "right Alt"
    /// also meaning a plain Alt chord.
    pub altgr: bool,
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
    /// No output (modifier keys; a freshly-pressed dead key; KP5 with Num Lock off).
    None,
}

/// Per-keyboard dead-key state: the accent awaiting its next key, if any.
///
/// Lives wherever the codepoint is computed (the kernel keyboard state, or a
/// test). A freshly-pressed dead key arms `pending` and produces no text; the
/// next key composes against it (see [`resolve`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeadKeyState {
    pub pending: Option<u8>,
}

impl DeadKeyState {
    pub const fn new() -> Self {
        Self { pending: None }
    }

    /// Clear any pending dead key (e.g. on focus change or layout swap).
    pub fn reset(&mut self) {
        self.pending = None;
    }
}

/// The full result of resolving a key against a layout: an optional accent to
/// emit **before** the main outcome (a dead-key flush), plus the outcome itself.
///
/// `flush` is non-zero only when a pending dead key did not compose with this
/// key and must be emitted as its bare spacing accent first. The kernel routes
/// it as its own input event ahead of the key's event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub flush: u32,
    pub outcome: KeyOutcome,
}

impl Resolved {
    pub const fn none() -> Self {
        Self {
            flush: 0,
            outcome: KeyOutcome::None,
        }
    }

    pub const fn text(cp: u32) -> Self {
        Self {
            flush: 0,
            outcome: KeyOutcome::Text(cp),
        }
    }

    pub const fn named(nk: NamedKey) -> Self {
        Self {
            flush: 0,
            outcome: KeyOutcome::Named(nk),
        }
    }

    fn with_flush(mut self, accent: u32) -> Self {
        self.flush = accent;
        self
    }
}

/// Select the level (0..4) for a key, honoring AltGr and the per-key caps fold.
///
/// CapsLock case-folds only caps-affected keys, acting like Shift; Caps+Shift on
/// such a key cancels back to the base case (Linux `KT_LETTER` semantics).
fn level_index(mods: Mods, locks: Locks, caps_affected: bool) -> usize {
    let eff_shift = mods.shift ^ (locks.caps && caps_affected);
    match (mods.altgr, eff_shift) {
        (false, false) => 0,
        (false, true) => 1,
        (true, false) => 2,
        (true, true) => 3,
    }
}

/// Resolve a key's layout-dependent cell at the active level, falling back to a
/// lower level when the selected one is empty (XKB-style: missing upper level →
/// lower). Returns the decoded [`CellKind`] (text literal, dead key, or none).
fn resolve_level(table: &LayoutTable, usage: u16, mods: Mods, locks: Locks) -> CellKind {
    let caps_affected = table.caps_affected(usage);
    let mut level = level_index(mods, locks, caps_affected);
    loop {
        match table.cell(usage, level).kind() {
            CellKind::None => {
                level = match level {
                    3 => 2,
                    2 => 0,
                    1 => 0,
                    _ => return CellKind::None,
                };
            }
            kind => return kind,
        }
    }
}

/// The codepoint this key contributes as a dead-key *compose base*: its literal
/// glyph, or `0x20` for Space. `None` for named/control keys (which end a dead
/// sequence by flushing the bare accent).
fn compose_base(usage: u16, level: CellKind) -> Option<u32> {
    match level {
        CellKind::Literal(cp) => Some(cp),
        CellKind::Dead(_) => None, // handled by the caller (dead + dead)
        CellKind::None => {
            if usage == KEY_SPACE {
                Some(0x20)
            } else {
                None
            }
        }
    }
}

/// Resolve a key press against `table`, threading the per-keyboard dead-key
/// state. This is the full engine used by the kernel keyboard driver.
///
/// Modifier keys never reach here (the caller filters them), but are handled
/// defensively as no-output.
pub fn resolve(
    table: &LayoutTable,
    usage: u16,
    mods: Mods,
    locks: Locks,
    dead: &mut DeadKeyState,
) -> Resolved {
    if is_modifier(usage) {
        return Resolved::none();
    }

    let level = resolve_level(table, usage, mods, locks);

    // A dead key is already pending: compose against it.
    if let Some(d) = dead.pending.take() {
        if let CellKind::Dead(d2) = level {
            // dead + dead → emit the first accent now, arm the new dead key.
            dead.pending = Some(d2);
            return Resolved::text(table.dead_accent_of(d));
        }
        if let Some(base) = compose_base(usage, level) {
            if let Some(result) = table.compose_lookup(d, base) {
                return Resolved::text(result);
            }
            if base == 0x20 {
                // dead + space → the bare spacing accent.
                return Resolved::text(table.dead_accent_of(d));
            }
            // No composition: flush the accent, then emit the base normally.
            return finish(usage, mods, locks, level).with_flush(table.dead_accent_of(d));
        }
        // A named/control key ends the sequence: flush the accent, then handle it.
        return finish(usage, mods, locks, level).with_flush(table.dead_accent_of(d));
    }

    // Fresh press of a dead key: arm it, produce nothing now.
    if let CellKind::Dead(d) = level {
        dead.pending = Some(d);
        return Resolved::none();
    }

    finish(usage, mods, locks, level)
}

/// Resolve a non-dead, non-pending key to its final outcome (text literal with
/// the Ctrl→control transform, or a layout-independent key).
fn finish(usage: u16, mods: Mods, locks: Locks, level: CellKind) -> Resolved {
    if let CellKind::Literal(cp) = level {
        if mods.ctrl {
            if let Some(ctrl) = ctrl_transform(cp) {
                return Resolved::text(ctrl);
            }
        }
        return Resolved::text(cp);
    }

    // Layout-independent text keys.
    if let Some(c) = control_char(usage) {
        return Resolved::text(c);
    }
    if usage == KEY_SPACE {
        return Resolved::text(' ' as u32);
    }
    if let Some(outcome) = keypad(usage, mods, locks) {
        return Resolved { flush: 0, outcome };
    }
    if let Some(named) = named_key(usage) {
        return Resolved::named(named);
    }

    Resolved::none()
}

/// The classic terminal Ctrl→control-code transform, applied to letters only:
/// `Ctrl+A` → `0x01` … `Ctrl+Z` → `0x1A`. Driven by the *resolved* glyph (the
/// layout letter), so on a QWERTZ layout `Ctrl` on the key labelled `Z` sends
/// `0x1A` regardless of physical position. Non-letters are unaffected.
fn ctrl_transform(cp: u32) -> Option<u32> {
    char::from_u32(cp)
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| (c.to_ascii_uppercase() as u32) & 0x1F)
}

/// Layout-independent control/whitespace text keys.
fn control_char(usage: u16) -> Option<u32> {
    Some(match usage {
        KEY_ENTER => '\n' as u32,
        KEY_TAB => '\t' as u32,
        KEY_BACKSPACE => 0x08,
        KEY_ESC => 0x1B,
        _ => return None,
    })
}

/// A key's classification for a GUI toolkit: a named action key, a printable
/// character, or nothing. Distinct from [`KeyOutcome`] in that the toolkit
/// policy treats Enter/Tab/Space/Backspace/Escape as *named* keys (so widgets
/// can act on them) and never folds Ctrl into the character (so Ctrl+A still
/// classifies as `'a'` for shortcut matching).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKey {
    Named(NamedKey),
    Char(char),
    None,
}

/// A keyboard layout: maps a canonical usage + state to an outcome.
///
/// Retained for source compatibility; the layout *data* now lives in
/// [`LayoutTable`]. [`UsQwerty`] resolves against the built-in US table.
pub trait Layout {
    fn map(&self, usage: u16, mods: Mods, locks: Locks) -> KeyOutcome;
}

/// The US-QWERTY layout (a handle onto the built-in [`US_QWERTY`] table).
pub struct UsQwerty;

impl Layout for UsQwerty {
    fn map(&self, usage: u16, mods: Mods, locks: Locks) -> KeyOutcome {
        let mut dead = DeadKeyState::new();
        resolve(&US_QWERTY, usage, mods, locks, &mut dead).outcome
    }
}

/// The printable character a key produces under `mods`/`locks`, **ignoring
/// Ctrl/Alt** (so Ctrl+A still yields `'a'`), against the given `table`. Returns
/// `None` for keys that produce only a named action (navigation, function, lock)
/// or a freshly-pressed dead key. Keypad keys yield their digit only when in
/// digit mode (`num XOR shift`); operators and KP-Enter are unconditional.
pub fn char_for_table(table: &LayoutTable, usage: u16, mods: Mods, locks: Locks) -> Option<char> {
    if let CellKind::Literal(cp) = resolve_level(table, usage, mods, locks) {
        return char::from_u32(cp);
    }
    match usage {
        KEY_ENTER | KEY_KP_ENTER => return Some('\n'),
        KEY_TAB => return Some('\t'),
        KEY_SPACE => return Some(' '),
        KEY_BACKSPACE => return Some('\u{08}'),
        KEY_ESC => return Some('\u{1b}'),
        KEY_KP_SLASH => return Some('/'),
        KEY_KP_ASTERISK => return Some('*'),
        KEY_KP_MINUS => return Some('-'),
        KEY_KP_PLUS => return Some('+'),
        _ => {}
    }
    if locks.num ^ mods.shift {
        let digit = match usage {
            KEY_KP_0 => b'0',
            KEY_KP_1 => b'1',
            KEY_KP_2 => b'2',
            KEY_KP_3 => b'3',
            KEY_KP_4 => b'4',
            KEY_KP_5 => b'5',
            KEY_KP_6 => b'6',
            KEY_KP_7 => b'7',
            KEY_KP_8 => b'8',
            KEY_KP_9 => b'9',
            KEY_KP_DOT => b'.',
            _ => return None,
        };
        return Some(digit as char);
    }
    None
}

/// [`char_for_table`] against the built-in US layout.
pub fn char_for(usage: u16, mods: Mods, locks: Locks) -> Option<char> {
    char_for_table(&US_QWERTY, usage, mods, locks)
}

/// The named (non-text) key a usage produces under `mods`/`locks`, including
/// Num-Lock-off keypad navigation. Layout-independent (no table needed).
pub fn named_for(usage: u16, mods: Mods, locks: Locks) -> Option<NamedKey> {
    if !(locks.num ^ mods.shift) {
        let nav = match usage {
            KEY_KP_0 => Some(NamedKey::Insert),
            KEY_KP_1 => Some(NamedKey::End),
            KEY_KP_2 => Some(NamedKey::Down),
            KEY_KP_3 => Some(NamedKey::PageDown),
            KEY_KP_4 => Some(NamedKey::Left),
            KEY_KP_6 => Some(NamedKey::Right),
            KEY_KP_7 => Some(NamedKey::Home),
            KEY_KP_8 => Some(NamedKey::Up),
            KEY_KP_9 => Some(NamedKey::PageUp),
            KEY_KP_DOT => Some(NamedKey::Delete),
            _ => None,
        };
        if nav.is_some() {
            return nav;
        }
    }
    named_key(usage)
}

/// Classify a key for a GUI toolkit against `table` (Ctrl-independent character,
/// named-key policy). See [`UiKey`].
pub fn ui_classify_table(table: &LayoutTable, usage: u16, mods: Mods, locks: Locks) -> UiKey {
    match usage {
        KEY_ENTER | KEY_KP_ENTER => return UiKey::Named(NamedKey::Enter),
        KEY_TAB => return UiKey::Named(NamedKey::Tab),
        KEY_BACKSPACE => return UiKey::Named(NamedKey::Backspace),
        KEY_ESC => return UiKey::Named(NamedKey::Escape),
        KEY_SPACE => return UiKey::Named(NamedKey::Space),
        _ => {}
    }
    if let Some(named) = named_for(usage, mods, locks) {
        return UiKey::Named(named);
    }
    if let Some(c) = char_for_table(table, usage, mods, locks) {
        return UiKey::Char(c);
    }
    UiKey::None
}

/// [`ui_classify_table`] against the built-in US layout.
pub fn ui_classify(usage: u16, mods: Mods, locks: Locks) -> UiKey {
    ui_classify_table(&US_QWERTY, usage, mods, locks)
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
    use crate::layout_table::{Cell, ComposeEntry, LayoutTable, us_qwerty};

    const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: false,
    };
    const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: false,
    };
    const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
        meta: false,
        altgr: false,
    };
    const ALTGR: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        altgr: true,
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

    // --- existing US behavior, via the UsQwerty shim ---

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
        assert_eq!(text(km.map(KEY_KP_7, NONE, NUM_ON)), b'7' as u32);
        assert_eq!(text(km.map(KEY_KP_0, NONE, NUM_ON)), b'0' as u32);
        assert_eq!(text(km.map(KEY_KP_DOT, NONE, NUM_ON)), b'.' as u32);
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

    // --- GUI-toolkit classification ---

    #[test]
    fn char_for_ignores_ctrl() {
        assert_eq!(char_for(KEY_A, CTRL, NUM_ON), Some('a'));
        assert_eq!(char_for(KEY_A, SHIFT, NUM_ON), Some('A'));
        assert_eq!(char_for(KEY_1, SHIFT, NUM_ON), Some('!'));
    }

    #[test]
    fn ui_classify_control_keys_are_named() {
        assert_eq!(
            ui_classify(KEY_ENTER, NONE, NUM_ON),
            UiKey::Named(NamedKey::Enter)
        );
        assert_eq!(
            ui_classify(KEY_TAB, NONE, NUM_ON),
            UiKey::Named(NamedKey::Tab)
        );
        assert_eq!(
            ui_classify(KEY_SPACE, NONE, NUM_ON),
            UiKey::Named(NamedKey::Space)
        );
        assert_eq!(
            ui_classify(KEY_BACKSPACE, NONE, NUM_ON),
            UiKey::Named(NamedKey::Backspace)
        );
        assert_eq!(
            ui_classify(KEY_ESC, NONE, NUM_ON),
            UiKey::Named(NamedKey::Escape)
        );
    }

    #[test]
    fn ui_classify_ctrl_a_is_char() {
        assert_eq!(ui_classify(KEY_A, CTRL, NUM_ON), UiKey::Char('a'));
        assert_eq!(ui_classify(KEY_A, NONE, NUM_ON), UiKey::Char('a'));
        assert_eq!(ui_classify(KEY_A, SHIFT, NUM_ON), UiKey::Char('A'));
    }

    #[test]
    fn ui_classify_keypad_follows_numlock() {
        assert_eq!(ui_classify(KEY_KP_7, NONE, NUM_ON), UiKey::Char('7'));
        assert_eq!(
            ui_classify(KEY_KP_7, NONE, NUM_OFF),
            UiKey::Named(NamedKey::Home)
        );
        assert_eq!(ui_classify(KEY_KP_5, NONE, NUM_OFF), UiKey::None);
    }

    #[test]
    fn ui_classify_modifiers_are_none() {
        assert_eq!(ui_classify(KEY_LEFTCTRL, NONE, NUM_ON), UiKey::None);
        assert_eq!(ui_classify(KEY_LEFTSHIFT, NONE, NUM_ON), UiKey::None);
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
        assert_eq!(km.map(KEY_LEFTCTRL, NONE, NUM_ON), KeyOutcome::None);
        assert_eq!(km.map(KEY_RIGHTALT, NONE, NUM_ON), KeyOutcome::None);
    }

    // --- new engine: AltGr levels, dead keys, level fallback ---

    /// A tiny synthetic de_CH-ish layout for engine tests, built programmatically:
    /// QWERTZ y/z swap, AltGr on `2`/`E`, and an acute dead key on `=`.
    fn demo_layout() -> LayoutTable {
        const ACUTE: u8 = 0;
        let mut t = us_qwerty();
        t.name = [0; 16];
        t.name[..5].copy_from_slice(b"de_CH");
        // y/z swap (caps-affected letters).
        t.levels[KEY_Y as usize] = [
            Cell::literal('z' as u32),
            Cell::literal('Z' as u32),
            Cell::NONE,
            Cell::NONE,
        ];
        t.levels[KEY_Z as usize] = [
            Cell::literal('y' as u32),
            Cell::literal('Y' as u32),
            Cell::NONE,
            Cell::NONE,
        ];
        // AltGr+2 = @  (base '2', shift '"', altgr '@').
        t.levels[KEY_2 as usize] = [
            Cell::literal('2' as u32),
            Cell::literal('"' as u32),
            Cell::literal('@' as u32),
            Cell::NONE,
        ];
        // AltGr+E = €.
        t.levels[KEY_E as usize][2] = Cell::literal(0x20AC);
        // Acute dead key on '=' (base), grave on shift unused here.
        t.dead_accent[ACUTE as usize] = 0x00B4; // ´
        t.levels[KEY_EQUAL as usize] = [Cell::dead(ACUTE), Cell::NONE, Cell::NONE, Cell::NONE];
        t.compose[0] = ComposeEntry {
            dead: ACUTE,
            _pad: [0; 3],
            base: 'a' as u32,
            result: 0x00E1,
        }; // á
        t.compose[1] = ComposeEntry {
            dead: ACUTE,
            _pad: [0; 3],
            base: 'e' as u32,
            result: 0x00E9,
        }; // é
        t.num_compose = 2;
        t
    }

    #[test]
    fn altgr_selects_level3() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        assert_eq!(
            resolve(&t, KEY_2, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('@' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_E, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x20AC)
        );
        // AltGr on a key with no level-3 falls back to base.
        assert_eq!(
            resolve(&t, KEY_A, ALTGR, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('a' as u32)
        );
    }

    #[test]
    fn qwertz_y_z_swap() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        assert_eq!(
            resolve(&t, KEY_Y, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('z' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_Z, NONE, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('y' as u32)
        );
        assert_eq!(
            resolve(&t, KEY_Y, SHIFT, NUM_ON, &mut d).outcome,
            KeyOutcome::Text('Z' as u32)
        );
        // Ctrl follows the layout letter: Ctrl on the physical-Y key (labelled Z)
        // sends Ctrl-Z (0x1A), since the resolved glyph is 'z'.
        assert_eq!(
            resolve(&t, KEY_Y, CTRL, NUM_ON, &mut d).outcome,
            KeyOutcome::Text(0x1A)
        );
    }

    #[test]
    fn dead_key_composes() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        // Press the acute dead key: no output, armed.
        let r = resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d);
        assert_eq!(r.outcome, KeyOutcome::None);
        assert_eq!(r.flush, 0);
        assert_eq!(d.pending, Some(0));
        // Then 'a' → 'á'.
        let r = resolve(&t, KEY_A, NONE, NUM_ON, &mut d);
        assert_eq!(r.outcome, KeyOutcome::Text(0x00E1));
        assert_eq!(r.flush, 0);
        assert_eq!(d.pending, None);
    }

    #[test]
    fn dead_key_space_emits_bare_accent() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d); // arm acute
        let r = resolve(&t, KEY_SPACE, NONE, NUM_ON, &mut d);
        assert_eq!(r.outcome, KeyOutcome::Text(0x00B4)); // ´
        assert_eq!(d.pending, None);
    }

    #[test]
    fn dead_key_no_match_flushes_accent_then_base() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d); // arm acute
        // 's' has no acute composition: flush ´ then emit 's'.
        let r = resolve(&t, KEY_S, NONE, NUM_ON, &mut d);
        assert_eq!(r.flush, 0x00B4);
        assert_eq!(r.outcome, KeyOutcome::Text('s' as u32));
        assert_eq!(d.pending, None);
    }

    #[test]
    fn dead_key_then_named_flushes_and_passes_named() {
        let t = demo_layout();
        let mut d = DeadKeyState::new();
        resolve(&t, KEY_EQUAL, NONE, NUM_ON, &mut d); // arm acute
        let r = resolve(&t, KEY_LEFT, NONE, NUM_ON, &mut d);
        assert_eq!(r.flush, 0x00B4);
        assert_eq!(r.outcome, KeyOutcome::Named(NamedKey::Left));
        assert_eq!(d.pending, None);
    }
}
