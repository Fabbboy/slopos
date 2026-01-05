//! I/O APIC hardware definitions.

// ============================================================================
// Size and Capacity Limits
// ============================================================================

/// Maximum number of IOAPIC controllers
pub(crate) const IOAPIC_MAX_CONTROLLERS: usize = 8;
/// Maximum Interrupt Source Override entries
pub(crate) const IOAPIC_MAX_ISO_ENTRIES: usize = 32;

// ============================================================================
// Register Offsets
// ============================================================================

/// Version register index
pub(crate) const IOAPIC_REG_VER: u8 = 0x01;
/// Redirection table base register index
pub(crate) const IOAPIC_REG_REDIR_BASE: u8 = 0x10;

// ============================================================================
// Register Masks
// ============================================================================

/// Writable bits in redirection entry low dword
pub(crate) const IOAPIC_REDIR_WRITABLE_MASK: u32 =
    (7 << 8) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16);

// ============================================================================
// ACPI MADT Entry Types
// ============================================================================

/// MADT entry type for IOAPIC controllers
pub(crate) const MADT_ENTRY_IOAPIC: u8 = 1;
/// MADT entry type for interrupt source overrides
pub(crate) const MADT_ENTRY_INTERRUPT_OVERRIDE: u8 = 2;

// ============================================================================
// ACPI MADT Polarity/Trigger Parsing
// ============================================================================

/// Extracts polarity bits [1:0]
pub(crate) const ACPI_MADT_POLARITY_MASK: u16 = 0x3;
/// Extracts trigger bits [3:2]
pub(crate) const ACPI_MADT_TRIGGER_MASK: u16 = 0xC;
/// Shift amount for trigger bits
pub(crate) const ACPI_MADT_TRIGGER_SHIFT: u16 = 2;

// ============================================================================
// Redirection Entry Flags - Delivery Mode (bits [10:8])
// ============================================================================

/// Fixed delivery mode
pub const IOAPIC_FLAG_DELIVERY_FIXED: u32 = 0u32 << 8;
/// Lowest priority delivery mode
pub const IOAPIC_FLAG_DELIVERY_LOWEST_PRI: u32 = 1u32 << 8;
/// System Management Interrupt
pub const IOAPIC_FLAG_DELIVERY_SMI: u32 = 2u32 << 8;
/// Non-Maskable Interrupt
pub const IOAPIC_FLAG_DELIVERY_NMI: u32 = 4u32 << 8;
/// INIT delivery
pub const IOAPIC_FLAG_DELIVERY_INIT: u32 = 5u32 << 8;
/// External INT delivery
pub const IOAPIC_FLAG_DELIVERY_EXTINT: u32 = 7u32 << 8;

// ============================================================================
// Redirection Entry Flags - Destination Mode (bit 11)
// ============================================================================

/// Physical destination mode
pub const IOAPIC_FLAG_DEST_PHYSICAL: u32 = 0u32 << 11;
/// Logical destination mode
pub const IOAPIC_FLAG_DEST_LOGICAL: u32 = 1u32 << 11;

// ============================================================================
// Redirection Entry Flags - Polarity (bit 13)
// ============================================================================

/// Active high polarity
pub const IOAPIC_FLAG_POLARITY_HIGH: u32 = 0u32 << 13;
/// Active low polarity
pub const IOAPIC_FLAG_POLARITY_LOW: u32 = 1u32 << 13;

// ============================================================================
// Redirection Entry Flags - Trigger Mode (bit 15)
// ============================================================================

/// Edge-triggered
pub const IOAPIC_FLAG_TRIGGER_EDGE: u32 = 0u32 << 15;
/// Level-triggered
pub const IOAPIC_FLAG_TRIGGER_LEVEL: u32 = 1u32 << 15;

// ============================================================================
// Redirection Entry Flags - Mask (bit 16)
// ============================================================================

/// Interrupt masked
pub const IOAPIC_FLAG_MASK: u32 = 1u32 << 16;
/// Interrupt unmasked
pub const IOAPIC_FLAG_UNMASKED: u32 = 0u32;
