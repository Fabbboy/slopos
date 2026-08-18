//! MSI/MSI-X message construction: both mechanisms deliver an interrupt by
//! writing to the LAPIC doorbell at `0xFEE0_0000` (Intel SDM Vol. 3A §10.11).

use crate::pci::{pci_config_read16, pci_config_write16};
use crate::pci_defs::{PCI_COMMAND_INTX_DISABLE, PCI_COMMAND_OFFSET};

/// LAPIC doorbell base; destination APIC ID occupies message-address bits [19:12].
pub const LAPIC_MSG_ADDR_BASE: u32 = 0xFEE0_0000;

pub const LAPIC_MSG_ADDR_DEST_SHIFT: u32 = 12;

/// Fixed delivery mode: 000b in message-data bits [10:8].
pub const LAPIC_MSG_DATA_DELIVERY_FIXED: u32 = 0b000 << 8;

pub const LAPIC_MSG_DATA_TRIGGER_EDGE: u32 = 0 << 15;

/// Vectors 0–31 are reserved for x86 CPU exceptions and are never deliverable.
pub const MIN_INTERRUPT_VECTOR: u8 = 32;

#[inline]
pub const fn is_valid_vector(vector: u8) -> bool {
    vector >= MIN_INTERRUPT_VECTOR
}

/// Message address for `target_apic_id`, in physical destination mode.
#[inline]
pub const fn lapic_msg_addr(target_apic_id: u8) -> u32 {
    LAPIC_MSG_ADDR_BASE | ((target_apic_id as u32) << LAPIC_MSG_ADDR_DEST_SHIFT)
}

/// Message data for `vector`: fixed delivery, edge triggered. Callers targeting
/// a 16-bit MSI data register may truncate — the significant bits fit in the low 16.
#[inline]
pub const fn lapic_msg_data(vector: u8) -> u32 {
    (vector as u32) | LAPIC_MSG_DATA_DELIVERY_FIXED | LAPIC_MSG_DATA_TRIGGER_EDGE
}

/// MSI and MSI-X must not coexist with legacy INTx; call when enabling either.
#[inline]
pub fn disable_intx(bus: u8, dev: u8, func: u8) {
    let cmd = pci_config_read16(bus, dev, func, PCI_COMMAND_OFFSET);
    pci_config_write16(
        bus,
        dev,
        func,
        PCI_COMMAND_OFFSET,
        cmd | PCI_COMMAND_INTX_DISABLE,
    );
}

/// Restore the legacy interrupt path when disabling MSI/MSI-X.
#[inline]
pub fn restore_intx(bus: u8, dev: u8, func: u8) {
    let cmd = pci_config_read16(bus, dev, func, PCI_COMMAND_OFFSET);
    pci_config_write16(
        bus,
        dev,
        func,
        PCI_COMMAND_OFFSET,
        cmd & !PCI_COMMAND_INTX_DISABLE,
    );
}
