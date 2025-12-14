#![allow(dead_code)]

use slopos_lib::{cpu, klog_debug};

unsafe extern "C" {
    fn scheduler_request_reschedule_from_interrupt();
}

const KEYBOARD_BUFFER_SIZE: usize = 256;

#[derive(Clone, Copy)]
struct KeyboardState {
    shift_left: bool,
    shift_right: bool,
    ctrl_left: bool,
    ctrl_right: bool,
    alt_left: bool,
    alt_right: bool,
    caps_lock: bool,
}

impl KeyboardState {
    const fn new() -> Self {
        Self {
            shift_left: false,
            shift_right: false,
            ctrl_left: false,
            ctrl_right: false,
            alt_left: false,
            alt_right: false,
            caps_lock: false,
        }
    }
}

#[derive(Clone, Copy)]
struct KeyboardBuffer {
    data: [u8; KEYBOARD_BUFFER_SIZE],
    head: u32,
    tail: u32,
    count: u32,
}

impl KeyboardBuffer {
    const fn new() -> Self {
        Self {
            data: [0; KEYBOARD_BUFFER_SIZE],
            head: 0,
            tail: 0,
            count: 0,
        }
    }
}

static mut KB_STATE: KeyboardState = KeyboardState::new();
static mut CHAR_BUFFER: KeyboardBuffer = KeyboardBuffer::new();
static mut SCANCODE_BUFFER: KeyboardBuffer = KeyboardBuffer::new(); // For debugging

// Base scancode map for letters (a-z) and symbols
const SCANCODE_LETTERS: [u8; 0x80] = [
    0x00, 0x00, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, // 0x00-0x07 (2-7)
    0x37, 0x38, 0x39, 0x30, 0x2D, 0x3D, 0x00, 0x09, // 0x08-0x0F (8-0, - =, Tab)
    0x71, 0x77, 0x65, 0x72, 0x74, 0x79, 0x75, 0x69, // 0x10-0x17 (q-w-e-r-t-y-u-i)
    0x6F, 0x70, 0x5B, 0x5D, 0x00, 0x00, 0x61, 0x73, // 0x18-0x1F (o-p-[-], a-s)
    0x64, 0x66, 0x67, 0x68, 0x6A, 0x6B, 0x6C, 0x3B, // 0x20-0x27 (d-f-g-h-j-k-l-;)
    0x27, 0x60, 0x00, 0x5C, 0x7A, 0x78, 0x63, 0x76, // 0x28-0x2F (', `, (Shift), \, z-x-c-v)
    0x62, 0x6E, 0x6D, 0x2C, 0x2E, 0x2F, 0x00, 0x00, // 0x30-0x37 (b-n-m-,-.-/, (unused))
    0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x38-0x3F (Space)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x40-0x47
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x48-0x4F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x50-0x57
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x58-0x5F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x60-0x67
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x68-0x6F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x70-0x77
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x78-0x7F
];

// Shifted scancode map for numbers and symbols
const SCANCODE_SHIFTED: [u8; 0x80] = [
    0x00, 0x00, 0x21, 0x40, 0x23, 0x24, 0x25, 0x5E, // 0x00-0x07 (!-@-#-$-%-^)
    0x26, 0x2A, 0x28, 0x29, 0x5F, 0x2B, 0x00, 0x00, // 0x08-0x0F (&-*-(-)-_-+)
    0x51, 0x57, 0x45, 0x52, 0x54, 0x59, 0x55, 0x49, // 0x10-0x17 (Q-W-E-R-T-Y-U-I)
    0x4F, 0x50, 0x7B, 0x7D, 0x00, 0x00, 0x41, 0x53, // 0x18-0x1F (O-P-{-}, A-S)
    0x44, 0x46, 0x47, 0x48, 0x4A, 0x4B, 0x4C, 0x3A, // 0x20-0x27 (D-F-G-H-J-K-L-:)
    0x22, 0x7E, 0x00, 0x7C, 0x5A, 0x58, 0x43, 0x56, // 0x28-0x2F ("-~-|, Z-X-C-V)
    0x42, 0x4E, 0x4D, 0x3C, 0x3E, 0x3F, 0x00, 0x00, // 0x30-0x37 (B-N-M-<- ->-?)
    0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x38-0x3F (Space)
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x40-0x47
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x48-0x4F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x50-0x57
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x58-0x5F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x60-0x67
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x68-0x6F
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x70-0x77
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 0x78-0x7F
];

#[inline]
unsafe fn kb_buffer_push_overwrite(buf: *mut KeyboardBuffer, byte: u8) {
    let buf = unsafe { &mut *buf };
    if buf.count >= KEYBOARD_BUFFER_SIZE as u32 {
        buf.tail = (buf.tail + 1) % KEYBOARD_BUFFER_SIZE as u32;
        buf.count = buf.count.saturating_sub(1);
    }
    buf.data[buf.head as usize] = byte;
    buf.head = (buf.head + 1) % KEYBOARD_BUFFER_SIZE as u32;
    buf.count = buf.count.saturating_add(1);
}

#[inline]
unsafe fn kb_buffer_pop(buf: *mut KeyboardBuffer) -> Option<u8> {
    let buf = unsafe { &mut *buf };
    let mut out = None;
    cpu::disable_interrupts();
    if buf.count > 0 {
        let byte = buf.data[buf.tail as usize];
        buf.tail = (buf.tail + 1) % KEYBOARD_BUFFER_SIZE as u32;
        buf.count = buf.count.saturating_sub(1);
        out = Some(byte);
    }
    cpu::enable_interrupts();
    out
}

#[inline]
unsafe fn kb_buffer_has_data(buf: *const KeyboardBuffer) -> bool {
    let buf = unsafe { &*buf };
    cpu::disable_interrupts();
    let has_data = buf.count > 0;
    cpu::enable_interrupts();
    has_data
}

#[inline]
fn is_break_code(scancode: u8) -> bool {
    scancode & 0x80 != 0
}

#[inline]
fn get_make_code(scancode: u8) -> u8 {
    scancode & 0x7F
}

#[inline]
fn shift_pressed() -> bool {
    unsafe { KB_STATE.shift_left || KB_STATE.shift_right }
}

#[inline]
fn caps_lock_enabled() -> bool {
    unsafe { KB_STATE.caps_lock }
}

fn translate_letter(make_code: u8) -> u8 {
    if shift_pressed() && (make_code as usize) < SCANCODE_SHIFTED.len() {
        let shifted = SCANCODE_SHIFTED[make_code as usize];
        if shifted != 0 {
            return shifted;
        }
    }

    if (make_code as usize) < SCANCODE_LETTERS.len() {
        let base_char = SCANCODE_LETTERS[make_code as usize];
        if base_char != 0 {
            if (b'a'..=b'z').contains(&base_char) {
                let should_uppercase = shift_pressed() ^ caps_lock_enabled();
                if should_uppercase {
                    return base_char - 0x20;
                }
            }
            return base_char;
        }
    }
    0
}

fn translate_scancode(scancode: u8) -> u8 {
    let make_code = get_make_code(scancode);
    match make_code {
        0x1C => b'\n',   // Enter
        0x0E => b'\x08', // Backspace
        0x39 => b' ',    // Space
        0x0F => b'\t',   // Tab
        0x01 => 0x1B,    // Escape
        _ => translate_letter(make_code),
    }
}

fn handle_modifier_key(make_code: u8, is_press: bool) {
    unsafe {
        match make_code {
            0x2A => KB_STATE.shift_left = is_press,  // Left Shift
            0x36 => KB_STATE.shift_right = is_press, // Right Shift
            0x1D => KB_STATE.ctrl_left = is_press,   // Left Ctrl
            0x38 => KB_STATE.alt_left = is_press,    // Left Alt
            0x3A => {
                if is_press {
                    KB_STATE.caps_lock = !KB_STATE.caps_lock;
                }
            }
            _ => {}
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_init() {
    unsafe {
        KB_STATE = KeyboardState::new();
        CHAR_BUFFER = KeyboardBuffer::new();
        SCANCODE_BUFFER = KeyboardBuffer::new();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_handle_scancode(scancode: u8) {
    klog_debug!("[KBD] Scancode: 0x{:02x}\n", scancode);

    let is_press = !is_break_code(scancode);
    let make_code = get_make_code(scancode);

    klog_debug!(
        "[KBD] Make code: 0x{:02x} is_press: {}",
        make_code,
        is_press as u32
    );

    unsafe { kb_buffer_push_overwrite(&raw mut SCANCODE_BUFFER, scancode) };

    if matches!(
        make_code,
        0x2A | 0x36 | 0x1D | 0x38 | 0x3A // Shift / Ctrl / Alt / Caps
    ) {
        handle_modifier_key(make_code, is_press);
        return;
    }

    if !is_press {
        return;
    }

    let ascii = translate_scancode(scancode);
    klog_debug!("[KBD] ASCII: 0x{:02x}\n", ascii);

    if ascii != 0 {
        unsafe {
            kb_buffer_push_overwrite(&raw mut CHAR_BUFFER, ascii);
        }
        klog_debug!("[KBD] Adding to buffer");
        unsafe {
            scheduler_request_reschedule_from_interrupt();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_getchar() -> u8 {
    unsafe { kb_buffer_pop(&raw mut CHAR_BUFFER).unwrap_or(0) }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_has_input() -> i32 {
    let has_data = unsafe { kb_buffer_has_data(&raw const CHAR_BUFFER) };
    if has_data {
        1
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_buffer_pending() -> i32 {
    unsafe { (CHAR_BUFFER.count > 0) as i32 }
}

#[unsafe(no_mangle)]
pub extern "C" fn keyboard_get_scancode() -> u8 {
    unsafe { kb_buffer_pop(&raw mut SCANCODE_BUFFER).unwrap_or(0) }
}
