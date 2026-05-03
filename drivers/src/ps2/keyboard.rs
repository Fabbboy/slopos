use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_utils::{RingBuffer, klog_debug, klog_info, klog_warn};

use crate::input_event::{has_keyboard_focus, input_route_key_event};
use crate::ps2;
use crate::tty::vconsole;
use crate::tty::{active_tty, push_input};
use slopos_kernel_services::driver_runtime::request_reschedule_from_interrupt;

const BUFFER_SIZE: usize = 256;
type Buffer = RingBuffer<u8, BUFFER_SIZE>;

#[derive(Clone, Copy)]
struct ModifierState {
    shift_left: bool,
    shift_right: bool,
    ctrl_left: bool,
    alt_left: bool,
    caps_lock: bool,
    super_left: bool,
    super_right: bool,
}

impl ModifierState {
    const fn new() -> Self {
        Self {
            shift_left: false,
            shift_right: false,
            ctrl_left: false,
            alt_left: false,
            caps_lock: false,
            super_left: false,
            super_right: false,
        }
    }

    fn is_shift(&self) -> bool {
        self.shift_left || self.shift_right
    }

    fn is_super(&self) -> bool {
        self.super_left || self.super_right
    }
}

struct KeyboardState {
    modifiers: ModifierState,
    scancode_buffer: Buffer,
    extended_code: bool,
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            modifiers: ModifierState::new(),
            scancode_buffer: Buffer::new_with(0),
            extended_code: false,
        }
    }

    fn reset(&mut self) {
        self.modifiers = ModifierState::new();
        self.scancode_buffer = Buffer::new_with(0);
        self.extended_code = false;
    }
}

static STATE: SpinLock<KeyboardState> = SpinLock::new(KeyboardState::new(), LOCK_LEVEL_RESOURCE);

const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;
const KEY_UP: u8 = 0x82;
const KEY_DOWN: u8 = 0x83;
const KEY_LEFT: u8 = 0x84;
const KEY_RIGHT: u8 = 0x85;
const KEY_HOME: u8 = 0x86;
const KEY_END: u8 = 0x87;
const KEY_DELETE: u8 = 0x88;

const KEY_SHIFT_LEFT: u8 = 0x94;
const KEY_SHIFT_RIGHT: u8 = 0x95;
const KEY_SHIFT_HOME: u8 = 0x96;
const KEY_SHIFT_END: u8 = 0x97;

const SCANCODE_LETTERS: [u8; 0x80] = [
    0x00, 0x00, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30, 0x2D, 0x3D, 0x00, 0x09,
    0x71, 0x77, 0x65, 0x72, 0x74, 0x79, 0x75, 0x69, 0x6F, 0x70, 0x5B, 0x5D, 0x00, 0x00, 0x61, 0x73,
    0x64, 0x66, 0x67, 0x68, 0x6A, 0x6B, 0x6C, 0x3B, 0x27, 0x60, 0x00, 0x5C, 0x7A, 0x78, 0x63, 0x76,
    0x62, 0x6E, 0x6D, 0x2C, 0x2E, 0x2F, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

const SCANCODE_SHIFTED: [u8; 0x80] = [
    0x00, 0x00, 0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, 0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x00, 0x00,
    0x51, 0x57, 0x45, 0x52, 0x54, 0x59, 0x55, 0x49, 0x4F, 0x50, 0x7B, 0x7D, 0x00, 0x00, 0x41, 0x53,
    0x44, 0x46, 0x47, 0x48, 0x4A, 0x4B, 0x4C, 0x3A, 0x22, 0x7E, 0x00, 0x7C, 0x5A, 0x58, 0x43, 0x56,
    0x42, 0x4E, 0x4D, 0x3C, 0x3E, 0x3F, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

#[inline(always)]
fn is_break_code(scancode: u8) -> bool {
    scancode & 0x80 != 0
}

#[inline(always)]
fn get_make_code(scancode: u8) -> u8 {
    scancode & 0x7F
}

fn translate_letter(make_code: u8, modifiers: &ModifierState) -> u8 {
    let shift = modifiers.is_shift();
    let caps = modifiers.caps_lock;

    if shift && (make_code as usize) < SCANCODE_SHIFTED.len() {
        let shifted = SCANCODE_SHIFTED[make_code as usize];
        if shifted != 0 {
            return shifted;
        }
    }

    if (make_code as usize) < SCANCODE_LETTERS.len() {
        let base_char = SCANCODE_LETTERS[make_code as usize];
        if base_char != 0 {
            if (b'a'..=b'z').contains(&base_char) {
                let should_uppercase = shift ^ caps;
                if should_uppercase {
                    return base_char - 0x20;
                }
            }
            return base_char;
        }
    }
    0
}

fn translate_scancode(scancode: u8, modifiers: &ModifierState) -> u8 {
    let make_code = get_make_code(scancode);
    match make_code {
        0x1C => b'\n',
        0x0E => b'\x08',
        0x39 => b' ',
        0x0F => b'\t',
        0x01 => 0x1B,
        _ => {
            let ch = translate_letter(make_code, modifiers);
            // Ctrl+letter → control code (0x01–0x1A)
            if modifiers.ctrl_left && ch != 0 {
                let lower = if (b'A'..=b'Z').contains(&ch) {
                    ch + 0x20
                } else {
                    ch
                };
                if (b'a'..=b'z').contains(&lower) {
                    return lower - b'a' + 1;
                }
            }
            ch
        }
    }
}

fn handle_modifier(modifiers: &mut ModifierState, make_code: u8, is_press: bool) {
    match make_code {
        0x2A => modifiers.shift_left = is_press,
        0x36 => modifiers.shift_right = is_press,
        0x1D => modifiers.ctrl_left = is_press,
        0x38 => modifiers.alt_left = is_press,
        0x3A => {
            if is_press {
                modifiers.caps_lock = !modifiers.caps_lock;
            }
        }
        _ => {}
    }
}

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
    STATE.lock().reset();
    klog_info!("PS/2 keyboard: initialised");
}

pub fn handle_scancode(scancode: u8) {
    klog_debug!("[KBD] Scancode: 0x{:02x}", scancode);

    let mut state = STATE.lock();

    if scancode == 0xE0 {
        state.extended_code = true;
        return;
    }

    let is_press = !is_break_code(scancode);
    let make_code = get_make_code(scancode);

    klog_debug!(
        "[KBD] Make code: 0x{:02x} is_press: {}",
        make_code,
        is_press as u32
    );

    state.scancode_buffer.push_overwrite(scancode);

    // Modifier keys: update state and route the event (no character to deliver).
    if matches!(make_code, 0x2A | 0x36 | 0x1D | 0x38 | 0x3A) {
        handle_modifier(&mut state.modifiers, make_code, is_press);
        drop(state);
        input_route_key_event(scancode, 0, is_press, slopos_utils::clock::uptime_ms());
        return;
    }

    // Extended keys (preceded by 0xE0).
    if state.extended_code {
        state.extended_code = false;
        // Super (Logo/Windows) keys: track press AND release for modifier state.
        // Left Super = 0x5B, Right Super = 0x5C (PS/2 scan code set 1).
        match make_code {
            0x5B => {
                state.modifiers.super_left = is_press;
                drop(state);
                input_route_key_event(scancode, 0, is_press, slopos_utils::clock::uptime_ms());
                return;
            }
            0x5C => {
                state.modifiers.super_right = is_press;
                drop(state);
                input_route_key_event(scancode, 0, is_press, slopos_utils::clock::uptime_ms());
                return;
            }
            _ => {}
        }
        if !is_press {
            return;
        }
        let shift = state.modifiers.is_shift();
        let extended_key = match make_code {
            0x48 => KEY_UP,
            0x50 => KEY_DOWN,
            0x4B => {
                if shift {
                    KEY_SHIFT_LEFT
                } else {
                    KEY_LEFT
                }
            }
            0x4D => {
                if shift {
                    KEY_SHIFT_RIGHT
                } else {
                    KEY_RIGHT
                }
            }
            0x47 => {
                if shift {
                    KEY_SHIFT_HOME
                } else {
                    KEY_HOME
                }
            }
            0x4F => {
                if shift {
                    KEY_SHIFT_END
                } else {
                    KEY_END
                }
            }
            0x53 => KEY_DELETE,
            0x49 => {
                if shift {
                    drop(state);
                    vconsole::scroll_view_up(12);
                    return;
                }
                KEY_PAGE_UP
            }
            0x51 => {
                if shift {
                    drop(state);
                    vconsole::scroll_view_down(12);
                    return;
                }
                KEY_PAGE_DOWN
            }
            _ => 0,
        };
        if extended_key != 0 {
            drop(state);
            let ts = slopos_utils::clock::uptime_ms();
            input_route_key_event(scancode, extended_key, true, ts);
            if !has_keyboard_focus() {
                push_input(active_tty(), extended_key);
                request_reschedule_from_interrupt();
            }
        }
        return;
    }

    if !is_press {
        drop(state);
        input_route_key_event(make_code, 0, false, slopos_utils::clock::uptime_ms());
        return;
    }

    let ascii = translate_scancode(scancode, &state.modifiers);
    drop(state);

    let ts = slopos_utils::clock::uptime_ms();
    input_route_key_event(make_code, ascii, true, ts);

    if ascii != 0 && !has_keyboard_focus() {
        push_input(active_tty(), ascii);
        request_reschedule_from_interrupt();
    }
}

pub fn get_scancode() -> u8 {
    STATE.lock().scancode_buffer.try_pop().unwrap_or(0)
}

/// Return the current keyboard modifier state as a bitfield.
///
/// Bit layout matches `slopos_abi::input::MODIFIER_*` constants:
///   bit 0 = Shift, bit 1 = Ctrl, bit 2 = Alt,
///   bit 3 = Super (Logo), bit 4 = Caps Lock.
pub fn get_modifier_state() -> u8 {
    let state = STATE.lock();
    let m = &state.modifiers;
    let mut bits: u8 = 0;
    if m.is_shift() {
        bits |= 1;
    }
    if m.ctrl_left {
        bits |= 1 << 1;
    }
    if m.alt_left {
        bits |= 1 << 2;
    }
    if m.is_super() {
        bits |= 1 << 3;
    }
    if m.caps_lock {
        bits |= 1 << 4;
    }
    bits
}

pub fn poll_wait_enter() {
    use slopos_arch::cpu;
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
