//! Keyboard adapter integration tests: synthetic scancodes → focused task's
//! input queue, asserting the canonical `(keycode, codepoint, modifiers)`
//! payload alongside the legacy `(scancode, ascii)` bytes.

use slopos_abi::input::{InputEventType, KEY_FLAG_FROM_KEYPAD, MODIFIER_SHIFT};
use slopos_keymap_core::keycode;
use slopos_testing::{TestResult, fail, pass};

use crate::input_event::{input_cleanup_task, input_poll, input_set_keyboard_focus};
use crate::ps2::keyboard::{handle_scancode, reset_state_for_test};

const DUMMY_TASK: u32 = 9001;

fn setup() {
    reset_state_for_test();
    input_cleanup_task(DUMMY_TASK);
    input_set_keyboard_focus(DUMMY_TASK);
    while input_poll(DUMMY_TASK).is_some() {}
}

fn teardown() {
    input_set_keyboard_focus(0);
    input_cleanup_task(DUMMY_TASK);
    reset_state_for_test();
}

/// KP7 with Num Lock on (the default) produces the digit '7'.
pub fn test_keypad_numlock_digit() -> TestResult {
    setup();
    handle_scancode(0x47); // KP7 make code
    let ev = input_poll(DUMMY_TASK);
    teardown();

    let ev = match ev {
        Some(e) => e,
        None => return fail!("keypad 7 produced no event (the original bug)"),
    };
    if ev.key_keycode() != keycode::KEY_KP_7 {
        return fail!("keypad 7 decoded to the wrong canonical keycode");
    }
    if ev.key_codepoint() != b'7' as u32 {
        return fail!("keypad 7 did not produce digit '7' with Num Lock on");
    }
    if ev.key_ascii() != b'7' {
        return fail!("keypad 7 legacy ascii is not '7'");
    }
    if ev.key_flags() & KEY_FLAG_FROM_KEYPAD == 0 {
        return fail!("keypad 7 missing FROM_KEYPAD flag");
    }
    pass!()
}

pub fn test_letter_canonical_payload() -> TestResult {
    setup();
    handle_scancode(0x1E); // 'a' make code
    let ev = input_poll(DUMMY_TASK);
    teardown();

    let ev = match ev {
        Some(e) => e,
        None => return fail!("letter 'a' produced no event"),
    };
    if ev.key_keycode() != keycode::KEY_A {
        return fail!("letter 'a' wrong canonical keycode");
    }
    if ev.key_codepoint() != b'a' as u32 || ev.key_ascii() != b'a' {
        return fail!("letter 'a' wrong codepoint/ascii");
    }
    pass!()
}

pub fn test_shift_letter_uppercase_and_modifier() -> TestResult {
    setup();
    handle_scancode(0x2A); // Left Shift press
    handle_scancode(0x1E); // 'a' -> 'A'
    let _shift_ev = input_poll(DUMMY_TASK);
    let ev = input_poll(DUMMY_TASK);
    teardown();

    let ev = match ev {
        Some(e) => e,
        None => return fail!("shift+'a' produced no letter event"),
    };
    if ev.key_keycode() != keycode::KEY_A {
        return fail!("shift+'a' wrong keycode");
    }
    if ev.key_codepoint() != b'A' as u32 || ev.key_ascii() != b'A' {
        return fail!("shift+'a' did not produce uppercase 'A'");
    }
    if ev.key_modifiers() & MODIFIER_SHIFT == 0 {
        return fail!("shift+'a' missing Shift in modifier snapshot");
    }
    pass!()
}

/// Extended arrows still carry the legacy 0x82 pseudo-code the terminal/shell
/// expect.
pub fn test_extended_arrow_legacy_and_canonical() -> TestResult {
    setup();
    handle_scancode(0xE0); // extended prefix
    handle_scancode(0x48); // Up arrow
    let ev = input_poll(DUMMY_TASK);
    teardown();

    let ev = match ev {
        Some(e) => e,
        None => return fail!("Up arrow produced no event"),
    };
    if ev.key_keycode() != keycode::KEY_UP {
        return fail!("Up arrow wrong canonical keycode");
    }
    if ev.key_ascii() != 0x82 {
        return fail!("Up arrow legacy pseudo-code is not 0x82");
    }
    pass!()
}

pub fn test_press_then_release_events() -> TestResult {
    setup();
    handle_scancode(0x1E); // 'a' press
    handle_scancode(0x9E); // 'a' release
    let press = input_poll(DUMMY_TASK);
    let release = input_poll(DUMMY_TASK);
    teardown();

    let press = match press {
        Some(e) => e,
        None => return fail!("missing press event"),
    };
    if press.event_type != InputEventType::KeyPress || press.key_codepoint() != b'a' as u32 {
        return fail!("press event malformed");
    }
    let release = match release {
        Some(e) => e,
        None => return fail!("missing release event"),
    };
    if release.event_type != InputEventType::KeyRelease {
        return fail!("expected KeyRelease");
    }
    if release.key_keycode() != keycode::KEY_A {
        return fail!("release missing canonical keycode");
    }
    if release.key_ascii() != 0 {
        return fail!("release should carry no text byte");
    }
    pass!()
}

/// `handle_scancode` resolves against the layout loaded at runtime, not the
/// hardcoded default.
pub fn test_runtime_layout_swap() -> TestResult {
    use crate::ps2::keyboard::set_layout;
    use slopos_keymap_core::{LayoutTable, parse};
    use slopos_ostd::KBox;

    const DE_CH: &str = include_str!("../../../assets/keymaps/de_CH.layout");

    setup();

    let mut table = match KBox::<LayoutTable>::zeroed() {
        Ok(t) => t,
        Err(_) => {
            teardown();
            return fail!("layout alloc failed");
        }
    };
    if parse(DE_CH.as_bytes(), &mut table).is_err() {
        teardown();
        return fail!("de_CH layout failed to parse");
    }
    set_layout(table);

    // Physical-Y key (set-1 make 0x15) → 'z' under QWERTZ.
    handle_scancode(0x15);
    let z_ev = input_poll(DUMMY_TASK);

    // AltGr (right Alt = E0 38) + '2' (set-1 0x03) → '@'.
    handle_scancode(0xE0);
    handle_scancode(0x38);
    let _altgr_press = input_poll(DUMMY_TASK);
    handle_scancode(0x03);
    let at_ev = input_poll(DUMMY_TASK);
    handle_scancode(0xE0);
    handle_scancode(0xB8); // right Alt release

    teardown();

    let z = match z_ev {
        Some(e) => e,
        None => return fail!("de_CH physical-Y produced no event"),
    };
    if z.key_codepoint() != 'z' as u32 {
        return fail!("de_CH physical-Y did not produce 'z' (QWERTZ swap)");
    }
    let at = match at_ev {
        Some(e) => e,
        None => return fail!("de_CH AltGr+2 produced no event"),
    };
    if at.key_codepoint() != '@' as u32 {
        return fail!("de_CH AltGr+2 did not produce '@'");
    }
    pass!()
}

slopos_testing::stest!(name = test_runtime_layout_swap, suite = keyboard);
slopos_testing::stest!(name = test_keypad_numlock_digit, suite = keyboard);
slopos_testing::stest!(name = test_letter_canonical_payload, suite = keyboard);
slopos_testing::stest!(
    name = test_shift_letter_uppercase_and_modifier,
    suite = keyboard
);
slopos_testing::stest!(
    name = test_extended_arrow_legacy_and_canonical,
    suite = keyboard
);
slopos_testing::stest!(name = test_press_then_release_events, suite = keyboard);
