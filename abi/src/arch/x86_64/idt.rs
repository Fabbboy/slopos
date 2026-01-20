//! Interrupt Descriptor Table (IDT) definitions.
//!
//! This module provides constants for CPU exception vectors, hardware IRQ
//! base vector, syscall vector, and IDT gate types.

// =============================================================================
// Gate Types
// =============================================================================

/// Interrupt gate type attribute (DPL=0, present, interrupt gate).
/// Clears IF on entry.
pub const IDT_GATE_INTERRUPT: u8 = 0x8E;

/// Trap gate type attribute (DPL=0, present, trap gate).
/// Does not clear IF on entry.
pub const IDT_GATE_TRAP: u8 = 0x8F;

// =============================================================================
// CPU Exception Vectors (0-31)
// =============================================================================

/// Divide Error (#DE) - vector 0.
pub const EXCEPTION_DIVIDE_ERROR: u8 = 0;

/// Debug (#DB) - vector 1.
pub const EXCEPTION_DEBUG: u8 = 1;

/// Non-Maskable Interrupt (NMI) - vector 2.
pub const EXCEPTION_NMI: u8 = 2;

/// Breakpoint (#BP) - vector 3.
pub const EXCEPTION_BREAKPOINT: u8 = 3;

/// Overflow (#OF) - vector 4.
pub const EXCEPTION_OVERFLOW: u8 = 4;

/// Bound Range Exceeded (#BR) - vector 5.
pub const EXCEPTION_BOUND_RANGE: u8 = 5;

/// Invalid Opcode (#UD) - vector 6.
pub const EXCEPTION_INVALID_OPCODE: u8 = 6;

/// Device Not Available (#NM) - vector 7.
pub const EXCEPTION_DEVICE_NOT_AVAIL: u8 = 7;

/// Double Fault (#DF) - vector 8.
pub const EXCEPTION_DOUBLE_FAULT: u8 = 8;

/// Coprocessor Segment Overrun - vector 9 (reserved).
pub const EXCEPTION_COPROCESSOR_OVERRUN: u8 = 9;

/// Invalid TSS (#TS) - vector 10.
pub const EXCEPTION_INVALID_TSS: u8 = 10;

/// Segment Not Present (#NP) - vector 11.
pub const EXCEPTION_SEGMENT_NOT_PRES: u8 = 11;

/// Stack-Segment Fault (#SS) - vector 12.
pub const EXCEPTION_STACK_FAULT: u8 = 12;

/// General Protection (#GP) - vector 13.
pub const EXCEPTION_GENERAL_PROTECTION: u8 = 13;

/// Page Fault (#PF) - vector 14.
pub const EXCEPTION_PAGE_FAULT: u8 = 14;

/// Reserved - vector 15.
pub const EXCEPTION_RESERVED_15: u8 = 15;

/// x87 FPU Floating-Point Error (#MF) - vector 16.
pub const EXCEPTION_FPU_ERROR: u8 = 16;

/// Alignment Check (#AC) - vector 17.
pub const EXCEPTION_ALIGNMENT_CHECK: u8 = 17;

/// Machine Check (#MC) - vector 18.
pub const EXCEPTION_MACHINE_CHECK: u8 = 18;

/// SIMD Floating-Point Exception (#XM/#XF) - vector 19.
pub const EXCEPTION_SIMD_FP_EXCEPTION: u8 = 19;

/// Virtualization Exception (#VE) - vector 20.
pub const EXCEPTION_VIRTUALIZATION: u8 = 20;

/// Control Protection Exception (#CP) - vector 21.
pub const EXCEPTION_CONTROL_PROTECTION: u8 = 21;

// Vectors 22-31 are reserved

// =============================================================================
// Hardware IRQ and Syscall Vectors
// =============================================================================

/// Base vector for hardware IRQs (IRQ0 maps to this vector).
/// Hardware IRQs are remapped to start at vector 32 to avoid conflicts
/// with CPU exceptions (vectors 0-31).
pub const IRQ_BASE_VECTOR: u8 = 32;

/// Syscall interrupt vector (int 0x80).
pub const SYSCALL_VECTOR: u8 = 0x80;

/// TLB shootdown IPI vector (0xFD).
/// Used for cross-CPU TLB invalidation on SMP systems.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFD;

/// Reschedule IPI vector (0xFC).
/// Used to wake a CPU from idle to run newly-queued tasks.
pub const RESCHEDULE_IPI_VECTOR: u8 = 0xFC;

// =============================================================================
// IDT Size
// =============================================================================

/// Number of entries in the IDT (256 vectors).
pub const IDT_ENTRIES: usize = 256;
