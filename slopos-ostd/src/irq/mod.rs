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
pub mod line;

pub use idt::{
    DiagnosticSink, ExceptionMode, IDT_ENTRIES, IDT_GATE_INTERRUPT, IDT_GATE_TRAP, IdtBuilder,
    IdtEntry, IrqEntryGuard, IstPreemptHold, handle_corrupt_iret_frame, register_diagnostic_sink,
    vector_uses_ist,
};
pub use line::{
    ALLOC_VECTOR_BASE, ALLOC_VECTOR_END, CallbackHandle, IrqAllocator, IrqContext, IrqError,
    IrqLine, dispatch, register_irq_reserved, shutdown,
};
