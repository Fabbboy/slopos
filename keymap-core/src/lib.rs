//! Pure, layout-independent keyboard core, free of `alloc`, `unsafe` and
//! platform globals so it is host-testable.
//!
//! A keyboard backend decodes its raw byte protocol into canonical [`keycode`]
//! usages with [`scancode_set1::Set1Decoder`], folds modifier/lock state with
//! [`modifiers::ModTracker`], and asks a [`keymap::Layout`] for the produced
//! text / named key. Auto-repeat timing lives in [`repeat::KeyRepeat`].

#![no_std]
#![forbid(unsafe_code)]

pub mod keymap;
pub mod layout_table;
pub mod modifiers;
pub mod parse;
pub mod repeat;
pub mod scancode_set1;
pub mod sysrq;

pub use keymap::{
    DeadKeyState, KeyOutcome, Layout, Locks, Mods, Resolved, UiKey, UsQwerty, char_for,
    char_for_table, named_for, resolve, ui_classify, ui_classify_table,
};
pub use layout_table::{
    Cell, CellKind, ComposeEntry, LAYOUT_MAGIC, LAYOUT_NAME_LEN, LAYOUT_VERSION, LayoutError,
    LayoutTable, MAX_COMPOSE, MAX_DEADKEYS, NUM_KEYS, NUM_LEVELS, SERIALIZED_LEN, US_QWERTY,
    deserialize, is_layout_dependent, serialize, us_qwerty, validate,
};
pub use modifiers::{
    LOCK_CAPS, LOCK_NUM, LOCK_SCROLL, ModSnapshot, ModTracker, mods_locks_from_raw,
};
pub use parse::parse;
pub use repeat::{KeyRepeat, REPEAT_DELAY_MS, REPEAT_INTERVAL_MS};
pub use scancode_set1::{DecodeStep, Set1Decoder};

/// The canonical keycode vocabulary (HID Keyboard/Keypad usages + `NamedKey`).
pub use slopos_abi::input::keycode;
