//! Local APIC and x2APIC definitions.
//!
//! This module provides type-safe definitions for the Local APIC hardware,
//! including MSR values, register offsets, and control flags.

// =============================================================================
// APIC Base MSR
// =============================================================================

/// IA32_APIC_BASE MSR value (MSR 0x1B).
///
/// Layout:
/// - Bits 0-7: Reserved
/// - Bit 8: BSP flag (1 = bootstrap processor)
/// - Bit 9: Reserved
/// - Bit 10: x2APIC enable
/// - Bit 11: APIC global enable
/// - Bits 12-51: APIC base physical address (4KB aligned)
/// - Bits 52-63: Reserved
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct ApicBaseMsr(pub u64);

impl ApicBaseMsr {
    /// Mask for extracting the APIC physical base address (bits 12-51).
    pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    /// Bootstrap processor flag (bit 8).
    pub const BSP: u64 = 1 << 8;

    /// x2APIC mode enable (bit 10).
    pub const X2APIC_ENABLE: u64 = 1 << 10;

    /// APIC global enable (bit 11).
    pub const GLOBAL_ENABLE: u64 = 1 << 11;

    /// Extract the physical base address of the APIC registers.
    #[inline]
    pub const fn address(self) -> u64 {
        self.0 & Self::ADDR_MASK
    }

    /// Check if this is the bootstrap processor.
    #[inline]
    pub const fn is_bsp(self) -> bool {
        self.0 & Self::BSP != 0
    }

    /// Check if x2APIC mode is enabled.
    #[inline]
    pub const fn is_x2apic(self) -> bool {
        self.0 & Self::X2APIC_ENABLE != 0
    }

    /// Check if the APIC is globally enabled.
    #[inline]
    pub const fn is_enabled(self) -> bool {
        self.0 & Self::GLOBAL_ENABLE != 0
    }

    /// Create a new MSR value with the given base address and flags.
    #[inline]
    pub const fn new(base: u64, bsp: bool, x2apic: bool, enable: bool) -> Self {
        let mut val = base & Self::ADDR_MASK;
        if bsp {
            val |= Self::BSP;
        }
        if x2apic {
            val |= Self::X2APIC_ENABLE;
        }
        if enable {
            val |= Self::GLOBAL_ENABLE;
        }
        Self(val)
    }
}

// =============================================================================
// APIC Base Register Flags (for backward compatibility)
// =============================================================================

/// Bootstrap Processor flag in APIC Base MSR.
pub const APIC_BASE_BSP: u64 = ApicBaseMsr::BSP;

/// x2APIC mode enable flag.
pub const APIC_BASE_X2APIC: u64 = ApicBaseMsr::X2APIC_ENABLE;

/// Global APIC enable flag.
pub const APIC_BASE_GLOBAL_ENABLE: u64 = ApicBaseMsr::GLOBAL_ENABLE;

/// Mask to extract physical base address (bits 12-51).
pub const APIC_BASE_ADDR_MASK: u64 = ApicBaseMsr::ADDR_MASK;

// =============================================================================
// MSR Address
// =============================================================================

/// APIC Base MSR address as raw u32 (for backward compatibility).
pub const MSR_APIC_BASE: u32 = 0x1B;

// =============================================================================
// Local APIC Register Offsets
// =============================================================================

/// APIC ID register offset.
pub const LAPIC_ID: u32 = 0x020;

/// Version register offset.
pub const LAPIC_VERSION: u32 = 0x030;

/// End Of Interrupt register offset.
pub const LAPIC_EOI: u32 = 0x0B0;

/// Spurious interrupt vector register offset.
pub const LAPIC_SPURIOUS: u32 = 0x0F0;

/// Error status register offset.
pub const LAPIC_ESR: u32 = 0x280;

/// Interrupt Command Register (low 32-bits) offset.
pub const LAPIC_ICR_LOW: u32 = 0x300;

/// Interrupt Command Register (high 32-bits) offset.
pub const LAPIC_ICR_HIGH: u32 = 0x310;

/// Local Vector Table: Timer offset.
pub const LAPIC_LVT_TIMER: u32 = 0x320;

/// Local Vector Table: Performance Counter offset.
pub const LAPIC_LVT_PERFCNT: u32 = 0x340;

/// Local Vector Table: LINT0 offset.
pub const LAPIC_LVT_LINT0: u32 = 0x350;

/// Local Vector Table: LINT1 offset.
pub const LAPIC_LVT_LINT1: u32 = 0x360;

/// Local Vector Table: Error offset.
pub const LAPIC_LVT_ERROR: u32 = 0x370;

/// Timer Initial Count Register offset.
pub const LAPIC_TIMER_ICR: u32 = 0x380;

/// Timer Current Count Register offset.
pub const LAPIC_TIMER_CCR: u32 = 0x390;

/// Timer Divide Configuration Register offset.
pub const LAPIC_TIMER_DCR: u32 = 0x3E0;

// =============================================================================
// LAPIC Control Flags
// =============================================================================

/// Enable spurious interrupt handling (bit 8 of spurious register).
pub const LAPIC_SPURIOUS_ENABLE: u32 = 1 << 8;

/// Mask flag for LVT entries (bit 16).
pub const LAPIC_LVT_MASKED: u32 = 1 << 16;

/// External interrupt delivery mode (bits 8-10 = 111).
pub const LAPIC_LVT_DELIVERY_MODE_EXTINT: u32 = 0x7 << 8;

// =============================================================================
// Timer Configuration
// =============================================================================

/// Periodic timer mode (bit 17).
pub const LAPIC_TIMER_PERIODIC: u32 = 0x0002_0000;

/// Timer divisor of 16 (DCR value).
pub const LAPIC_TIMER_DIV_16: u32 = 0x3;

// =============================================================================
// IPI (Inter-Processor Interrupt) Command Flags
// =============================================================================

/// Fixed delivery mode (bits 8-10 = 000).
pub const LAPIC_ICR_DELIVERY_FIXED: u32 = 0 << 8;

/// Physical destination mode (bit 11 = 0).
pub const LAPIC_ICR_DEST_PHYSICAL: u32 = 0 << 11;

/// Assert interrupt level (bit 14 = 1).
pub const LAPIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;

/// Edge-triggered (bit 15 = 0).
pub const LAPIC_ICR_TRIGGER_EDGE: u32 = 0 << 15;

/// Broadcast to all processors (bits 24-31 = 0xFF).
pub const LAPIC_ICR_DEST_BROADCAST: u32 = 0xFF << 24;

/// Delivery status bit (bit 12).
pub const LAPIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
