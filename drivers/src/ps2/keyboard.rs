//! PS/2 keyboard driver — a thin stateful adapter over `keymap-core`.
//!
//! The IRQ handler ([`handle_scancode`]) feeds raw set-1 scancode bytes into a
//! [`Set1Decoder`], folds modifier/lock state with a [`ModTracker`], and asks
//! the [`UsQwerty`] layout for the produced text / named key. Each event is
//! published carrying **both** the legacy `(scancode, ascii)` bytes (so the
//! compositor, terminal, and TTY line discipline keep working unchanged) and
//! the canonical `(keycode, codepoint, modifiers, flags)` payload.
//!
//! All keyboard *logic* — scancode tables, layout, the numeric-keypad + Num
//! Lock behavior — lives in the host-tested `keymap-core` crate; this module is
//! just the kernel glue (locking, IRQ routing, the TTY fallback, LED I/O, and
//! the legacy-byte bridge).

use slopos_arch::cpu;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{klog_info, klog_warn};

use slopos_abi::input::{KEY_FLAG_FROM_KEYPAD, KEY_FLAG_HAS_CANONICAL};
use slopos_keymap_core::keycode::{self, NamedKey};
use slopos_keymap_core::keymap::{KeyOutcome, Layout, UsQwerty};
use slopos_keymap_core::{LOCK_CAPS, LOCK_NUM, LOCK_SCROLL, ModSnapshot, ModTracker, Set1Decoder};

use crate::input_event::{has_keyboard_focus, input_route_key_full};
use crate::ps2;
use crate::tty::vconsole;
use crate::tty::{active_tty, push_input};
use slopos_kernel_services::driver_runtime::request_reschedule_from_interrupt;

/// Keyboard device command: set the lock LEDs (followed by a 1-byte LED mask).
const DEV_CMD_SET_LEDS: u8 = 0xED;
/// Bounded poll budget when waiting for a device ACK during LED I/O.
const ACK_WAIT_ITERS: u32 = 50_000;

/// All keyboard state behind one lock: the byte-stream decoder and the
/// modifier/lock tracker.
struct KeyboardState {
    decoder: Set1Decoder,
    mods: ModTracker,
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            decoder: Set1Decoder::new(),
            mods: ModTracker::new(),
        }
    }
}

static STATE: SpinLock<KeyboardState> = SpinLock::new(KeyboardState::new(), LOCK_LEVEL_RESOURCE);

pub fn init() {
    klog_info!("PS/2 keyboard: initialising device");

    ps2::write_data(ps2::DEV_CMD_RESET);
    if ps2::wait_data() {
        let response = ps2::read_data_nowait();
        if response == ps2::DEV_ACK {
            if ps2::wait_data() {
                let test_result = ps2::read_data_nowait();
                if test_result != ps2::DEV_SELF_TEST_PASS {
                    klog_warn!("PS/2 keyboard: self-test returned 0x{:02x}", test_result);
                }
            }
        } else {
            klog_warn!("PS/2 keyboard: reset NAK 0x{:02x}", response);
        }
    } else {
        klog_warn!("PS/2 keyboard: reset timed out");
    }

    ps2::flush();

    let snap = {
        let mut state = STATE.lock();
        state.decoder = Set1Decoder::new();
        state.mods = ModTracker::new();
        state.mods.snapshot()
    };
    // Reflect the initial lock state (Num Lock starts on) on the physical LEDs.
    // Called here with IRQs still masked, so the ACK exchange is race-free.
    set_leds(snap);

    klog_info!("PS/2 keyboard: initialised");
}

/// IRQ entry point: process one raw scancode byte from the controller.
pub fn handle_scancode(byte: u8) {
    let mut state = STATE.lock();

    let step = match state.decoder.feed(byte) {
        Some(step) => step,
        None => return, // prefix byte, fake-shift, swallowed Pause, or unknown
    };
    let usage = step.usage;
    let pressed = step.pressed;
    // Legacy scancode = the set-1 make code (low 7 bits). For E0-prefixed keys
    // this is the bare code (e.g. 0x48 for Up).
    let legacy_scancode = byte & 0x7F;

    // ESC toggles the on-screen fblog console during boot (press only).
    // `handle_esc_press` is a pure atomic flip, safe to call under the lock.
    if usage == keycode::KEY_ESC && pressed && slopos_ostd::fblog::handle_esc_press() {
        return;
    }

    // Fold modifier / lock state; capture snapshots before releasing the lock.
    let is_mod_or_lock = state.mods.update(usage, pressed);
    let mods = state.mods.mods();
    let locks = state.mods.locks();
    let snap = state.mods.snapshot();
    let lock_toggled = pressed
        && matches!(
            usage,
            keycode::KEY_CAPSLOCK | keycode::KEY_NUMLOCK | keycode::KEY_SCROLLLOCK
        );
    drop(state);

    // Kernel vconsole scrollback: Shift+PageUp / Shift+PageDown page the
    // on-screen log and are consumed (no app event), preserving legacy behavior.
    if pressed && mods.shift && matches!(usage, keycode::KEY_PAGEUP | keycode::KEY_PAGEDOWN) {
        if usage == keycode::KEY_PAGEUP {
            vconsole::scroll_view_up(12);
        } else {
            vconsole::scroll_view_down(12);
        }
        return;
    }

    // Refresh the lock LEDs after a Caps/Num/Scroll toggle.
    if lock_toggled {
        set_leds(snap);
    }

    // Modifier / lock keys produce no text; everything else maps through the
    // layout.
    let outcome = if is_mod_or_lock {
        KeyOutcome::None
    } else {
        UsQwerty.map(usage, mods, locks)
    };
    let (ascii, codepoint) = legacy_and_canonical(outcome, pressed);

    let mut flags = KEY_FLAG_HAS_CANONICAL;
    if keycode::is_keypad(usage) {
        flags |= KEY_FLAG_FROM_KEYPAD;
    }

    let ts = slopos_kernel_services::clock::uptime_ms();
    input_route_key_full(
        legacy_scancode,
        ascii,
        usage,
        codepoint,
        snap.mods,
        flags,
        pressed,
        ts,
    );

    // TTY fallback: when nothing holds keyboard focus, feed the active TTY the
    // produced byte (printable, control, or nav pseudo-code) on press.
    if pressed && ascii != 0 && !has_keyboard_focus() {
        push_input(active_tty(), ascii);
        request_reschedule_from_interrupt();
    }
}

/// Derive the legacy `ascii` byte and the canonical `codepoint` from a keymap
/// outcome. Releases carry no text.
fn legacy_and_canonical(outcome: KeyOutcome, pressed: bool) -> (u8, u32) {
    if !pressed {
        return (0, 0);
    }
    match outcome {
        KeyOutcome::Text(cp) => {
            let ascii = if cp <= 0xFF { cp as u8 } else { 0 };
            (ascii, cp)
        }
        KeyOutcome::Named(nk) => (named_to_legacy_ascii(nk), 0),
        KeyOutcome::None => (0, 0),
    }
}

/// Map a navigation named key to the SlopOS legacy `ascii` pseudo-code that the
/// terminal/shell decode into ANSI escape sequences. Non-navigation named keys
/// (function keys, locks, …) have no legacy byte and yield 0; consumers that
/// want them read the canonical keycode instead.
fn named_to_legacy_ascii(nk: NamedKey) -> u8 {
    match nk {
        NamedKey::PageUp => 0x80,
        NamedKey::PageDown => 0x81,
        NamedKey::Up => 0x82,
        NamedKey::Down => 0x83,
        NamedKey::Left => 0x84,
        NamedKey::Right => 0x85,
        NamedKey::Home => 0x86,
        NamedKey::End => 0x87,
        NamedKey::Delete => 0x88,
        _ => 0,
    }
}

/// Program the keyboard's lock LEDs to match `snap`.
///
/// Best-effort and bounded: sends `0xED` + the LED mask, polling for each ACK.
/// Safe to call with the state lock released (it takes no locks) and at init
/// (IRQs masked). At runtime it runs in the IRQ handler with IRQs off; lock
/// keys are pressed in isolation, so the tiny window where a concurrent
/// scancode could be mistaken for the ACK is negligible.
fn set_leds(snap: ModSnapshot) {
    let mut led = 0u8;
    if snap.locks & LOCK_SCROLL != 0 {
        led |= 0b001;
    }
    if snap.locks & LOCK_NUM != 0 {
        led |= 0b010;
    }
    if snap.locks & LOCK_CAPS != 0 {
        led |= 0b100;
    }

    ps2::write_data(DEV_CMD_SET_LEDS);
    if !wait_ack() {
        return;
    }
    ps2::write_data(led);
    let _ = wait_ack();
}

/// Bounded poll for a device ACK (0xFA). Stray bytes are discarded.
fn wait_ack() -> bool {
    for _ in 0..ACK_WAIT_ITERS {
        if ps2::has_data() {
            if ps2::read_data_nowait() == ps2::DEV_ACK {
                return true;
            }
        } else {
            cpu::pause();
        }
    }
    false
}

/// Return the current keyboard modifier state as a `MODIFIER_*` bitfield.
pub fn get_modifier_state() -> u8 {
    STATE.lock().mods.snapshot().mods
}

/// Reset decoder + modifier/lock state to defaults, without device I/O.
///
/// Test-only: keyboard state is a global shared with the live IRQ handler, so
/// tests reset it before and after to avoid cross-test contamination (a stuck
/// `E0` latch or held modifier) leaking into other tests or the live desktop.
#[cfg(feature = "test-hooks")]
pub fn reset_state_for_test() {
    let mut state = STATE.lock();
    state.decoder = Set1Decoder::new();
    state.mods = ModTracker::new();
}

pub fn poll_wait_enter() {
    const ENTER_MAKE_CODE: u8 = 0x1C;

    loop {
        if ps2::has_data() {
            let scancode = ps2::read_data_nowait();
            if scancode == ENTER_MAKE_CODE {
                break;
            }
        }
        cpu::pause();
    }
}
