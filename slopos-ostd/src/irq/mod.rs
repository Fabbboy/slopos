//! Interrupt machinery: vector allocation, callback dispatch, IDT
//! construction, and IRET-corruption recovery.
//!
//! # No bottom halves
//!
//! SlopOS deliberately does not ship the softirq / tasklet / work-queue
//! family of bottom-half mechanisms. Drivers that need to defer work
//! out of an IRQ-context callback spawn an ordinary `Task` and signal
//! it from inside the handler; deferral semantics are uniform with the
//! rest of the kernel's task model. This keeps the trusted core
//! smaller (one scheduling primitive instead of three) and matches
//! the Asterinas framekernel design.

pub mod idt;
pub mod interrupt_frame;
pub mod line;

pub use idt::{
    DiagnosticSink, EXCEPTION_ALIGNMENT_CHECK, EXCEPTION_BOUND_RANGE, EXCEPTION_BREAKPOINT,
    EXCEPTION_CONTROL_PROTECTION, EXCEPTION_COPROCESSOR_OVERRUN, EXCEPTION_DEBUG,
    EXCEPTION_DEVICE_NOT_AVAIL, EXCEPTION_DIVIDE_ERROR, EXCEPTION_DOUBLE_FAULT,
    EXCEPTION_FPU_ERROR, EXCEPTION_GENERAL_PROTECTION, EXCEPTION_INVALID_OPCODE,
    EXCEPTION_INVALID_TSS, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI, EXCEPTION_OVERFLOW,
    EXCEPTION_PAGE_FAULT, EXCEPTION_RESERVED_15, EXCEPTION_SEGMENT_NOT_PRES,
    EXCEPTION_SIMD_FP_EXCEPTION, EXCEPTION_STACK_FAULT, EXCEPTION_VIRTUALIZATION, ExceptionMode,
    IDT_ENTRIES, IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IRQ_BASE_VECTOR, IdtBuilder, IdtEntry,
    IrqEntryGuard, IstPreemptHold, LAPIC_TIMER_VECTOR, LUF_DRAIN_IPI_VECTOR, MSI_VECTOR_BASE,
    MSI_VECTOR_COUNT, MSI_VECTOR_END, RCU_QS_IPI_VECTOR, RESCHEDULE_IPI_VECTOR, SYSCALL_VECTOR,
    TLB_SHOOTDOWN_VECTOR, handle_corrupt_iret_frame, register_diagnostic_sink, vector_uses_ist,
};
pub use interrupt_frame::InterruptFrame;
pub use line::{
    ALLOC_VECTOR_BASE, ALLOC_VECTOR_END, CallbackHandle, IrqAllocator, IrqContext, IrqError,
    IrqLine, OwnedIrq, dispatch, register_irq_reserved, shutdown,
};
