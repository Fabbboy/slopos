//! Legacy 8259 PIC (Programmable Interrupt Controller) definitions.
//!
//! SlopOS relies on APIC/IOAPIC; these are used only to mask the legacy PIC.

// ============================================================================
// PIC I/O Ports
// ============================================================================

/// Master PIC command port
pub(crate) const PIC1_COMMAND: u16 = 0x20;
/// Master PIC data port
pub(crate) const PIC1_DATA: u16 = 0x21;
/// Slave PIC command port
pub(crate) const PIC2_COMMAND: u16 = 0xA0;
/// Slave PIC data port
pub(crate) const PIC2_DATA: u16 = 0xA1;

// ============================================================================
// PIC Commands
// ============================================================================

/// End of Interrupt command
pub(crate) const PIC_EOI: u8 = 0x20;
