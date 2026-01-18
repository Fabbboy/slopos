//! PS/2 Controller Driver
//!
//! Provides shared low-level access to the PS/2 controller for keyboard and mouse drivers.
//! The PS/2 controller (historically the 8042) handles communication with PS/2 devices
//! through ports 0x60 (data) and 0x64 (status/command).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐     ┌─────────────────┐     ┌──────────────┐
//! │  Keyboard   │────▶│  PS/2 Controller │◀────│    Mouse     │
//! │   Driver    │     │   (this module)  │     │    Driver    │
//! └─────────────┘     └─────────────────┘     └──────────────┘
//!                              │
//!                              ▼
//!                     ┌─────────────────┐
//!                     │   I/O Ports     │
//!                     │ 0x60 (data)     │
//!                     │ 0x64 (cmd/stat) │
//!                     └─────────────────┘
//! ```
//!
//! # Status Register (Port 0x64 read)
//!
//! | Bit | Name | Description |
//! |-----|------|-------------|
//! | 0   | OBF  | Output buffer full (data available to read) |
//! | 1   | IBF  | Input buffer full (controller busy) |
//! | 2   | SYS  | System flag (POST passed) |
//! | 3   | A2   | Address line A2 (0=data, 1=command) |
//! | 4   | INH  | Inhibit switch |
//! | 5   | MOBF | Mouse output buffer full |
//! | 6   | TMOE | Timeout error |
//! | 7   | PARE | Parity error |

pub mod keyboard;
pub mod mouse;

use slopos_lib::cpu;
use slopos_lib::ports::{PS2_COMMAND, PS2_DATA, PS2_STATUS};

// =============================================================================
// Status Register Bits
// =============================================================================

/// Output buffer full - data available to read from port 0x60
pub const STATUS_OUTPUT_FULL: u8 = 0x01;

/// Input buffer full - controller is processing, wait before writing
pub const STATUS_INPUT_FULL: u8 = 0x02;

/// Mouse data available (auxiliary output buffer full)
pub const STATUS_MOUSE_DATA: u8 = 0x20;

/// Timeout error occurred
pub const STATUS_TIMEOUT: u8 = 0x40;

/// Parity error occurred
pub const STATUS_PARITY: u8 = 0x80;

// =============================================================================
// Controller Commands (written to port 0x64)
// =============================================================================

/// Read controller configuration byte
pub const CMD_READ_CONFIG: u8 = 0x20;

/// Write controller configuration byte
pub const CMD_WRITE_CONFIG: u8 = 0x60;

/// Disable auxiliary (mouse) interface
pub const CMD_DISABLE_AUX: u8 = 0xA7;

/// Enable auxiliary (mouse) interface
pub const CMD_ENABLE_AUX: u8 = 0xA8;

/// Test auxiliary (mouse) interface
pub const CMD_TEST_AUX: u8 = 0xA9;

/// Test PS/2 controller
pub const CMD_TEST_CONTROLLER: u8 = 0xAA;

/// Test first PS/2 port (keyboard)
pub const CMD_TEST_FIRST_PORT: u8 = 0xAB;

/// Disable first PS/2 port (keyboard)
pub const CMD_DISABLE_FIRST: u8 = 0xAD;

/// Enable first PS/2 port (keyboard)
pub const CMD_ENABLE_FIRST: u8 = 0xAE;

/// Write next byte to auxiliary (mouse) device
pub const CMD_WRITE_AUX: u8 = 0xD4;

/// Pulse reset line (system reset)
pub const CMD_PULSE_RESET: u8 = 0xFE;

// =============================================================================
// Configuration Byte Bits
// =============================================================================

/// Enable first port (keyboard) interrupt (IRQ1)
pub const CONFIG_FIRST_IRQ: u8 = 0x01;

/// Enable auxiliary (mouse) interrupt (IRQ12)
pub const CONFIG_AUX_IRQ: u8 = 0x02;

/// System flag (should be set after POST)
pub const CONFIG_SYSTEM: u8 = 0x04;

/// Disable first port clock
pub const CONFIG_FIRST_CLOCK_DISABLE: u8 = 0x10;

/// Disable auxiliary port clock
pub const CONFIG_AUX_CLOCK_DISABLE: u8 = 0x20;

/// Enable first port translation (scancode set 1)
pub const CONFIG_TRANSLATION: u8 = 0x40;

// =============================================================================
// Device Commands (written to port 0x60)
// =============================================================================

/// Set device defaults
pub const DEV_CMD_DEFAULTS: u8 = 0xF6;

/// Enable data reporting (mouse)
pub const DEV_CMD_ENABLE: u8 = 0xF4;

/// Disable data reporting
pub const DEV_CMD_DISABLE: u8 = 0xF5;

/// Device acknowledge response
pub const DEV_ACK: u8 = 0xFA;

/// Device resend request
pub const DEV_RESEND: u8 = 0xFE;

// =============================================================================
// Timing Constants
// =============================================================================

/// Maximum iterations to wait for controller ready (approximately 100ms at typical speeds)
const WAIT_ITERATIONS: u32 = 100_000;

// =============================================================================
// Low-Level Controller Access
// =============================================================================

/// Read the PS/2 controller status register.
///
/// Returns the current status byte. Check individual bits using the STATUS_* constants.
#[inline(always)]
pub fn read_status() -> u8 {
    unsafe { PS2_STATUS.read() }
}

/// Check if data is available to read from the controller.
#[inline(always)]
pub fn has_data() -> bool {
    read_status() & STATUS_OUTPUT_FULL != 0
}

/// Check if the data is from the mouse (auxiliary device).
#[inline(always)]
pub fn is_mouse_data() -> bool {
    read_status() & STATUS_MOUSE_DATA != 0
}

/// Check if the controller input buffer is full (busy).
#[inline(always)]
pub fn is_busy() -> bool {
    read_status() & STATUS_INPUT_FULL != 0
}

/// Wait until the controller is ready to accept input (input buffer empty).
///
/// This must be called before writing commands or data to the controller.
/// Returns `true` if ready, `false` if timeout occurred.
#[inline(always)]
pub fn wait_ready() -> bool {
    for _ in 0..WAIT_ITERATIONS {
        if !is_busy() {
            return true;
        }
        cpu::pause();
    }
    false
}

/// Wait until data is available to read (output buffer full).
///
/// This must be called before reading data from the controller.
/// Returns `true` if data available, `false` if timeout occurred.
#[inline(always)]
pub fn wait_data() -> bool {
    for _ in 0..WAIT_ITERATIONS {
        if has_data() {
            return true;
        }
        cpu::pause();
    }
    false
}

/// Write a command to the PS/2 controller (port 0x64).
///
/// Waits for the controller to be ready before writing.
/// Use CMD_* constants for command values.
#[inline(always)]
pub fn write_command(cmd: u8) {
    wait_ready();
    unsafe { PS2_COMMAND.write(cmd) }
}

/// Write data to the PS/2 data port (port 0x60).
///
/// Waits for the controller to be ready before writing.
/// Used to send data after certain commands or to send bytes to devices.
#[inline(always)]
pub fn write_data(data: u8) {
    wait_ready();
    unsafe { PS2_DATA.write(data) }
}

/// Read data from the PS/2 data port (port 0x60).
///
/// Waits for data to be available before reading.
/// Returns the byte read from the controller.
#[inline(always)]
pub fn read_data() -> u8 {
    wait_data();
    unsafe { PS2_DATA.read() }
}

/// Read data immediately without waiting.
///
/// Used in interrupt handlers where data is known to be available.
/// Caller must ensure data is available (check status first).
#[inline(always)]
pub fn read_data_nowait() -> u8 {
    unsafe { PS2_DATA.read() }
}

// =============================================================================
// Mouse (Auxiliary Device) Operations
// =============================================================================

/// Write a command byte to the mouse (auxiliary device).
///
/// Sends CMD_WRITE_AUX to the controller, then sends the command byte.
/// The mouse will typically respond with DEV_ACK (0xFA).
#[inline(always)]
pub fn write_aux(cmd: u8) {
    write_command(CMD_WRITE_AUX);
    write_data(cmd);
}

/// Read the controller configuration byte.
pub fn read_config() -> u8 {
    write_command(CMD_READ_CONFIG);
    read_data()
}

/// Write the controller configuration byte.
pub fn write_config(config: u8) {
    write_command(CMD_WRITE_CONFIG);
    write_data(config);
}
