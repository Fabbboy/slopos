use slopos_ostd::klog_info;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

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
    packet: [u8; 4],
    packet_size: u8,
    mouse_type: u8,
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
            packet: [0; 4],
            packet_size: 3,
            mouse_type: 0,
            max_x: 1,
            max_y: 1,
        }
    }
}

static STATE: SpinLock<MouseState> = SpinLock::new(MouseState::new(), LOCK_LEVEL_RESOURCE);

/// Attempt IntelliMouse (ImPS/2) detection.
/// Magic sequence: SET_SAMPLE_RATE 200, 100, 80 → GET_ID → expect 3.
fn probe_intellimouse() -> u8 {
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(100);
    ps2::write_aux_set_sample_rate(80);

    if !ps2::write_aux_acked(ps2::DEV_CMD_GET_ID) {
        return 0;
    }
    match ps2::read_aux_data() {
        Some(3) => 3,
        _ => 0,
    }
}

/// Attempt IntelliMouse Explorer (ImExPS/2) detection.
/// Requires ImPS/2 already active. Magic: SET_SAMPLE_RATE 200, 200, 80 → GET_ID → expect 4.
fn probe_intellimouse_explorer() -> u8 {
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(200);
    ps2::write_aux_set_sample_rate(80);

    if !ps2::write_aux_acked(ps2::DEV_CMD_GET_ID) {
        return 3;
    }
    match ps2::read_aux_data() {
        Some(4) => 4,
        _ => 3,
    }
}

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

    // Probe for IntelliMouse extensions (must happen before enable)
    let mut mouse_type: u8 = probe_intellimouse();
    if mouse_type == 3 {
        mouse_type = probe_intellimouse_explorer();
    }

    let packet_size: u8 = if mouse_type >= 3 { 4 } else { 3 };
    klog_info!(
        "PS/2 mouse: detected type {} ({}), {}-byte packets",
        mouse_type,
        match mouse_type {
            0 => "standard",
            3 => "IntelliMouse (scroll wheel)",
            4 => "IntelliMouse Explorer (scroll + buttons 4/5)",
            _ => "unknown",
        },
        packet_size,
    );

    // Enable data reporting
    ps2::write_aux_acked(ps2::DEV_CMD_ENABLE);

    // Flush any trailing bytes the mouse may have sent during init
    ps2::flush();

    let (x, y) = {
        let mut state = STATE.lock();
        state.mouse_type = mouse_type;
        state.packet_size = packet_size;
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
/// The byte is accumulated into a 3- or 4-byte packet (depending on the
/// detected mouse protocol).  Byte 0 is validated: bit 3 must be set
/// (PS/2 protocol), and overflow bits (6:7) must be clear.  For ImPS/2
/// and ImExPS/2 mice the fourth byte carries scroll and extra button data.
pub fn handle_irq(data: u8) {
    let mut state = STATE.lock();
    let byte_num = state.packet_byte;

    // Byte 0 sync: bit 3 must be set (PS/2 protocol marker)
    if byte_num == 0 && data & 0x08 == 0 {
        return;
    }

    state.packet[byte_num as usize] = data;
    state.packet_byte += 1;

    // Wait for full packet (3 or 4 bytes depending on protocol)
    if state.packet_byte < state.packet_size {
        return;
    }
    state.packet_byte = 0;

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

    // Parse scroll data from byte 3 (IntelliMouse only)
    let mut dz: i32 = 0;
    let mut dw: i32 = 0;

    if state.mouse_type >= 3 && state.packet_size == 4 {
        let b3 = state.packet[3];
        match state.mouse_type {
            3 => {
                // ImPS/2: lower 4 bits are signed Z scroll
                let mut z = (b3 & 0x0F) as i8;
                if b3 & 0x08 != 0 {
                    z |= -16_i8; // sign-extend from 4 bits
                }
                dz = z as i32;
            }
            4 => {
                // ImExPS/2: upper 2 bits select encoding
                match b3 & 0xC0 {
                    0x00 | 0xC0 => {
                        // Standard: bits 3:0 = 4-bit signed Z
                        let mut z = (b3 & 0x0F) as i8;
                        if b3 & 0x08 != 0 {
                            z |= -16_i8;
                        }
                        dz = z as i32;
                    }
                    0x80 => {
                        // Vertical scroll (IM 4.0): bits 5:0 = 6-bit signed
                        let mut z = (b3 & 0x3F) as i8;
                        if b3 & 0x20 != 0 {
                            z |= -64_i8;
                        }
                        dz = z as i32;
                    }
                    0x40 => {
                        // Horizontal scroll (IM 4.0): bits 5:0 = 6-bit signed
                        let mut w = (b3 & 0x3F) as i8;
                        if b3 & 0x20 != 0 {
                            w |= -64_i8;
                        }
                        dw = -(w as i32);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

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

    // Route scroll axis events (value120: one click = ±120)
    if dz != 0 {
        input_event::input_route_pointer_axis(0, dz * 120, timestamp_ms);
    }
    if dw != 0 {
        input_event::input_route_pointer_axis(1, dw * 120, timestamp_ms);
    }
}

pub fn get_position() -> (i32, i32) {
    let state = STATE.lock();
    (state.x, state.y)
}

pub fn get_buttons() -> u8 {
    STATE.lock().buttons
}
