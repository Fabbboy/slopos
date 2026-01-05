//! Local APIC and x2APIC hardware definitions.

// ============================================================================
// CPUID Feature Detection
// ============================================================================

/// CPUID leaf 1, EDX: APIC present flag
pub(crate) const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;
/// CPUID leaf 1, ECX: x2APIC support flag
pub(crate) const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

// ============================================================================
// MSR Addresses
// ============================================================================

/// APIC Base MSR address
pub(crate) const MSR_APIC_BASE: u32 = 0x1B;

// ============================================================================
// APIC Base Register Flags
// ============================================================================

/// Bootstrap Processor flag in APIC Base MSR
pub(crate) const APIC_BASE_BSP: u64 = 1 << 8;
/// x2APIC mode enable flag
pub(crate) const APIC_BASE_X2APIC: u64 = 1 << 10;
/// Global APIC enable flag
pub(crate) const APIC_BASE_GLOBAL_ENABLE: u64 = 1 << 11;
/// Mask to extract physical base address (bits 12-51)
pub(crate) const APIC_BASE_ADDR_MASK: u64 = 0xFFFF_F000;

// ============================================================================
// Local APIC Register Offsets
// ============================================================================

/// APIC ID register
pub(crate) const LAPIC_ID: u32 = 0x020;
/// Version register
pub(crate) const LAPIC_VERSION: u32 = 0x030;
/// End Of Interrupt register
pub(crate) const LAPIC_EOI: u32 = 0x0B0;
/// Spurious interrupt vector register
pub(crate) const LAPIC_SPURIOUS: u32 = 0x0F0;
/// Error status register
pub(crate) const LAPIC_ESR: u32 = 0x280;
/// Interrupt Command Register (low 32-bits)
pub(crate) const LAPIC_ICR_LOW: u32 = 0x300;
/// Interrupt Command Register (high 32-bits)
pub(crate) const LAPIC_ICR_HIGH: u32 = 0x310;
/// Local Vector Table: Timer
pub(crate) const LAPIC_LVT_TIMER: u32 = 0x320;
/// Local Vector Table: Performance Counter
pub(crate) const LAPIC_LVT_PERFCNT: u32 = 0x340;
/// Local Vector Table: LINT0
pub(crate) const LAPIC_LVT_LINT0: u32 = 0x350;
/// Local Vector Table: LINT1
pub(crate) const LAPIC_LVT_LINT1: u32 = 0x360;
/// Local Vector Table: Error
pub(crate) const LAPIC_LVT_ERROR: u32 = 0x370;
/// Timer Initial Count Register
pub(crate) const LAPIC_TIMER_ICR: u32 = 0x380;
/// Timer Current Count Register
pub(crate) const LAPIC_TIMER_CCR: u32 = 0x390;
/// Timer Divide Configuration Register
pub(crate) const LAPIC_TIMER_DCR: u32 = 0x3E0;

// ============================================================================
// LAPIC Control Flags
// ============================================================================

/// Enable spurious interrupt handling
pub(crate) const LAPIC_SPURIOUS_ENABLE: u32 = 1 << 8;
/// Mask flag for LVT entries
pub(crate) const LAPIC_LVT_MASKED: u32 = 1 << 16;
/// External interrupt delivery mode
pub(crate) const LAPIC_LVT_DELIVERY_MODE_EXTINT: u32 = 0x7 << 8;

// ============================================================================
// Timer Configuration
// ============================================================================

/// Periodic timer mode
pub(crate) const LAPIC_TIMER_PERIODIC: u32 = 0x0002_0000;
/// Timer divisor of 16
pub(crate) const LAPIC_TIMER_DIV_16: u32 = 0x3;

// ============================================================================
// IPI (Inter-Processor Interrupt) Command Flags
// ============================================================================

/// Fixed delivery mode
pub(crate) const LAPIC_ICR_DELIVERY_FIXED: u32 = 0 << 8;
/// Physical destination mode
pub(crate) const LAPIC_ICR_DEST_PHYSICAL: u32 = 0 << 11;
/// Assert interrupt level
pub(crate) const LAPIC_ICR_LEVEL_ASSERT: u32 = 1 << 14;
/// Edge-triggered
pub(crate) const LAPIC_ICR_TRIGGER_EDGE: u32 = 0 << 15;
/// Broadcast to all processors
pub(crate) const LAPIC_ICR_DEST_BROADCAST: u32 = 0xFF << 24;
/// Delivery status bit
pub(crate) const LAPIC_ICR_DELIVERY_STATUS: u32 = 1 << 12;
