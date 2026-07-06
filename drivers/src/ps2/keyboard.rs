//! PS/2 keyboard driver — a thin stateful adapter over `keymap-core`.
//!
//! The IRQ handler ([`handle_scancode`]) feeds raw set-1 scancode bytes into a
//! [`Set1Decoder`], folds modifier/lock state with a [`ModTracker`], and asks
//! the **active layout** (a runtime-swappable [`LayoutTable`], defaulting to the
//! built-in US-QWERTY) for the produced text / named key, running the dead-key
//! compose state machine. Each event is published carrying **both** the legacy
//! `(scancode, ascii)` bytes (so the compositor, terminal, and TTY line
//! discipline keep working unchanged) and the canonical `(keycode, codepoint,
//! modifiers, flags)` payload.
//!
//! All keyboard *logic* — scancode tables, layout resolution, AltGr levels, the
//! numeric-keypad + Num Lock behavior, dead-key composition — lives in the
//! host-tested `keymap-core` crate; this module is just the kernel glue (locking,
//! IRQ routing, the active-layout swap, the TTY fallback, LED I/O, and the
//! legacy-byte bridge).

use slopos_arch::cpu;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBox, klog_info, klog_warn};

use slopos_abi::Errno;
use slopos_abi::input::{KEY_FLAG_FROM_KEYPAD, KEY_FLAG_HAS_CANONICAL};
use slopos_keymap_core::keycode::{self, NamedKey};
use slopos_keymap_core::keymap::KeyOutcome;
use slopos_keymap_core::{
    DeadKeyState, LOCK_CAPS, LOCK_NUM, LOCK_SCROLL, LayoutTable, ModSnapshot, ModTracker, Resolved,
    SERIALIZED_LEN, Set1Decoder, US_QWERTY, deserialize, resolve,
};
use slopos_mm::user_io_buf::memdup_user;

use crate::input_event::{has_keyboard_focus, input_route_key_full};
use crate::ps2;
use crate::tty::vconsole;
use crate::tty::{active_tty, push_input};
use slopos_kernel_services::driver_runtime::request_reschedule_from_interrupt;

/// Keyboard device command: set the lock LEDs (followed by a 1-byte LED mask).
const DEV_CMD_SET_LEDS: u8 = 0xED;
/// Bounded poll budget when waiting for a device ACK during LED I/O.
const ACK_WAIT_ITERS: u32 = 50_000;

/// All keyboard state behind one lock: the byte-stream decoder, the
/// modifier/lock tracker, the dead-key compose state, and the active layout
/// (`None` ⇒ the built-in [`US_QWERTY`]).
struct KeyboardState {
    decoder: Set1Decoder,
    mods: ModTracker,
    dead: DeadKeyState,
    layout: Option<KBox<LayoutTable>>,
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            decoder: Set1Decoder::new(),
            mods: ModTracker::new(),
            dead: DeadKeyState::new(),
            layout: None,
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
        state.dead = DeadKeyState::new();
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

    // Resolve through the active layout while the lock is held (pure + fast: no
    // allocation, no blocking). Modifier/lock keys and releases produce no text;
    // only a fresh press runs the layout + dead-key state machine.
    let resolved = if pressed && !is_mod_or_lock {
        let st = &mut *state;
        let table = st.layout.as_deref().unwrap_or(&US_QWERTY);
        resolve(table, usage, mods, locks, &mut st.dead)
    } else {
        Resolved::none()
    };
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

    let ts = slopos_kernel_services::clock::uptime_ms();

    // Dead-key flush: a pending accent that did not compose with this key is
    // emitted as its own text event, ahead of the key's own event.
    if resolved.flush != 0 {
        route_text(resolved.flush, snap.mods, ts);
    }

    let (ascii, codepoint) = legacy_and_canonical(resolved.outcome, pressed);

    let mut flags = KEY_FLAG_HAS_CANONICAL;
    if keycode::is_keypad(usage) {
        flags |= KEY_FLAG_FROM_KEYPAD;
    }

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
    // produced byte(s) on press (the flushed accent first, then the key).
    if pressed && !has_keyboard_focus() {
        let flush_byte = if resolved.flush != 0 && resolved.flush <= 0x7F {
            resolved.flush as u8
        } else {
            0
        };
        if flush_byte != 0 {
            push_input(active_tty(), flush_byte);
        }
        if ascii != 0 {
            push_input(active_tty(), ascii);
        }
        if flush_byte != 0 || ascii != 0 {
            request_reschedule_from_interrupt();
        }
    }
}

/// Route a bare text codepoint as a synthetic key-press event (used for the
/// dead-key accent flush; carries no canonical keycode).
fn route_text(codepoint: u32, modifiers: u8, ts: u64) {
    let ascii = if codepoint <= 0x7F {
        codepoint as u8
    } else {
        0
    };
    input_route_key_full(
        0,
        ascii,
        0,
        codepoint,
        modifiers,
        KEY_FLAG_HAS_CANONICAL,
        true,
        ts,
    );
}

/// Derive the legacy `ascii` byte and the canonical `codepoint` from a keymap
/// outcome. Releases carry no text. The legacy byte is ASCII-only: non-ASCII
/// text (umlauts, accents) travels solely as the canonical codepoint — a raw
/// Latin-1 byte on the TTY/PTY byte stream would be mojibake to UTF-8
/// consumers, and 0x80..=0x88 are reserved as nav pseudo-codes.
fn legacy_and_canonical(outcome: KeyOutcome, pressed: bool) -> (u8, u32) {
    if !pressed {
        return (0, 0);
    }
    match outcome {
        KeyOutcome::Text(cp) => {
            let ascii = if cp <= 0x7F { cp as u8 } else { 0 };
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

/// Install a new active layout, replacing the previous one. The old layout is
/// freed **outside** the lock: a heap free can trigger a cross-CPU TLB/LUF drain
/// that must not run while the keyboard `SpinLock` is held.
pub fn set_layout(layout: KBox<LayoutTable>) {
    let old = {
        let mut state = STATE.lock();
        state.dead.reset(); // a layout swap invalidates any pending dead key
        state.layout.replace(layout)
    };
    drop(old);
}

/// Deserialise + validate a serialised `LayoutTable` blob from the calling
/// task's user memory and install it as the active layout.
///
/// The kernel only ever ingests the **binary** form here (no text parsing): the
/// blob is bounded, copied via `memdup_user`, then `deserialize` runs
/// `keymap-core`'s validator before the table is installed.
pub fn load_layout_from_user(data_ptr: u64, len: usize) -> Result<(), Errno> {
    if data_ptr == 0 || len != SERIALIZED_LEN {
        return Err(Errno::EINVAL);
    }
    let bytes = memdup_user(data_ptr, len, SERIALIZED_LEN)?;
    let mut layout = KBox::<LayoutTable>::zeroed().map_err(|_| Errno::ENOMEM)?;
    deserialize(bytes.as_slice(), &mut layout).map_err(|_| Errno::EINVAL)?;
    set_layout(layout);
    Ok(())
}

/// Write the active layout's short name into the kernel buffer `out`; return the
/// number of bytes written. The caller (the syscall handler) copies `out` to
/// user memory through the SMAP-safe path — this never touches user pages.
pub fn layout_name(out: &mut [u8]) -> usize {
    let state = STATE.lock();
    let table = state.layout.as_deref().unwrap_or(&US_QWERTY);
    let bytes = table.name_str().as_bytes();
    let n = bytes.len().min(out.len());
    out[..n].copy_from_slice(&bytes[..n]);
    n
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

/// Reset decoder + modifier/lock + dead-key state and the active layout to
/// defaults, without device I/O.
///
/// Test-only: keyboard state is a global shared with the live IRQ handler, so
/// tests reset it before and after to avoid cross-test contamination (a stuck
/// `E0` latch, a held modifier, a pending dead key, or a loaded layout) leaking
/// into other tests or the live desktop.
#[cfg(feature = "test-hooks")]
pub fn reset_state_for_test() {
    let old = {
        let mut state = STATE.lock();
        state.decoder = Set1Decoder::new();
        state.mods = ModTracker::new();
        state.dead = DeadKeyState::new();
        state.layout.take()
    };
    drop(old);
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
