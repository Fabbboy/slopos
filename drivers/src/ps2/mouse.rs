use slopos_sync::IrqMutex;
use slopos_utils::klog_info;

use crate::input_event::{self, get_timestamp_ms};
use crate::ps2;

pub const BUTTON_LEFT: u8 = 0x01;
pub const BUTTON_RIGHT: u8 = 0x02;
pub const BUTTON_MIDDLE: u8 = 0x04;

struct MouseState {
    x: i32,
    y: i32,
    buttons: u8,
    packet_byte: u8,
    packet: [u8; 3],
    max_x: i32,
    max_y: i32,
}

impl MouseState {
    const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            buttons: 0,
            packet_byte: 0,
            packet: [0; 3],
            max_x: 1,
            max_y: 1,
        }
    }
}

static STATE: IrqMutex<MouseState> = IrqMutex::new(MouseState::new());

/// Initialise the PS/2 mouse device.
///
/// Expects that `ps2::init_controller()` has already run (ports enabled,
/// clean config written with IRQs off).  Sends set-defaults and enable-
/// reporting commands via the AUX-aware ACK path so we never accidentally
/// consume a keyboard byte as a mouse ACK.
pub fn init() {
    klog_info!("PS/2 mouse: initialising device");

    // Set defaults (sample rate, resolution, scaling)
    ps2::write_aux_acked(ps2::DEV_CMD_DEFAULTS);

    // Enable data reporting
    ps2::write_aux_acked(ps2::DEV_CMD_ENABLE);

    // Flush any trailing bytes the mouse may have sent during init
    ps2::flush();

    let (x, y) = {
        let mut state = STATE.lock();
        state.x = state.max_x / 2;
        state.y = state.max_y / 2;
        state.packet_byte = 0;
        (state.x, state.y)
    };

    input_event::input_route_pointer_motion(x, y, 0);

    klog_info!("PS/2 mouse: initialised at ({}, {})", x, y);
}

pub fn set_bounds(width: i32, height: i32) {
    if width <= 0 || height <= 0 {
        return;
    }

    let mut state = STATE.lock();
    state.max_x = width;
    state.max_y = height;
    state.x = state.x.clamp(0, width - 1);
    state.y = state.y.clamp(0, height - 1);
}

/// Process a single mouse data byte from the IRQ handler.
///
/// The byte is accumulated into a 3-byte packet.  Byte 0 is validated:
/// bit 3 must be set (PS/2 protocol), and overflow bits (6:7) must be clear.
/// Invalid byte-0 values reset the state machine.
pub fn handle_irq(data: u8) {
    let mut state = STATE.lock();
    let byte_num = state.packet_byte;

    // PS/2 mouse packet byte 0 always has bit 3 set (per protocol).
    // If we're expecting byte 0 and bit 3 is clear, this isn't a valid
    // mouse packet start.  Reset the state machine and discard.
    if byte_num == 0 && data & 0x08 == 0 {
        return;
    }

    state.packet[byte_num as usize] = data;
    state.packet_byte = (byte_num + 1) % 3;

    if state.packet_byte != 0 {
        return;
    }

    let packet_flags = state.packet[0];
    let dx_raw = state.packet[1];
    let dy_raw = state.packet[2];

    // Overflow bits set — discard entire packet
    if packet_flags & 0xC0 != 0 {
        return;
    }

    let old_buttons = state.buttons;
    state.buttons = packet_flags & 0x07;

    let mut dx = dx_raw as i16;
    if packet_flags & 0x10 != 0 {
        dx -= 256;
    }

    let mut dy = dy_raw as i16;
    if packet_flags & 0x20 != 0 {
        dy -= 256;
    }

    dy = -dy;

    state.x += dx as i32;
    state.y += dy as i32;

    state.x = state.x.clamp(0, state.max_x - 1);
    state.y = state.y.clamp(0, state.max_y - 1);

    let final_x = state.x;
    let final_y = state.y;
    let final_buttons = state.buttons;

    drop(state);

    let timestamp_ms = get_timestamp_ms();

    if dx != 0 || dy != 0 {
        input_event::input_route_pointer_motion(final_x, final_y, timestamp_ms);
    }

    let button_changes = old_buttons ^ final_buttons;
    for button_bit in [BUTTON_LEFT, BUTTON_RIGHT, BUTTON_MIDDLE] {
        if button_changes & button_bit != 0 {
            let pressed = final_buttons & button_bit != 0;
            input_event::input_route_pointer_button(button_bit, pressed, timestamp_ms);
        }
    }
}

pub fn get_position() -> (i32, i32) {
    let state = STATE.lock();
    (state.x, state.y)
}

pub fn get_buttons() -> u8 {
    STATE.lock().buttons
}
