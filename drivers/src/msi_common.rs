//! Shared constants and helpers for MSI and MSI-X interrupt mechanisms.
//!
//! Both MSI and MSI-X deliver interrupts by writing a message to the LAPIC
//! doorbell region at `0xFEE0_0000`.  The message address encodes the target
//! APIC ID, and the message data encodes the interrupt vector, delivery mode,
//! and trigger mode.  This module centralises the architecture-specific message
//! format and common PCI operations used by both mechanisms.
//!
//! ## x86 LAPIC message format (Intel SDM Vol. 3A §10.11)
//!
//! ```text
//! Message Address [31:0]:
//!   [31:20] = 0xFEE (fixed LAPIC doorbell prefix)
//!   [19:12] = Destination APIC ID
//!   [11:4]  = Reserved
//!   [3]     = Redirection Hint (0 = physical)
//!   [2]     = Destination Mode (0 = physical)
//!   [1:0]   = Reserved
//!
//! Message Data [31:0]:
//!   [15]    = Trigger Mode (0 = edge)
//!   [14]    = Level (ignored for edge)
//!   [13:11] = Reserved
//!   [10:8]  = Delivery Mode (000 = fixed)
//!   [7:0]   = Vector
//! ```

use crate::pci::{pci_config_read16, pci_config_write16};
use crate::pci_defs::{PCI_COMMAND_INTX_DISABLE, PCI_COMMAND_OFFSET};

// =============================================================================
// x86 LAPIC message address format
// =============================================================================

/// Fixed base address for MSI/MSI-X messages — the LAPIC doorbell region.
///
/// All MSI and MSI-X messages target this address range.  The destination
/// APIC ID is encoded in bits [19:12].
pub const LAPIC_MSG_ADDR_BASE: u32 = 0xFEE0_0000;

/// Bit position of the destination APIC ID in the message address.
pub const LAPIC_MSG_ADDR_DEST_SHIFT: u32 = 12;

// =============================================================================
// x86 LAPIC message data format
// =============================================================================

/// Delivery mode: Fixed (000b in bits [10:8] of message data).
pub const LAPIC_MSG_DATA_DELIVERY_FIXED: u32 = 0b000 << 8;

/// Trigger mode: Edge (0 in bit 15 of message data).
pub const LAPIC_MSG_DATA_TRIGGER_EDGE: u32 = 0 << 15;

// =============================================================================
// Vector validation
// =============================================================================

/// Minimum valid interrupt vector.  Vectors 0–31 are reserved for CPU
/// exceptions on x86 and must never be used for MSI/MSI-X delivery.
pub const MIN_INTERRUPT_VECTOR: u8 = 32;

/// Returns `true` if `vector` is a valid MSI/MSI-X interrupt vector (≥ 32).
#[inline]
pub const fn is_valid_vector(vector: u8) -> bool {
    vector >= MIN_INTERRUPT_VECTOR
}

// =============================================================================
// Message construction
// =============================================================================

/// Build the LAPIC message address for a given destination APIC ID.
///
/// The address targets the LAPIC doorbell in physical destination mode.
#[inline]
pub const fn lapic_msg_addr(target_apic_id: u8) -> u32 {
    LAPIC_MSG_ADDR_BASE | ((target_apic_id as u32) << LAPIC_MSG_ADDR_DEST_SHIFT)
}

/// Build the LAPIC message data for a given interrupt vector.
///
/// Uses fixed delivery mode and edge triggering.  Returns `u32` — callers
/// targeting 16-bit MSI data registers should truncate with `as u16`
/// (the significant bits fit within the low 16).
#[inline]
pub const fn lapic_msg_data(vector: u8) -> u32 {
    (vector as u32) | LAPIC_MSG_DATA_DELIVERY_FIXED | LAPIC_MSG_DATA_TRIGGER_EDGE
}

// =============================================================================
// Legacy INTx management
// =============================================================================

/// Disable legacy INTx assertion in the PCI Command register.
///
/// MSI and MSI-X must not coexist with legacy INTx interrupts.  Call this
/// when enabling either mechanism.
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

/// Re-enable legacy INTx assertion in the PCI Command register.
///
/// Call when disabling MSI/MSI-X to restore the legacy interrupt path.
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
