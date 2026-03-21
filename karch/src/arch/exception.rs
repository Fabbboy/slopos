//! Exception metadata for x86_64.
//!
//! Provides exception names and classification functions that can be used
//! by both kernel and test code without depending on the boot crate.

use super::idt::{EXCEPTION_DOUBLE_FAULT, EXCEPTION_MACHINE_CHECK, EXCEPTION_NMI};

pub fn exception_is_critical(vector: u8) -> bool {
    matches!(
        vector,
        EXCEPTION_DOUBLE_FAULT | EXCEPTION_MACHINE_CHECK | EXCEPTION_NMI
    )
}

pub fn get_exception_name(vector: u8) -> &'static str {
    match vector {
        0 => "Divide Error",
        1 => "Debug",
        2 => "Non-Maskable Interrupt",
        3 => "Breakpoint",
        4 => "Overflow",
        5 => "Bound Range Exceeded",
        6 => "Invalid Opcode",
        7 => "Device Not Available",
        8 => "Double Fault",
        9 => "Coprocessor Segment Overrun",
        10 => "Invalid TSS",
        11 => "Segment Not Present",
        12 => "Stack Segment Fault",
        13 => "General Protection Fault",
        14 => "Page Fault",
        15 => "Reserved",
        16 => "x87 FPU Error",
        17 => "Alignment Check",
        18 => "Machine Check",
        19 => "SIMD Floating-Point Exception",
        20 => "Virtualization Exception",
        21 => "Control Protection Exception",
        22..=31 => "Reserved",
        _ => "Unknown",
    }
}
