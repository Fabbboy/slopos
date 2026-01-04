use slopos_lib::{cpu, klog_debug, klog_info};

use crate::input_event;
use crate::irq;
use crate::pit::pit_get_frequency;

const MOUSE_BUFFER_SIZE: usize = 256;
const PS2_DATA_PORT: u16 = 0x60;
const PS2_STATUS_PORT: u16 = 0x64;
const PS2_COMMAND_PORT: u16 = 0x64;

// Mouse buttons
pub const MOUSE_BUTTON_LEFT: u8 = 0x01;
pub const MOUSE_BUTTON_RIGHT: u8 = 0x02;
pub const MOUSE_BUTTON_MIDDLE: u8 = 0x04;

#[derive(Clone, Copy)]
struct MouseEvent {
    x: i32,
    y: i32,
    buttons: u8,
}

#[derive(Clone, Copy)]
struct MouseBuffer {
    data: [MouseEvent; MOUSE_BUFFER_SIZE],
    head: u32,
    tail: u32,
    count: u32,
}

impl MouseBuffer {
    const fn new() -> Self {
        Self {
            data: [MouseEvent {
                x: 0,
                y: 0,
                buttons: 0,
            }; MOUSE_BUFFER_SIZE],
            head: 0,
            tail: 0,
            count: 0,
        }
    }
}

struct MouseState {
    x: i32,
    y: i32,
    buttons: u8,
    packet_byte: u8,
    packet: [u8; 3],
    max_x: i32,
    max_y: i32,
}

static mut MOUSE_STATE: MouseState = MouseState {
    x: 0,
    y: 0,
    buttons: 0,
    packet_byte: 0,
    packet: [0; 3],
    max_x: 1024,
    max_y: 768,
};

static mut EVENT_BUFFER: MouseBuffer = MouseBuffer::new();

#[inline(always)]
fn ps2_wait_input() {
    for _ in 0..100000 {
        unsafe {
            let status: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") PS2_STATUS_PORT,
                out("al") status,
                options(nomem, nostack, preserves_flags)
            );
            if status & 0x02 == 0 {
                return;
            }
        }
        cpu::pause();
    }
}

#[inline(always)]
fn ps2_wait_output() {
    for _ in 0..100000 {
        unsafe {
            let status: u8;
            core::arch::asm!(
                "in al, dx",
                in("dx") PS2_STATUS_PORT,
                out("al") status,
                options(nomem, nostack, preserves_flags)
            );
            if status & 0x01 != 0 {
                return;
            }
        }
        cpu::pause();
    }
}

#[inline(always)]
fn ps2_write_command(cmd: u8) {
    ps2_wait_input();
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") PS2_COMMAND_PORT,
            in("al") cmd,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline(always)]
fn ps2_write_data(data: u8) {
    ps2_wait_input();
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") PS2_DATA_PORT,
            in("al") data,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[inline(always)]
fn ps2_read_data() -> u8 {
    ps2_wait_output();
    unsafe {
        let data: u8;
        core::arch::asm!(
            "in al, dx",
            in("dx") PS2_DATA_PORT,
            out("al") data,
            options(nomem, nostack, preserves_flags)
        );
        data
    }
}

fn mouse_write(cmd: u8) {
    ps2_write_command(0xD4); // Send to mouse
    ps2_write_data(cmd);
}

fn mouse_read() -> u8 {
    ps2_read_data()
}

pub fn mouse_init() {
    klog_info!("Initializing PS/2 mouse...");

    // Get compaq status
    ps2_write_command(0x20);
    let mut status = ps2_read_data();
    klog_debug!("PS/2 controller status: 0x{:02x}", status);

    // Enable auxiliary device
    ps2_write_command(0xA8);

    // Enable interrupts
    status |= 0x02; // Enable auxiliary interrupt
    ps2_write_command(0x60);
    ps2_write_data(status);

    // Set mouse defaults
    mouse_write(0xF6);
    let ack = mouse_read();
    if ack != 0xFA {
        klog_info!("Mouse set defaults NAK: 0x{:02x}", ack);
    }

    // Enable data reporting
    mouse_write(0xF4);
    let ack = mouse_read();
    if ack != 0xFA {
        klog_info!("Mouse enable reporting NAK: 0x{:02x}", ack);
    }

    unsafe {
        MOUSE_STATE.x = MOUSE_STATE.max_x / 2;
        MOUSE_STATE.y = MOUSE_STATE.max_y / 2;
        MOUSE_STATE.packet_byte = 0;

        // Generate initial mouse event so compositor knows starting position
        let initial_event = MouseEvent {
            x: MOUSE_STATE.x,
            y: MOUSE_STATE.y,
            buttons: 0,
        };
        buffer_push(initial_event);
    }

    klog_info!("PS/2 mouse initialized at ({}, {})",
        unsafe { MOUSE_STATE.x },
        unsafe { MOUSE_STATE.y });
}

pub fn mouse_set_bounds(width: i32, height: i32) {
    unsafe {
        MOUSE_STATE.max_x = width;
        MOUSE_STATE.max_y = height;
        MOUSE_STATE.x = MOUSE_STATE.x.clamp(0, width - 1);
        MOUSE_STATE.y = MOUSE_STATE.y.clamp(0, height - 1);
    }
}

#[inline(always)]
unsafe fn buffer_push(event: MouseEvent) {
    let buf = &raw mut EVENT_BUFFER;
    if (*buf).count >= MOUSE_BUFFER_SIZE as u32 {
        (*buf).tail = ((*buf).tail + 1) % MOUSE_BUFFER_SIZE as u32;
        (*buf).count = (*buf).count.saturating_sub(1);
    }
    (*buf).data[(*buf).head as usize] = event;
    (*buf).head = ((*buf).head + 1) % MOUSE_BUFFER_SIZE as u32;
    (*buf).count = (*buf).count.saturating_add(1);
}

/// Get current timestamp in milliseconds for input events.
fn get_timestamp_ms() -> u64 {
    let ticks = irq::get_timer_ticks();
    let freq = pit_get_frequency();
    if freq == 0 {
        return 0;
    }
    (ticks * 1000) / freq as u64
}

#[inline(always)]
unsafe fn buffer_pop() -> Option<MouseEvent> {
    let buf = &raw mut EVENT_BUFFER;
    let mut out = None;
    cpu::disable_interrupts();
    if (*buf).count > 0 {
        let event = (*buf).data[(*buf).tail as usize];
        (*buf).tail = ((*buf).tail + 1) % MOUSE_BUFFER_SIZE as u32;
        (*buf).count = (*buf).count.saturating_sub(1);
        out = Some(event);
    }
    cpu::enable_interrupts();
    out
}

pub fn mouse_handle_irq(scancode: u8) {
    unsafe {
        let state = &raw mut MOUSE_STATE;
        let byte_num = (*state).packet_byte;

        (*state).packet[byte_num as usize] = scancode;
        (*state).packet_byte = (byte_num + 1) % 3;

        // Wait for complete 3-byte packet
        if (*state).packet_byte != 0 {
            return;
        }

        let flags = (*state).packet[0];
        let dx_raw = (*state).packet[1];
        let dy_raw = (*state).packet[2];

        // Check for overflow or invalid packet
        if flags & 0xC0 != 0 {
            klog_debug!("[MOUSE] Invalid packet flags: 0x{:02x}", flags);
            return;
        }

        // Save old button state for detecting button events
        let old_buttons = (*state).buttons;

        // Extract button state
        (*state).buttons = flags & 0x07;

        // Convert to signed delta (9-bit sign-extended)
        let mut dx = dx_raw as i16;
        if flags & 0x10 != 0 {
            dx = (dx as i16) - 256;
        }

        let mut dy = dy_raw as i16;
        if flags & 0x20 != 0 {
            dy = (dy as i16) - 256;
        }

        // PS/2 mouse Y is inverted
        dy = -dy;

        // Update absolute position
        (*state).x += dx as i32;
        (*state).y += dy as i32;

        // Clamp to bounds
        (*state).x = (*state).x.clamp(0, (*state).max_x - 1);
        (*state).y = (*state).y.clamp(0, (*state).max_y - 1);

        // Push event to buffer (for legacy mouse_read_event API)
        buffer_push(MouseEvent {
            x: (*state).x,
            y: (*state).y,
            buttons: (*state).buttons,
        });

        // Route events to input_event system for Wayland-like per-task queues
        let timestamp_ms = get_timestamp_ms();

        // Route motion event if mouse moved
        if dx != 0 || dy != 0 {
            input_event::input_route_pointer_motion((*state).x, (*state).y, timestamp_ms);
        }

        // Route button events for any buttons that changed state
        let button_changes = old_buttons ^ (*state).buttons;
        for button_bit in [MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT, MOUSE_BUTTON_MIDDLE] {
            if button_changes & button_bit != 0 {
                let pressed = (*state).buttons & button_bit != 0;
                input_event::input_route_pointer_button(button_bit, pressed, timestamp_ms);
            }
        }
    }
}

pub fn mouse_get_position() -> (i32, i32) {
    unsafe { (MOUSE_STATE.x, MOUSE_STATE.y) }
}

pub fn mouse_get_buttons() -> u8 {
    unsafe { MOUSE_STATE.buttons }
}

pub fn mouse_read_event(x: *mut i32, y: *mut i32, buttons: *mut u8) -> bool {
    if x.is_null() || y.is_null() || buttons.is_null() {
        return false;
    }

    if let Some(event) = unsafe { buffer_pop() } {
        unsafe {
            *x = event.x;
            *y = event.y;
            *buttons = event.buttons;
        }
        true
    } else {
        false
    }
}

pub fn mouse_has_events() -> bool {
    unsafe { EVENT_BUFFER.count > 0 }
}
