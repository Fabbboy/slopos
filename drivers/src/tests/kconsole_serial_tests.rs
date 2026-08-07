//! Kernel-side tests for the diagnostic console's serial trigger.
//!
//! The trigger is a break condition followed by one key. A break cannot be
//! produced by any byte pattern, which is the whole reason it is the trigger:
//! a console reachable from the data stream would be reachable by anything
//! that can write to the line.
//!
//! Driven through the classifier rather than through a UART, because a break
//! is not something the test harness can make QEMU's stdio serial produce.

use slopos_testing::TestResult;
use slopos_testing::fail;

use crate::serial::{SerialAction, serial_console_step};

/// LSR with data ready and no error.
const DATA: u8 = 0x01;
/// LSR with data ready and the break flag set.
const DATA_BREAK: u8 = 0x11;

/// Ordinary bytes reach the line discipline untouched.
pub fn test_kcon_serial_passes_data_through() -> TestResult {
    let mut armed = false;
    for byte in [b'a', 0x00, 0x03, 0xFF] {
        match serial_console_step(DATA, byte, &mut armed, true) {
            SerialAction::Deliver(b) if b == byte => {}
            other => return fail!("byte {:#x} was not delivered: {:?}", byte, other),
        }
    }
    if armed {
        return fail!("ordinary data armed the trigger");
    }
    TestResult::Pass
}

/// A break arms, and the next byte becomes a command rather than input.
pub fn test_kcon_serial_break_arms_and_selects() -> TestResult {
    let mut armed = false;

    // The break's framing byte is not input.
    match serial_console_step(DATA_BREAK, 0x00, &mut armed, true) {
        SerialAction::Consumed => {}
        other => return fail!("the break's framing byte was not consumed: {:?}", other),
    }
    if !armed {
        return fail!("a break did not arm the trigger");
    }

    match serial_console_step(DATA, b't', &mut armed, true) {
        SerialAction::Command(b't') => {}
        other => return fail!("the key after a break was not a command: {:?}", other),
    }
    if armed {
        return fail!("the trigger stayed armed after selecting");
    }
    TestResult::Pass
}

/// The window is exactly one byte.
///
/// An operator who sends a break and then changes their mind loses one
/// keystroke, not every keystroke until they remember to disarm.
pub fn test_kcon_serial_arm_lasts_one_byte() -> TestResult {
    let mut armed = false;
    serial_console_step(DATA_BREAK, 0x00, &mut armed, true);
    serial_console_step(DATA, b'x', &mut armed, true);
    match serial_console_step(DATA, b'y', &mut armed, true) {
        SerialAction::Deliver(b'y') => TestResult::Pass,
        other => fail!(
            "the second byte after a break was still a command: {:?}",
            other
        ),
    }
}

/// A disabled serial trigger consumes the break but arms nothing.
///
/// The framing byte is still not input — it never was — but no subsequent
/// keystroke is diverted.
pub fn test_kcon_serial_trigger_can_be_disabled() -> TestResult {
    let mut armed = false;
    match serial_console_step(DATA_BREAK, 0x00, &mut armed, false) {
        SerialAction::Consumed => {}
        other => return fail!("the framing byte was delivered as input: {:?}", other),
    }
    if armed {
        return fail!("the trigger armed while disabled");
    }
    match serial_console_step(DATA, b't', &mut armed, false) {
        SerialAction::Deliver(b't') => TestResult::Pass,
        other => fail!(
            "a key was diverted while the trigger was disabled: {:?}",
            other
        ),
    }
}

/// Back-to-back breaks leave exactly one arm.
pub fn test_kcon_serial_repeated_breaks_arm_once() -> TestResult {
    let mut armed = false;
    for _ in 0..4 {
        serial_console_step(DATA_BREAK, 0x00, &mut armed, true);
    }
    if !armed {
        return fail!("repeated breaks left the trigger disarmed");
    }
    match serial_console_step(DATA, b'h', &mut armed, true) {
        SerialAction::Command(b'h') => {}
        other => {
            return fail!(
                "the key after repeated breaks was not a command: {:?}",
                other
            );
        }
    }
    if armed {
        return fail!("one key did not consume the arm");
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_kcon_serial_passes_data_through,
    suite = kconsole
);
slopos_testing::stest!(
    name = test_kcon_serial_break_arms_and_selects,
    suite = kconsole
);
slopos_testing::stest!(name = test_kcon_serial_arm_lasts_one_byte, suite = kconsole);
slopos_testing::stest!(
    name = test_kcon_serial_trigger_can_be_disabled,
    suite = kconsole
);
slopos_testing::stest!(
    name = test_kcon_serial_repeated_breaks_arm_once,
    suite = kconsole
);
