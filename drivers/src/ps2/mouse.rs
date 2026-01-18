use slopos_lib::{klog_debug, klog_info};

use crate::input_event::{self, get_timestamp_ms};
use crate::ps2;

pub const BUTTON_LEFT: u8 = 0x01;
pub const BUTTON_RIGHT: u8 = 0x02;
pub const BUTTON_MIDDLE: u8 = 0x04;

struct State {
    x: i32,
    y: i32,
    buttons: u8,
    packet_byte: u8,
    packet: [u8; 3],
    max_x: i32,
    max_y: i32,
}

static mut STATE: State = State {
    x: 0,
    y: 0,
    buttons: 0,
    packet_byte: 0,
    packet: [0; 3],
    max_x: 1920,
    max_y: 1080,
};

pub fn init() {
    klog_info!("Initializing PS/2 mouse...");

    let mut config = ps2::read_config();
    klog_debug!("PS/2 controller status: 0x{:02x}", config);

    ps2::write_command(ps2::CMD_ENABLE_AUX);

    config |= ps2::CONFIG_AUX_IRQ;
    ps2::write_config(config);

    ps2::write_aux(ps2::DEV_CMD_DEFAULTS);
    let ack = ps2::read_data();
    if ack != ps2::DEV_ACK {
        klog_info!("Mouse set defaults NAK: 0x{:02x}", ack);
    }

    ps2::write_aux(ps2::DEV_CMD_ENABLE);
    let ack = ps2::read_data();
    if ack != ps2::DEV_ACK {
        klog_info!("Mouse enable reporting NAK: 0x{:02x}", ack);
    }

    unsafe {
        STATE.x = STATE.max_x / 2;
        STATE.y = STATE.max_y / 2;
        STATE.packet_byte = 0;
    }

    klog_info!(
        "PS/2 mouse initialized at ({}, {})",
        unsafe { STATE.x },
        unsafe { STATE.y }
    );
}

pub fn set_bounds(width: i32, height: i32) {
    unsafe {
        STATE.max_x = width;
        STATE.max_y = height;
        STATE.x = STATE.x.clamp(0, width - 1);
        STATE.y = STATE.y.clamp(0, height - 1);
    }
}

pub fn handle_irq(data: u8) {
    unsafe {
        let state = &raw mut STATE;
        let byte_num = (*state).packet_byte;

        (*state).packet[byte_num as usize] = data;
        (*state).packet_byte = (byte_num + 1) % 3;

        if (*state).packet_byte != 0 {
            return;
        }

        let packet_flags = (*state).packet[0];
        let dx_raw = (*state).packet[1];
        let dy_raw = (*state).packet[2];

        if packet_flags & 0xC0 != 0 {
            klog_debug!("[MOUSE] Invalid packet flags: 0x{:02x}", packet_flags);
            return;
        }

        let old_buttons = (*state).buttons;
        (*state).buttons = packet_flags & 0x07;

        let mut dx = dx_raw as i16;
        if packet_flags & 0x10 != 0 {
            dx = (dx as i16) - 256;
        }

        let mut dy = dy_raw as i16;
        if packet_flags & 0x20 != 0 {
            dy = (dy as i16) - 256;
        }

        dy = -dy;

        (*state).x += dx as i32;
        (*state).y += dy as i32;

        (*state).x = (*state).x.clamp(0, (*state).max_x - 1);
        (*state).y = (*state).y.clamp(0, (*state).max_y - 1);

        let timestamp_ms = get_timestamp_ms();

        if dx != 0 || dy != 0 {
            input_event::input_route_pointer_motion((*state).x, (*state).y, timestamp_ms);
        }

        let button_changes = old_buttons ^ (*state).buttons;
        for button_bit in [BUTTON_LEFT, BUTTON_RIGHT, BUTTON_MIDDLE] {
            if button_changes & button_bit != 0 {
                let pressed = (*state).buttons & button_bit != 0;
                input_event::input_route_pointer_button(button_bit, pressed, timestamp_ms);
            }
        }
    }
}

pub fn get_position() -> (i32, i32) {
    unsafe { (STATE.x, STATE.y) }
}

pub fn get_buttons() -> u8 {
    unsafe { STATE.buttons }
}
