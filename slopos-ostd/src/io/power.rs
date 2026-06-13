//! Safe wrappers around the platform power-off / reset / serial-drain
//! port writes used by the kernel's shutdown / reboot paths.
//!
//! Each function encapsulates the exact port-write sequence the
//! corresponding firmware contract documents, so consumers don't
//! re-derive the magic value at every call site and don't spell
//! `unsafe { … }` around `IoPort::write`. The IO ports themselves are
//! reserved through [`IoPortRegistry::reserve`]; an unreserved port
//! panics with a descriptive `.expect()` message because shutdown
//! happens after every dynamic configuration step has run.

use super::port::IoPortRegistry;

/// Quiesce the COM1 transmitter: poll the LSR (Line Status Register)
/// until the "transmitter empty" bit (0x40) latches or the bounded
/// retry loop expires. Returns once the FIFO drains.
///
/// The retry budget covers up to ~1024 polls; on QEMU this completes
/// in a single iteration. The implementation tolerates a stuck LSR
/// (returns after `max_retries` regardless) — shutdown must not hang
/// on a wedged UART.
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

/// Drive every known ACPI PM1A_CNT power-off port with the suspend-
/// state magic. Each hypervisor / firmware reads one of these; firing
/// all three is the documented "best-effort poweroff" sequence.
/// Returns once all three writes have been issued.
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

/// Pulse the PS/2 controller command port with the CPU-reset opcode
/// (0xFE). This is the legacy keyboard-controller reset path: a
/// successful write triggers a chipset assertion of the CPU's RESET#
/// line. Returns immediately if the platform did not reset (callers
/// fall through to a triple-fault).
pub fn ps2_reset_pulse() {
    let ps2_cmd = IoPortRegistry::reserve::<u8>(0x64).expect("PS/2 command port");
    // SAFETY: 0x64 is registered as the PS/2 controller command port;
    // writing 0xFE is the documented "pulse output lines" command
    // that the chipset wires to RESET#. The unsafe write is
    // centralised here.
    unsafe { ps2_cmd.write(0xFE) };
}

/// PCH reset-control register (`RST_CNT`): the modern x86 reset, wired to
/// the platform `RESET#`. The reset is edge-triggered by raising `RST_CPU`
/// after arming, so it is driven as a two-step arm-then-fire with a settle
/// in between (Linux's `BOOT_CF9` sequence).
mod rst_cnt {
    pub const PORT: u16 = 0xCF9;
    pub const RST_CPU: u8 = 1 << 1;
    pub const SYS_RST: u8 = 1 << 2; // 1 = hard reset
}

/// Reset the platform via the PCH `0xCF9` register — the path for USB-only
/// or UEFI machines where [`ps2_reset_pulse`] is a no-op and a triple fault
/// is ignored. Returns only if the platform did not reset. `settle` is run
/// between arm and fire (≈50 µs suffices).
pub fn cf9_reset_pulse(settle: impl Fn()) {
    use rst_cnt::*;
    let Ok(reg) = IoPortRegistry::reserve::<u8>(PORT) else {
        return;
    };
    // SAFETY: registered PCH reset-control register; the read is
    // side-effect-free and the two writes are its documented reset edge.
    unsafe {
        let base = reg.read() & !(RST_CPU | SYS_RST);
        reg.write(base | RST_CPU);
        settle();
        reg.write(base | RST_CPU | SYS_RST);
    }
}
