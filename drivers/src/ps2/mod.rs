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
//! # Init Sequence (Linux/RedoxOS-aligned)
//!
//! 1. Disable both ports (prevent device interference during setup)
//! 2. Flush output buffer (drain stale BIOS/bootloader bytes)
//! 3. Controller self-test (0xAA → expect 0x55)
//! 4. Write clean config byte from scratch (NOT read-modify-write)
//! 5. Enable ports
//! 6. Init keyboard device (reset, set defaults)
//! 7. Init mouse device (set defaults, enable reporting)
//! 8. Write final config with IRQs enabled
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
use slopos_arch::cpu;
use slopos_utils::ports::{PS2_COMMAND, PS2_DATA, PS2_STATUS};
use slopos_utils::{klog_debug, klog_info, klog_warn};

// =============================================================================
// Status Register Bits
// =============================================================================
/// Output buffer full - data available to read from port 0x60
pub const STATUS_OUTPUT_FULL: u8 = 0x01;
pub const STATUS_INPUT_FULL: u8 = 0x02;
pub const STATUS_MOUSE_DATA: u8 = 0x20;
pub const STATUS_TIMEOUT: u8 = 0x40;
pub const STATUS_PARITY: u8 = 0x80;
// =============================================================================
// Controller Commands (written to port 0x64)
// =============================================================================
/// Read controller configuration byte
pub const CMD_READ_CONFIG: u8 = 0x20;
pub const CMD_WRITE_CONFIG: u8 = 0x60;
pub const CMD_DISABLE_AUX: u8 = 0xA7;
pub const CMD_ENABLE_AUX: u8 = 0xA8;
pub const CMD_TEST_AUX: u8 = 0xA9;
pub const CMD_TEST_CONTROLLER: u8 = 0xAA;
pub const CMD_TEST_FIRST_PORT: u8 = 0xAB;
pub const CMD_DISABLE_FIRST: u8 = 0xAD;
pub const CMD_ENABLE_FIRST: u8 = 0xAE;
pub const CMD_WRITE_AUX: u8 = 0xD4;
pub const CMD_PULSE_RESET: u8 = 0xFE;
// =============================================================================
// Configuration Byte Bits
// =============================================================================
/// Enable first port (keyboard) interrupt (IRQ1)
pub const CONFIG_FIRST_IRQ: u8 = 0x01;
pub const CONFIG_AUX_IRQ: u8 = 0x02;
pub const CONFIG_SYSTEM: u8 = 0x04;
pub const CONFIG_FIRST_CLOCK_DISABLE: u8 = 0x10;
pub const CONFIG_AUX_CLOCK_DISABLE: u8 = 0x20;
pub const CONFIG_TRANSLATION: u8 = 0x40;
// =============================================================================
// Device Commands (written to port 0x60)
// =============================================================================

/// Reset device
pub const DEV_CMD_RESET: u8 = 0xFF;
/// Set device defaults
pub const DEV_CMD_DEFAULTS: u8 = 0xF6;
pub const DEV_CMD_ENABLE: u8 = 0xF4;
pub const DEV_CMD_DISABLE: u8 = 0xF5;
pub const DEV_ACK: u8 = 0xFA;
pub const DEV_RESEND: u8 = 0xFE;
/// Device self-test passed response
pub const DEV_SELF_TEST_PASS: u8 = 0xAA;

// =============================================================================
// Timing Constants
// =============================================================================
/// Maximum iterations to wait for controller ready (approximately 100ms at typical speeds)
const WAIT_ITERATIONS: u32 = 100_000;
/// Maximum bytes to drain during flush (safety limit)
const FLUSH_MAX_BYTES: u32 = 64;

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

/// Check if the data in the output buffer came from the mouse (AUX port).
///
/// Reads status bit 5 (MOUSE_OBF).  Reliable on QEMU >= 6.1 where
/// `kbd_safe_update_irq` prevents status changes while OBF is set.
#[inline(always)]
pub fn is_mouse_data() -> bool {
    read_status() & STATUS_MOUSE_DATA != 0
}
/// Check if the controller input buffer is full (busy).
#[inline(always)]
pub fn is_busy() -> bool {
    read_status() & STATUS_INPUT_FULL != 0
}
/// Poll the status register until `condition` returns `true`, or timeout.
#[inline(always)]
fn wait_for_status(condition: fn() -> bool) -> bool {
    for _ in 0..WAIT_ITERATIONS {
        if condition() {
            return true;
        }
        cpu::pause();
    }
    false
}
/// Check if the controller is ready to accept input (input buffer empty).
#[inline(always)]
fn is_ready() -> bool {
    !is_busy()
}
/// Wait until the controller is ready to accept input (input buffer empty).
///
/// This must be called before writing commands or data to the controller.
/// Returns `true` if ready, `false` if timeout occurred.
#[inline(always)]
pub fn wait_ready() -> bool {
    wait_for_status(is_ready)
}
/// Wait until data is available to read (output buffer full).
///
/// This must be called before reading data from the controller.
/// Returns `true` if data available, `false` if timeout occurred.
#[inline(always)]
pub fn wait_data() -> bool {
    wait_for_status(has_data)
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

// =============================================================================
// Auxiliary ACK Reading (AUX-aware)
// =============================================================================

/// Read a byte from the auxiliary (mouse) port, checking status bit 5.
///
/// Waits for OBF with AUX bit set.  If a keyboard byte arrives instead
/// (OBF set but AUX clear), it is silently discarded — during init the
/// keyboard port is disabled so this should be rare.
///
/// Returns `Some(byte)` on success, `None` on timeout.
pub fn read_aux_data() -> Option<u8> {
    for _ in 0..WAIT_ITERATIONS {
        let status = read_status();
        if status & STATUS_OUTPUT_FULL != 0 {
            let byte = read_data_nowait();
            if status & STATUS_MOUSE_DATA != 0 {
                return Some(byte);
            }
            // Stray keyboard byte during init — discard and keep waiting
            klog_debug!(
                "PS/2: discarded stray keyboard byte 0x{:02x} during AUX read",
                byte
            );
            continue;
        }
        cpu::pause();
    }
    None
}

/// Send a command to the mouse and wait for ACK (0xFA) via the AUX path.
///
/// Returns `true` if ACK received, `false` on NAK or timeout.
pub fn write_aux_acked(cmd: u8) -> bool {
    write_aux(cmd);
    match read_aux_data() {
        Some(DEV_ACK) => true,
        Some(other) => {
            klog_warn!(
                "PS/2 mouse: expected ACK for 0x{:02x}, got 0x{:02x}",
                cmd,
                other
            );
            false
        }
        None => {
            klog_warn!("PS/2 mouse: timeout waiting for ACK to 0x{:02x}", cmd);
            false
        }
    }
}

// =============================================================================
// Controller Initialisation
// =============================================================================

/// Drain all pending bytes from the controller output buffer.
///
/// Reads and discards up to `FLUSH_MAX_BYTES` bytes.  Matches the
/// `i8042_flush` pattern used by the Linux kernel to clear stale data
/// left by BIOS or the bootloader.
pub fn flush() {
    for i in 0..FLUSH_MAX_BYTES {
        if read_status() & STATUS_OUTPUT_FULL == 0 {
            if i > 0 {
                klog_debug!("PS/2: flushed {} stale byte(s)", i);
            }
            return;
        }
        let _ = read_data_nowait();
        // Small delay between reads so the controller can update status
        for _ in 0..100 {
            cpu::pause();
        }
    }
    klog_warn!(
        "PS/2: flush hit limit ({} bytes) — controller may be misbehaving",
        FLUSH_MAX_BYTES
    );
}

/// Full PS/2 controller initialisation following Linux/RedoxOS patterns.
///
/// This must be called BEFORE individual device init (keyboard, mouse).
/// The sequence is:
///  1. Disable both ports (stop devices from sending data)
///  2. Flush stale bytes from the output buffer
///  3. Controller self-test (command 0xAA, expect response 0x55)
///  4. Write a clean configuration byte from scratch (NOT read-modify-write)
///     — IRQs disabled during device init, translation enabled, clocks on
///  5. Enable both ports (re-enable clock lines so devices can communicate)
///
/// After this function returns, callers should init individual devices
/// and then write the final config with IRQs enabled.
pub fn init_controller() {
    klog_info!("PS/2: starting controller initialisation");

    // Step 1: Disable both ports to prevent device interference
    write_command(CMD_DISABLE_FIRST);
    write_command(CMD_DISABLE_AUX);
    klog_debug!("PS/2: both ports disabled");

    // Step 2: Flush output buffer
    flush();

    // Step 3: Controller self-test
    write_command(CMD_TEST_CONTROLLER);
    if wait_data() {
        let result = read_data_nowait();
        if result == 0x55 {
            klog_debug!("PS/2: controller self-test passed");
        } else {
            klog_warn!(
                "PS/2: controller self-test returned 0x{:02x} (expected 0x55)",
                result
            );
        }
    } else {
        klog_warn!("PS/2: controller self-test timed out");
    }

    // Flush again — self-test may produce extra bytes
    flush();

    // Step 4: Write a CLEAN configuration byte.
    // Do NOT read-modify-write — we set exactly the bits we want.
    // IRQs OFF during device init (will be enabled after devices are ready).
    // Translation OFF during init so device responses (0xFA ACK, 0xAA self-test)
    // are read raw and not mangled by the scancode translation table.
    // Both clocks ENABLED (clock disable bits = 0).
    let init_config = CONFIG_SYSTEM;
    write_config(init_config);
    klog_debug!(
        "PS/2: wrote init config 0x{:02x} (IRQs off, translation off)",
        init_config
    );

    // Step 5: Enable both ports (un-gate the clock lines)
    write_command(CMD_ENABLE_FIRST);
    write_command(CMD_ENABLE_AUX);
    klog_debug!("PS/2: both ports enabled");

    klog_info!("PS/2: controller initialisation complete");
}

/// Write the final configuration byte with IRQs enabled.
///
/// Called after both keyboard and mouse devices have been initialised.
/// Enables IRQ 1 (keyboard) and IRQ 12 (mouse).
pub fn enable_irqs() {
    let final_config = CONFIG_FIRST_IRQ | CONFIG_AUX_IRQ | CONFIG_SYSTEM | CONFIG_TRANSLATION;
    write_config(final_config);
    klog_info!("PS/2: IRQs enabled — config 0x{:02x}", final_config);
}
