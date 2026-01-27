use slopos_lib::{IrqMutex, RingBuffer, klog_debug};

use crate::input_event::{self, get_timestamp_ms};
use crate::ps2;
use crate::tty::tty_notify_input_ready;
use slopos_core::scheduler_request_reschedule_from_interrupt;

const BUFFER_SIZE: usize = 256;
type Buffer = RingBuffer<u8, BUFFER_SIZE>;

#[derive(Clone, Copy)]
struct ModifierState {
    shift_left: bool,
    shift_right: bool,
    ctrl_left: bool,
    alt_left: bool,
    caps_lock: bool,
}

impl ModifierState {
    const fn new() -> Self {
        Self {
            shift_left: false,
            shift_right: false,
            ctrl_left: false,
            alt_left: false,
            caps_lock: false,
        }
    }

    fn is_shift(&self) -> bool {
        self.shift_left || self.shift_right
    }
}

struct KeyboardState {
    modifiers: ModifierState,
    char_buffer: Buffer,
    scancode_buffer: Buffer,
    extended_code: bool,
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            modifiers: ModifierState::new(),
            char_buffer: Buffer::new_with(0),
            scancode_buffer: Buffer::new_with(0),
            extended_code: false,
        }
    }

    fn reset(&mut self) {
        self.modifiers = ModifierState::new();
        self.char_buffer = Buffer::new_with(0);
        self.scancode_buffer = Buffer::new_with(0);
        self.extended_code = false;
    }
}

static STATE: IrqMutex<KeyboardState> = IrqMutex::new(KeyboardState::new());

const KEY_PAGE_UP: u8 = 0x80;
const KEY_PAGE_DOWN: u8 = 0x81;

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
        _ => translate_letter(make_code, modifiers),
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
    STATE.lock().reset();
}

pub fn handle_scancode(scancode: u8) {
    klog_debug!("[KBD] Scancode: 0x{:02x}\n", scancode);

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

    let ascii = translate_scancode(scancode, &state.modifiers);
    let timestamp_ms = get_timestamp_ms();

    drop(state);
    input_event::input_route_key_event(make_code, ascii, is_press, timestamp_ms);
    let mut state = STATE.lock();

    if matches!(make_code, 0x2A | 0x36 | 0x1D | 0x38 | 0x3A) {
        handle_modifier(&mut state.modifiers, make_code, is_press);
        return;
    }

    if state.extended_code {
        state.extended_code = false;
        if !is_press {
            return;
        }
        let extended_key = match make_code {
            0x49 => KEY_PAGE_UP,
            0x51 => KEY_PAGE_DOWN,
            _ => 0,
        };
        if extended_key != 0 {
            state.char_buffer.push_overwrite(extended_key);
            drop(state);
            tty_notify_input_ready();
            scheduler_request_reschedule_from_interrupt();
        }
        return;
    }

    if !is_press {
        return;
    }

    klog_debug!("[KBD] ASCII: 0x{:02x}\n", ascii);

    if ascii != 0 {
        state.char_buffer.push_overwrite(ascii);
        klog_debug!("[KBD] Adding to buffer");
        drop(state);
        tty_notify_input_ready();
        scheduler_request_reschedule_from_interrupt();
    }
}

pub fn getchar() -> u8 {
    STATE.lock().char_buffer.try_pop().unwrap_or(0)
}

pub fn has_input() -> i32 {
    if STATE.lock().char_buffer.is_empty() {
        0
    } else {
        1
    }
}

pub fn get_scancode() -> u8 {
    STATE.lock().scancode_buffer.try_pop().unwrap_or(0)
}

pub fn poll_wait_enter() {
    use slopos_lib::cpu;
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
