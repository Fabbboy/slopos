//! Architecture-specific definitions.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

// Re-export x86_64 types at arch level for convenience
#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

// =============================================================================
// Backward Compatibility Re-exports
// =============================================================================
// These constants were previously defined in the flat arch.rs file.
// They are preserved here for backward compatibility with existing code.

/// User-mode code segment selector (CPL 3).
///
/// This is a backward-compatibility alias. Prefer `SegmentSelector::USER_CODE`.
#[cfg(target_arch = "x86_64")]
pub const GDT_USER_CODE_SELECTOR: u16 = 0x23;

/// User-mode data segment selector (CPL 3).
///
/// This is a backward-compatibility alias. Prefer `SegmentSelector::USER_DATA`.
#[cfg(target_arch = "x86_64")]
pub const GDT_USER_DATA_SELECTOR: u16 = 0x1B;

/// Base vector for hardware IRQs (IRQ0 maps to this vector).
#[cfg(target_arch = "x86_64")]
pub use x86_64::idt::IRQ_BASE_VECTOR;

/// Syscall interrupt vector (int 0x80).
#[cfg(target_arch = "x86_64")]
pub use x86_64::idt::SYSCALL_VECTOR;
