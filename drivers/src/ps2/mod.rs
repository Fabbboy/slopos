//! Shared low-level access to the PS/2 controller (the 8042) for the keyboard and
//! mouse drivers: port 0x60 is data, port 0x64 is status (read) / command (write).
pub mod keyboard;
pub mod mouse;
pub mod platform;
use core::sync::atomic::{AtomicBool, Ordering};

use slopos_arch::cpu;
use slopos_kernel_services::driver_runtime::{
    LEGACY_IRQ_KEYBOARD, LEGACY_IRQ_MOUSE, irq_increment_keyboard_events,
};
use slopos_ostd::io::Ps2Regs;
use slopos_ostd::io::port::IoPortRegistry;
use slopos_ostd::sync::OnceLock;
use slopos_ostd::{klog_debug, klog_info, klog_warn};

/// `i8042.legacy` cmdline escape hatch: when set, [`crate::irq::init`] runs the
/// hardcoded PS/2 bring-up at boot and the platform-bus i8042 driver declines.
static LEGACY_MODE: AtomicBool = AtomicBool::new(false);

pub fn set_legacy_mode(on: bool) {
    LEGACY_MODE.store(on, Ordering::Release);
}

pub fn legacy_mode() -> bool {
    LEGACY_MODE.load(Ordering::Acquire)
}

/// Read the pending byte and route it to the keyboard or mouse handler. `irq_line`
/// selects the target; for an unexpected line the AUX status bit disambiguates.
pub fn dispatch_irq(irq_line: u8) {
    let status = read_status();
    if status & STATUS_OUTPUT_FULL == 0 {
        return;
    }
    let data = read_data_nowait();
    match irq_line {
        LEGACY_IRQ_KEYBOARD => {
            irq_increment_keyboard_events();
            keyboard::handle_scancode(data);
        }
        LEGACY_IRQ_MOUSE => {
            mouse::handle_irq(data);
        }
        _ => {
            if status & STATUS_MOUSE_DATA != 0 {
                mouse::handle_irq(data);
            } else {
                irq_increment_keyboard_events();
                keyboard::handle_scancode(data);
            }
        }
    }
}

static PORTS: OnceLock<Ps2Regs> = OnceLock::new();

fn ports() -> &'static Ps2Regs {
    PORTS.call_once(|| {
        Ps2Regs::new(
            IoPortRegistry::reserve::<u8>(0x60).expect("PS/2 data port"),
            IoPortRegistry::reserve::<u8>(0x64).expect("PS/2 status port"),
            IoPortRegistry::reserve::<u8>(0x64).expect("PS/2 command port"),
        )
    });
    PORTS.get().expect("PS/2 ports initialised")
}

pub const STATUS_OUTPUT_FULL: u8 = 0x01;
pub const STATUS_INPUT_FULL: u8 = 0x02;
pub const STATUS_MOUSE_DATA: u8 = 0x20;
pub const STATUS_TIMEOUT: u8 = 0x40;
pub const STATUS_PARITY: u8 = 0x80;
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
pub const CONFIG_FIRST_IRQ: u8 = 0x01;
pub const CONFIG_AUX_IRQ: u8 = 0x02;
pub const CONFIG_SYSTEM: u8 = 0x04;
pub const CONFIG_FIRST_CLOCK_DISABLE: u8 = 0x10;
pub const CONFIG_AUX_CLOCK_DISABLE: u8 = 0x20;
pub const CONFIG_TRANSLATION: u8 = 0x40;

pub const DEV_CMD_RESET: u8 = 0xFF;
pub const DEV_CMD_DEFAULTS: u8 = 0xF6;
pub const DEV_CMD_ENABLE: u8 = 0xF4;
pub const DEV_CMD_DISABLE: u8 = 0xF5;
pub const DEV_CMD_SET_SAMPLE_RATE: u8 = 0xF3;
/// Returns the device type: 0 = standard, 3 = ImPS/2, 4 = ImExPS/2.
pub const DEV_CMD_GET_ID: u8 = 0xF2;
pub const DEV_ACK: u8 = 0xFA;
pub const DEV_RESEND: u8 = 0xFE;
pub const DEV_SELF_TEST_PASS: u8 = 0xAA;

/// Roughly 100 ms at typical controller speeds.
const WAIT_ITERATIONS: u32 = 100_000;
const FLUSH_MAX_BYTES: u32 = 64;

#[inline(always)]
pub fn read_status() -> u8 {
    ports().read_status()
}
#[inline(always)]
pub fn has_data() -> bool {
    read_status() & STATUS_OUTPUT_FULL != 0
}

/// Reliable on QEMU >= 6.1, where `kbd_safe_update_irq` prevents status changes
/// while OBF is set.
#[inline(always)]
pub fn is_mouse_data() -> bool {
    read_status() & STATUS_MOUSE_DATA != 0
}
#[inline(always)]
pub fn is_busy() -> bool {
    read_status() & STATUS_INPUT_FULL != 0
}
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
#[inline(always)]
fn is_ready() -> bool {
    !is_busy()
}
/// Must precede any command or data write to the controller.
#[inline(always)]
pub fn wait_ready() -> bool {
    wait_for_status(is_ready)
}
/// Must precede any read of the controller's output buffer.
#[inline(always)]
pub fn wait_data() -> bool {
    wait_for_status(has_data)
}
/// Write a command to the PS/2 controller (port 0x64).
#[inline(always)]
pub fn write_command(cmd: u8) {
    wait_ready();
    ports().write_command(cmd);
}
/// Write to the PS/2 data port (port 0x60).
#[inline(always)]
pub fn write_data(data: u8) {
    wait_ready();
    ports().write_data(data);
}
/// Read from the PS/2 data port (port 0x60).
#[inline(always)]
pub fn read_data() -> u8 {
    wait_data();
    ports().read_data()
}
/// Caller must confirm the output buffer is full first.
#[inline(always)]
pub fn read_data_nowait() -> u8 {
    ports().read_data()
}

/// Write a command byte to the mouse (auxiliary device).
#[inline(always)]
pub fn write_aux(cmd: u8) {
    write_command(CMD_WRITE_AUX);
    write_data(cmd);
}
/// The protocol requires two ACKs: one for the command byte, one for the rate byte.
pub fn write_aux_set_sample_rate(rate: u8) -> bool {
    if !write_aux_acked(DEV_CMD_SET_SAMPLE_RATE) {
        return false;
    }
    write_aux(rate);
    match read_aux_data() {
        Some(DEV_ACK) => true,
        Some(other) => {
            klog_warn!(
                "PS/2 mouse: expected ACK for rate {}, got 0x{:02x}",
                rate,
                other
            );
            false
        }
        None => {
            klog_warn!("PS/2 mouse: timeout waiting for ACK to rate {}", rate);
            false
        }
    }
}
pub fn read_config() -> u8 {
    write_command(CMD_READ_CONFIG);
    read_data()
}
pub fn write_config(config: u8) {
    write_command(CMD_WRITE_CONFIG);
    write_data(config);
}

/// Read a byte from the auxiliary (mouse) port, discarding any keyboard byte that
/// arrives instead. `None` on timeout.
pub fn read_aux_data() -> Option<u8> {
    for _ in 0..WAIT_ITERATIONS {
        let status = read_status();
        if status & STATUS_OUTPUT_FULL != 0 {
            let byte = read_data_nowait();
            if status & STATUS_MOUSE_DATA != 0 {
                return Some(byte);
            }
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

/// Quick, bounded probe for an attached PS/2 AUX (mouse) device: resets it, requires
/// an ACK, and drains its post-reset bytes so a later [`mouse::init`] starts from a
/// clean output buffer.
///
/// Lets the platform-bus i8042 driver skip mouse bring-up on machines with no PS/2
/// pointing device, where `mouse::init`'s many AUX round-trips would each time out
/// and stall the boot. Requires `init_controller()` to have already run.
pub fn aux_reset_probe() -> bool {
    write_aux(DEV_CMD_RESET);
    if !matches!(read_aux_data(), Some(DEV_ACK)) {
        return false;
    }
    // A reset ACK is followed by 0xAA (self-test pass) then a device-id byte; drain
    // both so they are not mistaken for a command ACK.
    let _ = read_aux_data();
    let _ = read_aux_data();
    flush();
    true
}

/// Send a command to the mouse and wait for ACK (0xFA) via the AUX path.
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

/// Drain bytes left in the controller output buffer by BIOS or the bootloader.
pub fn flush() {
    for i in 0..FLUSH_MAX_BYTES {
        if read_status() & STATUS_OUTPUT_FULL == 0 {
            if i > 0 {
                klog_debug!("PS/2: flushed {} stale byte(s)", i);
            }
            return;
        }
        let _ = read_data_nowait();
        // The controller needs a gap between reads to update its status register.
        for _ in 0..100 {
            cpu::pause();
        }
    }
    klog_warn!(
        "PS/2: flush hit limit ({} bytes) — controller may be misbehaving",
        FLUSH_MAX_BYTES
    );
}

/// Must run before any individual device init (keyboard, mouse); the caller writes
/// the final IRQ-enabled config afterwards via [`enable_irqs`].
pub fn init_controller() {
    klog_info!("PS/2: starting controller initialisation");

    // Disabling both ports keeps devices from injecting bytes during setup.
    write_command(CMD_DISABLE_FIRST);
    write_command(CMD_DISABLE_AUX);
    klog_debug!("PS/2: both ports disabled");

    flush();

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

    // The self-test may leave extra bytes behind.
    flush();

    // Written from scratch, not read-modify-write: IRQs off until the devices are
    // ready, translation off so 0xFA/0xAA device responses are read raw rather than
    // mangled by the scancode translation table, and both clock-disable bits clear.
    let init_config = CONFIG_SYSTEM;
    write_config(init_config);
    klog_debug!(
        "PS/2: wrote init config 0x{:02x} (IRQs off, translation off)",
        init_config
    );

    write_command(CMD_ENABLE_FIRST);
    write_command(CMD_ENABLE_AUX);
    klog_debug!("PS/2: both ports enabled");

    klog_info!("PS/2: controller initialisation complete");
}

/// Write the final config byte, enabling IRQ 1 (keyboard) and IRQ 12 (mouse).
/// Call only after both devices have been initialised.
pub fn enable_irqs() {
    let final_config = CONFIG_FIRST_IRQ | CONFIG_AUX_IRQ | CONFIG_SYSTEM | CONFIG_TRANSLATION;
    write_config(final_config);
    klog_info!("PS/2: IRQs enabled — config 0x{:02x}", final_config);
}
