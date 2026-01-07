//! Architecture-specific constants for x86_64.
//!
//! Canonical definitions for GDT selectors, interrupt vectors, and other
//! architecture-dependent values used across kernel subsystems.

#![allow(dead_code)]

// =============================================================================
// GDT Selectors (Ring 3 / User Mode)
// =============================================================================

/// User-mode code segment selector (CPL 3).
pub const GDT_USER_CODE_SELECTOR: u16 = 0x23;

/// User-mode data segment selector (CPL 3).
pub const GDT_USER_DATA_SELECTOR: u16 = 0x1B;

// =============================================================================
// Interrupt Vectors
// =============================================================================

/// Base vector for hardware IRQs (IRQ0 maps to this vector).
pub const IRQ_BASE_VECTOR: u8 = 32;

/// Syscall interrupt vector (int 0x80).
pub const SYSCALL_VECTOR: u8 = 0x80;
