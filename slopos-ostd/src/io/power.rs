//! Safe wrappers around the platform power-off / reset / serial-drain port
//! writes used by the kernel's shutdown / reboot paths.
//!
//! Ports are reserved through [`IoPortRegistry::reserve`]; an unreserved one
//! panics, because shutdown happens after every dynamic configuration step has
//! run.

use super::port::IoPortRegistry;
use super::raw_port::Port;

/// PM1 control `SLP_EN`: commits the sleep transition.
const PM1_SLP_EN: u16 = 1 << 13;
/// PM1 control `SCI_EN`: set once the platform is in ACPI mode.
const PM1_SCI_EN: u16 = 1 << 0;
/// ACPI GAS `AddressSpaceId` for System I/O.
const ACPI_ADDR_SPACE_IO: u8 = 1;

/// Quiesce the COM1 transmitter: poll the LSR until "transmitter empty"
/// (0x40) latches, or `max_retries` expires — shutdown must not hang on a
/// wedged UART.
pub fn drain_serial_tx(pause: impl Fn(), max_retries: u32) {
    let lsr_port = IoPortRegistry::reserve::<u8>(0x3F8 + 5).expect("COM1 LSR port");
    for _ in 0..max_retries {
        // SAFETY: COM1 LSR is registered as insensitive (read-only
        // status register, no side effects). The unsafe `IoPort::read`
        // is centralised here.
        let lsr = unsafe { lsr_port.read() };
        if (lsr & 0x40) != 0 {
            return;
        }
        pause();
    }
}

/// Drive every known ACPI PM1A_CNT power-off port with the suspend-state
/// magic: each hypervisor / firmware reads only one of them.
pub fn acpi_poweroff_broadcast() {
    let acpi = IoPortRegistry::reserve::<u16>(0x604).expect("ACPI PM1A_CNT port");
    let acpi_bochs = IoPortRegistry::reserve::<u16>(0xB004).expect("Bochs ACPI PM1A_CNT port");
    let acpi_vbox = IoPortRegistry::reserve::<u16>(0x4004).expect("VBox ACPI PM1A_CNT port");
    // SAFETY: each port is a registered ACPI PM1A_CNT slot whose
    // write semantics are "request soft-off"; the three are documented
    // as the trio every modern hypervisor honours. The unsafe writes
    // are centralised here.
    unsafe {
        acpi.write(0x2000);
        acpi_bochs.write(0x2000);
        acpi_vbox.write(0x3400);
    }
}

/// Request an ACPI S5 (soft-off) transition through the firmware-described
/// PM1 control registers.
///
/// `pm1a_port` / `pm1b_port` are the FADT PM1a / PM1b control I/O ports
/// (`pm1b_port == 0` on single-block systems); `slp_typ_a` / `slp_typ_b` are
/// the sleep-type values decoded from the DSDT `\_S5` package. Returns if no
/// write was issued or the platform ignored it, so callers can fall through.
pub fn acpi_s5_poweroff(pm1a_port: u16, pm1b_port: u16, slp_typ_a: u8, slp_typ_b: u8) {
    if pm1a_port != 0 {
        let value = (((slp_typ_a & 0x7) as u16) << 10) | PM1_SLP_EN;
        // SAFETY: `pm1a_port` is the firmware-advertised ACPI PM1a
        // control register (FADT PM1a_CNT_BLK). Writing SLP_TYP|SLP_EN
        // is the ACPI-defined soft-off request for that register.
        unsafe { Port::<u16>::new(pm1a_port).write(value) };
    }
    if pm1b_port != 0 {
        let value = (((slp_typ_b & 0x7) as u16) << 10) | PM1_SLP_EN;
        // SAFETY: `pm1b_port` is the firmware-advertised ACPI PM1b
        // control register (FADT PM1b_CNT_BLK); same soft-off contract.
        unsafe { Port::<u16>::new(pm1b_port).write(value) };
    }
}

/// Switch the platform into ACPI mode by writing `acpi_enable` to the FADT SMI
/// command port and polling PM1a control until `SCI_EN` latches.
///
/// No-op when already in ACPI mode, when the FADT advertises no SMI command, or
/// when the addresses are out of I/O range. The poll is bounded so a wedged SMI
/// cannot hang shutdown.
pub fn acpi_enable_if_needed(pm1a_port: u16, smi_cmd: u32, acpi_enable: u8, settle: impl Fn()) {
    if pm1a_port == 0 || smi_cmd == 0 || acpi_enable == 0 || smi_cmd > u16::MAX as u32 {
        return;
    }
    let pm1a = Port::<u16>::new(pm1a_port);
    // SAFETY: ACPI PM1a control register; the read is side-effect-free.
    if unsafe { pm1a.read() } & PM1_SCI_EN != 0 {
        return;
    }
    // SAFETY: `smi_cmd` is the FADT-advertised SMI command port and
    // `acpi_enable` its FADT-defined "enter ACPI mode" value; the write
    // raises the firmware SMI that flips the platform into ACPI mode.
    unsafe { Port::<u8>::new(smi_cmd as u16).write(acpi_enable) };
    for _ in 0..0x10000u32 {
        // SAFETY: ACPI PM1a control register; side-effect-free read.
        if unsafe { pm1a.read() } & PM1_SCI_EN != 0 {
            return;
        }
        settle();
    }
}

/// Reset the platform through the FADT reset register.
///
/// Only System-I/O-space reset registers are issued; other address spaces are
/// ignored so the caller falls through to its next reset method.
pub fn acpi_reset(address_space_id: u8, address: u64, value: u8) {
    if address_space_id != ACPI_ADDR_SPACE_IO || address == 0 || address > u16::MAX as u64 {
        return;
    }
    // SAFETY: `address` is the firmware-advertised ACPI reset register
    // (FADT RESET_REG) in I/O space; `value` is its FADT RESET_VALUE,
    // the byte the platform decodes as a hardware reset trigger.
    unsafe { Port::<u8>::new(address as u16).write(value) };
}

/// Pulse the PS/2 controller command port with the CPU-reset opcode (0xFE):
/// the chipset asserts the CPU's RESET# line. Returns if the platform did not
/// reset, so callers fall through to a triple-fault.
pub fn ps2_reset_pulse() {
    let ps2_cmd = IoPortRegistry::reserve::<u8>(0x64).expect("PS/2 command port");
    // SAFETY: 0x64 is registered as the PS/2 controller command port;
    // writing 0xFE is the documented "pulse output lines" command
    // that the chipset wires to RESET#. The unsafe write is
    // centralised here.
    unsafe { ps2_cmd.write(0xFE) };
}

/// PCH reset-control register (`RST_CNT`, port `0xCF9`), wired to the platform
/// `RESET#`. The reset fires on the 0→1 edge of `RST_CPU` after `SYS_RST` is
/// armed; `FULL_RST` upgrades the warm host reset to a cold power cycle.
mod rst_cnt {
    pub const PORT: u16 = 0xCF9;
    pub const SYS_RST: u8 = 1 << 1;
    pub const RST_CPU: u8 = 1 << 2;
    pub const FULL_RST: u8 = 1 << 3;
    /// Warm (system) reset: assert RESET# without a power cycle.
    pub const WARM: u8 = SYS_RST | RST_CPU;
    /// Cold (full) reset: PCH also drives the S3/S4/S5 lines.
    pub const COLD: u8 = FULL_RST | SYS_RST | RST_CPU;
}

/// Reset the platform via the PCH `0xCF9` register: warm sequence first, then
/// cold, since some PCH soft-straps ignore or promote the warm pulse. `settle`
/// runs between arm and fire (≈50 µs suffices).
pub fn cf9_reset_pulse(settle: impl Fn()) {
    use rst_cnt::*;
    let Ok(reg) = IoPortRegistry::reserve::<u8>(PORT) else {
        return;
    };
    // SAFETY: registered PCH reset-control register; the read is
    // side-effect-free and each arm/fire pair is its documented reset
    // edge (warm then cold).
    unsafe {
        for code in [WARM, COLD] {
            let base = reg.read() & !code;
            reg.write(base | SYS_RST);
            settle();
            reg.write(base | code);
            settle();
        }
    }
}
